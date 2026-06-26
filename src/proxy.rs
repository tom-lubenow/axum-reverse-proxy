use axum::body::Body;
use http::Uri;
use http::uri::Builder as UriBuilder;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::convert::Infallible;
use tracing::trace;

use crate::forward::{ProxyConnector, create_http_connector, forward_request};

/// A reverse proxy that forwards HTTP requests to an upstream server.
///
/// The `ReverseProxy` struct handles the forwarding of HTTP requests from a specified path
/// to a target upstream server. It manages its own HTTP client with configurable settings
/// for connection pooling, timeouts, and retries.
#[derive(Clone)]
pub struct ReverseProxy<C: Connect + Clone + Send + Sync + 'static> {
    path: String,
    target: String,
    client: Client<C, Body>,
    policy: ProxyPolicy,
}

/// Behavioural knobs applied to every request a [`ReverseProxy`] forwards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyPolicy {
    /// How the upstream `Host` header is derived. See [`HostBehaviour`].
    pub host_behaviour: HostBehaviour,
}

impl ProxyPolicy {
    /// Whether the client's original `Host` header is forwarded upstream rather
    /// than replaced with the upstream authority
    pub(crate) fn forwards_client_host(&self) -> bool {
        matches!(self.host_behaviour, HostBehaviour::Preserve)
    }

    /// Resolve the single `Host` header value to send upstream, given the
    /// client's request headers and the upstream authority to fall back to.
    ///
    /// Under [`HostBehaviour::Preserve`] this is the client's `Host`, falling
    /// back to `upstream_authority` when the client sent none (e.g. an HTTP/2
    /// client whose `:authority` is not surfaced as a `Host` header). Otherwise
    /// it is always `upstream_authority`.
    pub(crate) fn forwarded_host(
        &self,
        client_headers: &http::HeaderMap,
        upstream_authority: String,
    ) -> String {
        if self.forwards_client_host() {
            client_headers
                .get(http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .unwrap_or(upstream_authority)
        } else {
            upstream_authority
        }
    }
}

/// Controls the value of the `Host` header sent to the upstream server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostBehaviour {
    /// Forward the client's original `Host` header unchanged.
    Preserve,
    /// Replace `Host` with the upstream target's authority (the default, and
    /// the behaviour of every prior release).
    #[default]
    Replace,
}

pub type StandardReverseProxy = ReverseProxy<ProxyConnector>;

impl StandardReverseProxy {
    /// Creates a new `ReverseProxy` instance.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against (e.g., "/api")
    /// * `target` - The upstream server URL to forward requests to (e.g., "https://api.example.com")
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::ReverseProxy;
    ///
    /// let proxy = ReverseProxy::new("/api", "https://api.example.com");
    /// ```
    pub fn new<S>(path: S, target: S) -> Self
    where
        S: Into<String>,
    {
        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(create_http_connector());

        Self::new_with_client(path, target, client)
    }
}

impl<C: Connect + Clone + Send + Sync + 'static> ReverseProxy<C> {
    /// Creates a new `ReverseProxy` instance with a custom HTTP client.
    ///
    /// This method allows for more fine-grained control over the proxy behavior by accepting
    /// a pre-configured HTTP client.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against
    /// * `target` - The upstream server URL to forward requests to
    /// * `client` - A custom-configured HTTP client
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::ReverseProxy;
    /// use hyper_util::client::legacy::{Client, connect::HttpConnector};
    /// use axum::body::Body;
    /// use hyper_util::rt::TokioExecutor;
    ///
    /// let client = Client::builder(TokioExecutor::new())
    ///     .pool_idle_timeout(std::time::Duration::from_secs(120))
    ///     .build(HttpConnector::new());
    ///
    /// let proxy = ReverseProxy::new_with_client(
    ///     "/api",
    ///     "https://api.example.com",
    ///     client,
    /// );
    /// ```
    pub fn new_with_client<S>(path: S, target: S, client: Client<C, Body>) -> Self
    where
        S: Into<String>,
    {
        Self {
            path: path.into(),
            target: target.into(),
            client,
            policy: ProxyPolicy::default(),
        }
    }

    /// Apply a [`ProxyPolicy`] to this proxy, overriding the default behaviour.
    #[must_use]
    pub fn with_policy(mut self, policy: ProxyPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Get the base path this proxy is configured to handle
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the target URL this proxy forwards requests to
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Handles the proxying of a single request to the upstream server.
    pub async fn proxy_request(
        &self,
        req: axum::http::Request<Body>,
    ) -> Result<axum::http::Response<Body>, Infallible> {
        self.handle_request(req).await
    }

    /// Core proxy logic used by the [`tower::Service`] implementation.
    async fn handle_request(
        &self,
        req: axum::http::Request<Body>,
    ) -> Result<axum::http::Response<Body>, Infallible> {
        trace!("Proxying request method={} uri={}", req.method(), req.uri());

        // Transform the URI to the upstream target
        let path_q = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("");
        let upstream_uri = self.transform_uri(path_q);

        // Use shared forwarding logic
        forward_request(upstream_uri, req, &self.client, &self.policy).await
    }

    /// Transform an incoming request path+query into the target URI using http::Uri builder
    ///
    /// Rules:
    /// - Trim target trailing slash for joining
    /// - Strip proxy base path at a boundary (exact or followed by '/')
    /// - If remainder is exactly '/' under a non-empty base, treat as empty
    /// - Do not add a slash for query-only joins (avoid target '/?')
    fn transform_uri(&self, path_and_query: &str) -> Uri {
        let base_path = self.path.trim_end_matches('/');

        // Parse target URI
        let target_uri: Uri = self
            .target
            .parse()
            .expect("ReverseProxy target must be a valid URI");

        let scheme = target_uri.scheme_str().unwrap_or("http");
        let authority = target_uri
            .authority()
            .expect("ReverseProxy target must include authority (host)")
            .as_str()
            .to_string();

        // Check if target originally had a trailing slash
        let target_has_trailing_slash =
            target_uri.path().ends_with('/') && target_uri.path() != "/";

        // Normalize target base path: drop trailing slash and treat "/" as empty
        let target_base_path = {
            let p = target_uri.path();
            if p == "/" {
                ""
            } else {
                p.trim_end_matches('/')
            }
        };

        // Split incoming path and query
        let (path_part, query_part) = match path_and_query.find('?') {
            Some(i) => (&path_and_query[..i], Some(&path_and_query[i + 1..])),
            None => (path_and_query, None),
        };

        // Compute remainder after stripping base when applicable
        let remaining_path = if path_part == "/" && !self.path.is_empty() {
            ""
        } else if !base_path.is_empty() && path_part.starts_with(base_path) {
            let rem = &path_part[base_path.len()..];
            if rem.is_empty() || rem.starts_with('/') {
                rem
            } else {
                path_part
            }
        } else {
            path_part
        };

        // Join target base path with remainder
        let joined_path = if remaining_path.is_empty() {
            if target_base_path.is_empty() {
                "/"
            } else if target_has_trailing_slash {
                // Preserve trailing slash from target when no remaining path
                "__TRAILING__"
            } else {
                target_base_path
            }
        } else {
            // remaining_path starts with '/'; concatenate without duplicating slash
            if target_base_path.is_empty() {
                remaining_path
            } else {
                // allocate a small string to join
                // SAFETY: both parts are valid path slices
                // Build into a String for path_and_query
                // We will rebuild below
                // Placeholder; real joining below
                "__JOIN__"
            }
        };

        // Build final path_and_query string explicitly to keep exact bytes
        let final_path = if joined_path == "__JOIN__" {
            let mut s = String::with_capacity(target_base_path.len() + remaining_path.len());
            s.push_str(target_base_path);
            s.push_str(remaining_path);
            s
        } else if joined_path == "__TRAILING__" {
            let mut s = String::with_capacity(target_base_path.len() + 1);
            s.push_str(target_base_path);
            s.push('/');
            s
        } else {
            joined_path.to_string()
        };

        let mut path_and_query_buf = final_path;
        if let Some(q) = query_part {
            path_and_query_buf.push('?');
            path_and_query_buf.push_str(q);
        }

        // Build the full URI
        UriBuilder::new()
            .scheme(scheme)
            .authority(authority.as_str())
            .path_and_query(path_and_query_buf.as_str())
            .build()
            .expect("Failed to build upstream URI")
    }
}

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;

impl<C> Service<axum::http::Request<Body>> for ReverseProxy<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    type Response = axum::http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.handle_request(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::{HostBehaviour, ProxyPolicy, StandardReverseProxy as ReverseProxy};

    fn headers_with_host(host: Option<&str>) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        if let Some(host) = host {
            headers.insert(http::header::HOST, host.parse().unwrap());
        }
        headers
    }

    #[test]
    fn upstream_host_with_replace_policy_uses_upstream_authority() {
        let policy = ProxyPolicy::default();
        let headers = headers_with_host(Some("client.example.com"));
        assert_eq!(
            policy.forwarded_host(&headers, "upstream:8080".to_string()),
            "upstream:8080"
        );
    }

    #[test]
    fn upstream_host_with_preserve_policy_uses_client_host() {
        let policy = ProxyPolicy {
            host_behaviour: HostBehaviour::Preserve,
        };
        let headers = headers_with_host(Some("client.example.com"));
        assert_eq!(
            policy.forwarded_host(&headers, "upstream:8080".to_string()),
            "client.example.com"
        );
    }

    #[test]
    fn upstream_host_with_preserve_policy_and_no_client_host_falls_back_to_authority() {
        let policy = ProxyPolicy {
            host_behaviour: HostBehaviour::Preserve,
        };
        let headers = headers_with_host(None);
        assert_eq!(
            policy.forwarded_host(&headers, "upstream:8080".to_string()),
            "upstream:8080"
        );
    }

    #[test]
    fn transform_uri_with_and_without_trailing_slash() {
        let proxy = ReverseProxy::new("/api/", "http://target");
        assert_eq!(proxy.transform_uri("/api/test"), "http://target/test");

        let proxy_no_slash = ReverseProxy::new("/api", "http://target");
        assert_eq!(
            proxy_no_slash.transform_uri("/api/test"),
            "http://target/test"
        );
    }

    #[test]
    fn transform_uri_root() {
        let proxy = ReverseProxy::new("/", "http://target");
        assert_eq!(proxy.transform_uri("/test"), "http://target/test");
    }

    #[test]
    fn transform_uri_with_query() {
        let proxy_root = ReverseProxy::new("/", "http://target");

        assert_eq!(
            proxy_root.transform_uri("?query=test"),
            "http://target?query=test"
        );
        assert_eq!(
            proxy_root.transform_uri("/?query=test"),
            "http://target/?query=test"
        );
        assert_eq!(
            proxy_root.transform_uri("/test?query=test"),
            "http://target/test?query=test"
        );

        let proxy_root_no_slash = ReverseProxy::new("/", "http://target/api");
        assert_eq!(
            proxy_root_no_slash.transform_uri("/test?query=test"),
            "http://target/api/test?query=test"
        );
        assert_eq!(
            proxy_root_no_slash.transform_uri("?query=test"),
            "http://target/api?query=test"
        );

        let proxy_root_slash = ReverseProxy::new("/", "http://target/api/");
        assert_eq!(
            proxy_root_slash.transform_uri("/test?query=test"),
            "http://target/api/test?query=test"
        );
        assert_eq!(
            proxy_root_slash.transform_uri("?query=test"),
            "http://target/api/?query=test"
        );

        let proxy_no_slash = ReverseProxy::new("/test", "http://target/api");
        assert_eq!(
            proxy_no_slash.transform_uri("/test?query=test"),
            "http://target/api?query=test"
        );
        assert_eq!(
            proxy_no_slash.transform_uri("/test/?query=test"),
            "http://target/api/?query=test"
        );
        assert_eq!(
            proxy_no_slash.transform_uri("?query=test"),
            "http://target/api?query=test"
        );

        let proxy_with_slash = ReverseProxy::new("/test", "http://target/api/");
        assert_eq!(
            proxy_with_slash.transform_uri("/test?query=test"),
            "http://target/api/?query=test"
        );
        assert_eq!(
            proxy_with_slash.transform_uri("/test/?query=test"),
            "http://target/api/?query=test"
        );
        assert_eq!(
            proxy_with_slash.transform_uri("/something"),
            "http://target/api/something"
        );
        assert_eq!(
            proxy_with_slash.transform_uri("/test/something"),
            "http://target/api/something"
        );
    }
}
