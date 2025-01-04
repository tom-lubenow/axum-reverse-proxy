use axum::body::Body;
use bytes as bytes_crate;
use http::{StatusCode, Version};
use http_body_util::BodyExt;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use std::convert::Infallible;
use tracing::{error, trace};

use crate::websocket;

/// Configuration options for the reverse proxy
#[derive(Clone, Debug, Default)]
pub struct ProxyOptions {
    /// Whether to buffer the entire request/response bodies in memory
    /// If false (default), requests and responses will be streamed
    pub buffer_bodies: bool,
}

/// A reverse proxy that forwards HTTP requests to an upstream server.
///
/// The `ReverseProxy` struct handles the forwarding of HTTP requests from a specified path
/// to a target upstream server. It manages its own HTTP client with configurable settings
/// for connection pooling, timeouts, and retries.
#[derive(Clone)]
pub struct ReverseProxy {
    path: String,
    target: String,
    client: Client<HttpConnector, Body>,
    options: ProxyOptions,
}

impl ReverseProxy {
    /// Creates a new `ReverseProxy` instance with default options.
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
        Self::new_with_options(path, target, ProxyOptions::default())
    }

    /// Creates a new `ReverseProxy` instance with custom proxy options and a default HTTP client configuration.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against
    /// * `target` - The upstream server URL to forward requests to
    /// * `options` - Custom configuration options for the proxy
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::{ReverseProxy, ProxyOptions};
    ///
    /// let options = ProxyOptions {
    ///     buffer_bodies: true,
    /// };
    /// let proxy = ReverseProxy::new_with_options("/api", "https://api.example.com", options);
    /// ```
    pub fn new_with_options<S>(path: S, target: S, options: ProxyOptions) -> Self
    where
        S: Into<String>,
    {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.enforce_http(false);
        connector.set_keepalive(Some(std::time::Duration::from_secs(60)));
        connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
        connector.set_reuse_address(true);

        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(connector);

        Self::new_with_client_and_options(path, target, client, options)
    }

    /// Creates a new `ReverseProxy` instance with a custom HTTP client and default proxy options.
    pub fn new_with_client<S>(path: S, target: S, client: Client<HttpConnector, Body>) -> Self
    where
        S: Into<String>,
    {
        Self::new_with_client_and_options(path, target, client, ProxyOptions::default())
    }

    /// Creates a new `ReverseProxy` instance with a custom HTTP client and options.
    ///
    /// This method allows for more fine-grained control over the proxy behavior by accepting
    /// a pre-configured HTTP client.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against
    /// * `target` - The upstream server URL to forward requests to
    /// * `client` - A custom-configured HTTP client
    /// * `options` - Custom configuration options for the proxy
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::{ReverseProxy, ProxyOptions};
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
    pub fn new_with_client_and_options<S>(
        path: S,
        target: S,
        client: Client<HttpConnector, Body>,
        options: ProxyOptions,
    ) -> Self
    where
        S: Into<String>,
    {
        Self {
            path: path.into(),
            target: target.into(),
            client,
            options,
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
        mut req: axum::http::Request<Body>,
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
                        .body(Body::from(format!("WebSocket upgrade failed: {}", e)))
                        .unwrap());
                }
            }
        }

        let mut retries: u32 = 3;
        let mut error_msg;
        let mut buffered_body: Option<bytes_crate::Bytes> = None;

        if self.options.buffer_bodies {
            // If we're in buffered mode, collect the body once at the start
            let (parts, body) = req.into_parts();
            buffered_body = match body.collect().await {
                Ok(collected) => Some(collected.to_bytes()),
                Err(e) => {
                    error!("Failed to read request body: {}", e);
                    return Ok(axum::http::Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap());
                }
            };
            trace!(
                "Request body collected body_length={}",
                buffered_body.as_ref().unwrap().len()
            );

            // Reconstruct the request with the buffered body
            req = axum::http::Request::from_parts(
                parts,
                Body::from(buffered_body.as_ref().unwrap().clone()),
            );
        }

        loop {
            let forward_req = {
                let mut builder = axum::http::Request::builder()
                    .method(req.method().clone())
                    .version(Version::HTTP_11)
                    .uri(format!(
                        "{}{}",
                        self.target,
                        req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
                    ));

                // Forward headers
                for (key, value) in req.headers() {
                    if key != "host" {
                        builder = builder.header(key, value);
                    }
                }

                // Create the request body
                let body = if self.options.buffer_bodies {
                    // Only buffer if explicitly requested
                    Body::from(buffered_body.as_ref().unwrap().clone())
                } else {
                    // For streaming mode, simply take the body as is
                    let (parts, body) = req.into_parts();
                    req = axum::http::Request::from_parts(parts, Body::empty());
                    body
                };

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

                    // Convert the response body
                    let body = if self.options.buffer_bodies {
                        // Only buffer if explicitly requested
                        let bytes = body
                            .collect()
                            .await
                            .map(|collected| collected.to_bytes())
                            .unwrap_or_else(|_| bytes_crate::Bytes::new());
                        Body::from(bytes)
                    } else {
                        Body::from_stream(body.into_data_stream())
                    };

                    let mut response = axum::http::Response::new(body);
                    *response.status_mut() = parts.status;
                    *response.version_mut() = parts.version;
                    *response.headers_mut() = parts.headers;
                    return Ok(response);
                }
                Err(e) => {
                    error_msg = e.to_string();
                    retries = retries.saturating_sub(1);
                    if retries == 0 {
                        error!("Proxy error occurred after all retries err={}", error_msg);
                        return Ok(axum::http::Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from(format!(
                                "Failed to connect to upstream server: {}",
                                error_msg
                            )))
                            .unwrap());
                    }
                    error!(
                        "Proxy error occurred, retrying ({} left) err={}",
                        retries, error_msg
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }
}
