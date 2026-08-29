use axum::body::Body;
use http::Uri;
use http::uri::Builder as UriBuilder;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::convert::Infallible;
use std::time::Duration;
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
    scheme: String,
    authority: String,
    /// Normalized target base path: `""` for `/`, otherwise `/foo` with no
    /// trailing slash.
    target_base_path: String,
    /// Whether the configured target path ended with a trailing slash
    /// (excluding a bare `/`), which is preserved when no path remains.
    target_trailing_slash: bool,
    client: Client<C, Body>,
    policy: ProxyPolicy,
}

/// Proxy route and behavioural config applied to every request a
/// [`ReverseProxy`] forwards.
///
/// Construct with [`ProxyPolicy::new`] (or `Default`) and customize with the
/// builder methods:
///
/// ```rust
/// use axum_reverse_proxy::{HostBehaviour, ProxyPolicy};
///
/// let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProxyPolicy {
    /// How the upstream `Host` header is derived. See [`HostBehaviour`].
    pub host_behaviour: HostBehaviour,
    /// How `X-Forwarded-For` is handled. See [`XForwardedFor`].
    pub x_forwarded_for: XForwardedFor,
    /// How long to wait when establishing an upstream WebSocket connection
    /// before failing the client's upgrade request. Default: 5 seconds.
    pub websocket_connect_timeout: Duration,
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self {
            host_behaviour: HostBehaviour::default(),
            x_forwarded_for: XForwardedFor::default(),
            websocket_connect_timeout: Duration::from_secs(5),
        }
    }
}

impl ProxyPolicy {
    /// Create a policy with default behaviour.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set how the upstream `Host` header is derived.
    #[must_use]
    pub fn with_host_behaviour(mut self, behaviour: HostBehaviour) -> Self {
        self.host_behaviour = behaviour;
        self
    }

    /// Set how `X-Forwarded-For` is handled.
    #[must_use]
    pub fn with_x_forwarded_for(mut self, mode: XForwardedFor) -> Self {
        self.x_forwarded_for = mode;
        self
    }

    /// Set the upstream WebSocket connect timeout.
    #[must_use]
    pub fn with_websocket_connect_timeout(mut self, timeout: Duration) -> Self {
        self.websocket_connect_timeout = timeout;
        self
    }

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
    /// Replace `Host` with the upstream target's authority (the default).
    #[default]
    Replace,
}

/// Controls how the `X-Forwarded-For` header is handled for forwarded requests.
///
/// The client's address is only known when the Axum server was started with
/// [`into_make_service_with_connect_info`](axum::Router::into_make_service_with_connect_info);
/// without it, no address is available and the header is left untouched in
/// either mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XForwardedFor {
    /// Append the connecting client's IP address to `X-Forwarded-For`
    /// (creating the header if absent), and set `X-Forwarded-Host` from the
    /// client's `Host` header when not already present. This is the default
    /// and matches conventional reverse proxy behaviour.
    #[default]
    Append,
    /// Leave `X-Forwarded-For` and `X-Forwarded-Host` exactly as the client
    /// sent them.
    Preserve,
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
    /// # Panics
    ///
    /// Panics if `target` is not a valid URI with an authority (host). The
    /// target is validated once here rather than on every request.
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::ReverseProxy;
    ///
    /// let proxy = ReverseProxy::new("/api", "https://api.example.com");
    /// ```
    pub fn new<P, T>(path: P, target: T) -> Self
    where
        P: Into<String>,
        T: Into<String>,
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
    /// # Panics
    ///
    /// Panics if `target` is not a valid URI with an authority (host). The
    /// target is validated once here rather than on every request.
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
    pub fn new_with_client<P, T>(path: P, target: T, client: Client<C, Body>) -> Self
    where
        P: Into<String>,
        T: Into<String>,
    {
        let target = target.into();
        let target_uri: Uri = target
            .parse()
            .unwrap_or_else(|e| panic!("ReverseProxy target {target:?} is not a valid URI: {e}"));
        let scheme = target_uri.scheme_str().unwrap_or("http").to_string();
        let authority = target_uri
            .authority()
            .unwrap_or_else(|| {
                panic!("ReverseProxy target {target:?} must include an authority (host)")
            })
            .as_str()
            .to_string();
        let target_path = target_uri.path();
        let target_trailing_slash = target_path.ends_with('/') && target_path != "/";
        let target_base_path = if target_path == "/" {
            String::new()
        } else {
            target_path.trim_end_matches('/').to_string()
        };

        Self {
            path: path.into(),
            target,
            scheme,
            authority,
            target_base_path,
            target_trailing_slash,
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

    /// Transform an incoming request path+query into the target URI.
    ///
    /// Rules:
    /// - Trim target trailing slash for joining
    /// - Strip proxy base path at a boundary (exact or followed by '/')
    /// - If remainder is exactly '/' under a non-empty base, treat as empty
    /// - Do not add a slash for query-only joins (avoid target '/?')
    fn transform_uri(&self, path_and_query: &str) -> Uri {
        let base_path = self.path.trim_end_matches('/');

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

        // Join the target base path with the remainder. `remaining_path` is
        // either empty or starts with '/', and `target_base_path` is either
        // empty or `/`-prefixed with no trailing slash, so plain concatenation
        // never duplicates a slash.
        let mut path_and_query_buf = if remaining_path.is_empty() {
            if self.target_base_path.is_empty() {
                "/".to_string()
            } else if self.target_trailing_slash {
                format!("{}/", self.target_base_path)
            } else {
                self.target_base_path.clone()
            }
        } else {
            format!("{}{}", self.target_base_path, remaining_path)
        };

        if let Some(q) = query_part {
            path_and_query_buf.push('?');
            path_and_query_buf.push_str(q);
        }

        // Build the full URI
        UriBuilder::new()
            .scheme(self.scheme.as_str())
            .authority(self.authority.as_str())
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
        let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
        let headers = headers_with_host(Some("client.example.com"));
        assert_eq!(
            policy.forwarded_host(&headers, "upstream:8080".to_string()),
            "client.example.com"
        );
    }

    #[test]
    fn upstream_host_with_preserve_policy_and_no_client_host_falls_back_to_authority() {
        let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
        let headers = headers_with_host(None);
        assert_eq!(
            policy.forwarded_host(&headers, "upstream:8080".to_string()),
            "upstream:8080"
        );
    }

    #[test]
    #[should_panic(expected = "must include an authority")]
    fn invalid_target_panics_at_construction() {
        let _ = ReverseProxy::new("/api", "/not-a-url");
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
