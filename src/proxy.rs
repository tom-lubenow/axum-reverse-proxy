use axum::body::Body;
use http::StatusCode;
use http_body_util::BodyExt;
#[cfg(all(feature = "tls", not(feature = "native-tls")))]
use hyper_rustls::HttpsConnector;
#[cfg(feature = "native-tls")]
use hyper_tls::HttpsConnector as NativeTlsHttpsConnector;
use hyper_util::client::legacy::{
    Client,
    connect::{Connect, HttpConnector},
};
use std::convert::Infallible;
use tracing::{error, trace};

use crate::websocket;

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
}

#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub type StandardReverseProxy = ReverseProxy<HttpsConnector<HttpConnector>>;
#[cfg(feature = "native-tls")]
pub type StandardReverseProxy = ReverseProxy<NativeTlsHttpsConnector<HttpConnector>>;
#[cfg(all(not(feature = "tls"), not(feature = "native-tls")))]
pub type StandardReverseProxy = ReverseProxy<HttpConnector>;

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
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.enforce_http(false);
        connector.set_keepalive(Some(std::time::Duration::from_secs(60)));
        connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
        connector.set_reuse_address(true);

        #[cfg(all(feature = "tls", not(feature = "native-tls")))]
        let connector = {
            use hyper_rustls::HttpsConnectorBuilder;
            HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .wrap_connector(connector)
        };

        #[cfg(feature = "native-tls")]
        let connector = NativeTlsHttpsConnector::new_with_connector(connector);

        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(connector);

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
        }
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
        trace!("Original headers headers={:?}", req.headers());

        // Check if this is a WebSocket upgrade request
        if websocket::is_websocket_upgrade(req.headers()) {
            trace!("Detected WebSocket upgrade request");
            match websocket::handle_websocket(req, &self.target).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    error!("Failed to handle WebSocket upgrade: {}", e);
                    return Ok(axum::http::Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from(format!("WebSocket upgrade failed: {e}")))
                        .unwrap());
                }
            }
        }

        let forward_req = {
            let mut builder =
                axum::http::Request::builder()
                    .method(req.method().clone())
                    .uri(self.transform_uri(
                        req.uri().path_and_query().map(|x| x.as_str()).unwrap_or(""),
                    ));

            // Forward headers
            for (key, value) in req.headers() {
                if key != "host" {
                    builder = builder.header(key, value);
                }
            }

            // Take the request body
            let (parts, body) = req.into_parts();
            drop(parts);
            builder.body(body).unwrap()
        };

        trace!(
            "Forwarding headers forwarded_headers={:?}",
            forward_req.headers()
        );

        match self.client.request(forward_req).await {
            Ok(res) => {
                trace!(
                    "Received response status={} headers={:?} version={:?}",
                    res.status(),
                    res.headers(),
                    res.version()
                );

                let (parts, body) = res.into_parts();
                let body = Body::from_stream(body.into_data_stream());

                let mut response = axum::http::Response::new(body);
                *response.status_mut() = parts.status;
                *response.version_mut() = parts.version;
                *response.headers_mut() = parts.headers;
                Ok(response)
            }
            Err(e) => {
                let error_msg = e.to_string();
                error!("Proxy error occurred err={}", error_msg);
                Ok(axum::http::Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!(
                        "Failed to connect to upstream server: {error_msg}"
                    )))
                    .unwrap())
            }
        }
    }

    /// Transform an incoming request path into the target URI
    fn transform_uri(&self, path_and_query: &str) -> String {
        let (path, query) = path_and_query.split_once('?').map(|(p,q)| (p, Some(q))).unwrap_or((path_and_query, None));

        let target = self.target.trim_end_matches('/');
        let base_path = self.path.trim_end_matches('/');

        let remaining = if path == "/" && !self.path.is_empty() {
            ""
        } else if let Some(stripped) = path.strip_prefix(base_path) {
            stripped
        } else {
            path
        };

        let mut uri = String::with_capacity(target.len() + remaining.len() + query.map_or(0, |q| 1 + q.len()));
        uri.push_str(target);
        uri.push_str(remaining);
        if let Some(query) = query {
            if !uri.ends_with('/') && self.target.ends_with('/') && ( remaining.is_empty() || path_and_query.find("/?").is_some_and(|i| i > 0)) {
                uri.push('/');
            }
            uri.push('?');
            uri.push_str(query);
        }
        uri
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

    use super::StandardReverseProxy as ReverseProxy;

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
    fn transform_uri_with_query() {
        let proxy_root = ReverseProxy::new("/", "http://target");

        assert_eq!(
            proxy_root.transform_uri("?query=test"),
            "http://target?query=test"
        );
        assert_eq!(
            proxy_root.transform_uri("/?query=test"),
            "http://target?query=test"
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

        let proxy_root_slash = ReverseProxy::new("/test", "http://target/api/");
        assert_eq!(
            proxy_root_slash.transform_uri("/test?query=test"),
            "http://target/api/?query=test"
        );
        assert_eq!(
            proxy_root_slash.transform_uri("/test/?query=test"),
            "http://target/api/?query=test"
        );
        assert_eq!(
            proxy_root_slash.transform_uri("/something"),
            "http://target/api/something"
        );
        assert_eq!(
            proxy_root_slash.transform_uri("/test/something"),
            "http://target/api/something"
        );
    }

    #[test]
    fn transform_uri_root() {
        let proxy = ReverseProxy::new("/", "http://target");
        assert_eq!(proxy.transform_uri("/test"), "http://target/test");
    }
}
