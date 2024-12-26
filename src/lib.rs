//! A flexible and efficient reverse proxy implementation for Axum web applications.
//!
//! This crate provides a reverse proxy that can be easily integrated into Axum applications,
//! allowing for seamless forwarding of HTTP requests to upstream servers. It supports:
//!
//! - Path-based routing
//! - Automatic retry mechanism
//! - Header forwarding
//! - Configurable HTTP client settings
//! - Easy integration with Axum's Router
//!
//! # Example
//!
//! ```rust
//! use axum::Router;
//! use axum_reverse_proxy::ReverseProxy;
//!
//! // Create a reverse proxy that forwards requests from /api to httpbin.org
//! let proxy = ReverseProxy::new("/api", "https://httpbin.org");
//!
//! // Convert the proxy to a router and use it in your Axum application
//! let app: Router = proxy.into();
//! ```
//!
//!  You can also merge the proxy with an existing router, compatible with arbitrary state:
//!
//! ```rust
//! use axum::{routing::get, Router, response::IntoResponse, extract::State};
//! use axum_reverse_proxy::ReverseProxy;
//!
//! #[derive(Clone)]
//! struct AppState { foo: usize }
//!
//! async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
//!     (axum::http::StatusCode::OK, format!("Hello, World! {}", state.foo))
//! }
//!
//! let app: Router<AppState> = Router::new()
//!     .route("/", get(root_handler))
//!     .merge(ReverseProxy::new("/api", "https://httpbin.org"))
//!     .with_state(AppState { foo: 42 });
//! ```

use axum::{body::Body, extract::State, http::Request, response::Response, Router};
use bytes as bytes_crate;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::StatusCode;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
    rt::TokioIo,
};
use std::convert::Infallible;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        handshake::client::Request as WsRequest,
        protocol::frame::coding::CloseCode,
        Message,
    },
    WebSocketStream,
};
use tracing::{error, trace};
use http::{HeaderMap, HeaderValue, Version};
use url::Url;

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
    client: Client<HttpConnector, BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>>>,
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

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build::<_, BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>>>(
                connector,
            );

        Self {
            path: path.into(),
            target: target.into(),
            client,
            options,
        }
    }

    /// Creates a new `ReverseProxy` instance with a custom HTTP client and default proxy options.
    pub fn new_with_client<S>(
        path: S,
        target: S,
        client: Client<
            HttpConnector,
            BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>>,
        >,
    ) -> Self
    where
        S: Into<String>,
    {
        Self {
            path: path.into(),
            target: target.into(),
            client,
            options: ProxyOptions::default(),
        }
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
    /// use http_body_util::{combinators::BoxBody, Empty};
    /// use bytes::Bytes;
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
        client: Client<
            HttpConnector,
            BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>>,
        >,
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

    /// Helper function to create a BoxBody from bytes
    fn create_box_body(
        bytes: bytes_crate::Bytes,
    ) -> BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>> {
        let full = Full::new(bytes);
        let mapped = full.map_err(|never: Infallible| match never {});
        let mapped = mapped.map_err(|_| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "unreachable",
            )) as Box<dyn std::error::Error + Send + Sync>
        });
        BoxBody::new(mapped)
    }

    /// Check if a request is a WebSocket upgrade request
    fn is_websocket_upgrade(headers: &HeaderMap<HeaderValue>) -> bool {
        // Check for required WebSocket upgrade headers
        let has_upgrade = headers
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false);

        let has_connection = headers
            .get("connection")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("upgrade"))
            .unwrap_or(false);

        let has_websocket_key = headers.contains_key("sec-websocket-key");
        let has_websocket_version = headers.contains_key("sec-websocket-version");

        has_upgrade && has_connection && has_websocket_key && has_websocket_version
    }

    /// Handle WebSocket upgrade and forward frames between client and upstream
    async fn handle_websocket(
        &self,
        req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn std::error::Error + Send + Sync>> {
        trace!("Handling WebSocket upgrade request");

        // Get the WebSocket key before upgrading
        let ws_key = req.headers()
            .get("sec-websocket-key")
            .and_then(|key| key.to_str().ok())
            .ok_or("Missing or invalid Sec-WebSocket-Key header")?;

        // Calculate the WebSocket accept key
        use base64::{engine::general_purpose::STANDARD, Engine};
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(ws_key.as_bytes());
        hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let ws_accept = STANDARD.encode(hasher.finalize());

        // Get the path and query from the request
        let path_and_query = req.uri().path_and_query()
            .map(|x| x.as_str())
            .unwrap_or("");

        trace!("Original path: {}", path_and_query);
        trace!("Proxy path: {}", self.path);

        // Create upstream WebSocket request
        let upstream_url = format!(
            "ws://{}{}",
            self.target.trim_start_matches("http://"),
            path_and_query
        );

        trace!("Connecting to upstream WebSocket at {}", upstream_url);

        // Parse the URL to get the host
        let url = Url::parse(&upstream_url)?;
        let host = url.host_str().ok_or("Missing host in URL")?;
        let port = url.port().unwrap_or(80);
        let host_header = if port == 80 {
            host.to_string()
        } else {
            format!("{}:{}", host, port)
        };

        // Forward all headers except host to upstream
        let mut request = WsRequest::builder()
            .uri(upstream_url)
            .header("host", host_header);

        for (key, value) in req.headers() {
            if key != "host" {
                request = request.header(key.as_str(), value);
            }
        }

        // Build the request
        let request = request.body(())?;

        // Log the request headers
        trace!("Upstream request headers: {:?}", request.headers());

        // Return a response that indicates the connection has been upgraded
        trace!("Returning upgrade response to client");
        let response = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", ws_accept)
            .body(Body::empty())?;

        // Spawn a task to handle the WebSocket connection
        let (parts, body) = req.into_parts();
        let req = Request::from_parts(parts, body);
        tokio::spawn(async move {
            match Self::handle_websocket_connection(req, request).await {
                Ok(_) => trace!("WebSocket connection closed gracefully"),
                Err(e) => error!("WebSocket connection error: {}", e),
            }
        });

        Ok(response)
    }

    /// Handle the actual WebSocket connection after the upgrade
    async fn handle_websocket_connection(
        req: Request<Body>,
        upstream_request: tokio_tungstenite::tungstenite::handshake::client::Request,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use tokio::time::{timeout, Duration};
        use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
        use tokio::sync::mpsc;

        // Upgrade client connection to WebSocket with timeout
        let upgraded = match timeout(Duration::from_secs(5), hyper::upgrade::on(req)).await {
            Ok(Ok(upgraded)) => {
                trace!("Successfully upgraded client connection");
                upgraded
            }
            Ok(Err(e)) => {
                error!("Failed to upgrade client connection: {}", e);
                return Err(Box::new(e));
            }
            Err(e) => {
                error!("Timeout upgrading client connection: {}", e);
                return Err(Box::new(e));
            }
        };
        let io = TokioIo::new(upgraded);
        let client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        trace!("Upgraded client connection to WebSocket");

        // Connect to upstream WebSocket with timeout
        let (upstream_ws, response) = match timeout(Duration::from_secs(5), connect_async(upstream_request)).await {
            Ok(Ok(conn)) => {
                trace!("Successfully connected to upstream WebSocket");
                conn
            }
            Ok(Err(e)) => {
                error!("Failed to connect to upstream WebSocket: {}", e);
                return Err(Box::new(e));
            }
            Err(e) => {
                error!("Timeout connecting to upstream WebSocket: {}", e);
                return Err(Box::new(e));
            }
        };
        trace!("Upstream response status: {}", response.status());
        trace!("Upstream response headers: {:?}", response.headers());

        // Create channels for coordinating close frames
        let (close_tx, mut close_rx) = mpsc::channel(1);
        let close_tx_upstream = close_tx.clone();

        // Split both connections into sender and receiver halves
        let (mut client_tx, mut client_rx) = client_ws.split();
        let (mut upstream_tx, mut upstream_rx) = upstream_ws.split();

        // Forward messages in both directions
        let client_to_upstream = async move {
            while let Some(msg) = client_rx.next().await {
                match msg {
                    Ok(msg) => {
                        trace!("Forwarding message from client to upstream");
                        if let Message::Close(frame) = msg.clone() {
                            trace!("Received close frame from client: {:?}", frame);
                            // Forward close frame to upstream
                            if let Err(e) = timeout(Duration::from_secs(5), upstream_tx.send(msg)).await {
                                error!("Timeout sending close frame to upstream: {}", e);
                            }
                            // Notify the other task about the close frame
                            let _ = close_tx_upstream.send(()).await;
                            break;
                        }
                        // Forward other messages with timeout
                        if let Err(e) = timeout(Duration::from_secs(5), upstream_tx.send(msg)).await {
                            error!("Timeout forwarding message to upstream: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Error receiving message from client: {}", e);
                        break;
                    }
                }
            }
            trace!("Client to upstream task completed");
        };

        let upstream_to_client = async move {
            while let Some(msg) = upstream_rx.next().await {
                match msg {
                    Ok(msg) => {
                        trace!("Forwarding message from upstream to client");
                        if let Message::Close(frame) = msg.clone() {
                            trace!("Received close frame from upstream: {:?}", frame);
                            // Forward close frame to client
                            if let Err(e) = timeout(Duration::from_secs(5), client_tx.send(msg)).await {
                                error!("Timeout sending close frame to client: {}", e);
                            }
                            // Notify the other task about the close frame
                            let _ = close_tx.send(()).await;
                            break;
                        }
                        // Forward other messages with timeout
                        if let Err(e) = timeout(Duration::from_secs(5), client_tx.send(msg)).await {
                            error!("Timeout forwarding message to client: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Error receiving message from upstream: {}", e);
                        break;
                    }
                }
            }
            trace!("Upstream to client task completed");
        };

        // Run both forwarding tasks concurrently
        tokio::select! {
            _ = client_to_upstream => {
                trace!("Client to upstream task completed");
            }
            _ = upstream_to_client => {
                trace!("Upstream to client task completed");
            }
            _ = close_rx.recv() => {
                trace!("Close frame received, shutting down connection");
            }
        }

        Ok(())
    }

    /// Handles the proxying of a single request to the upstream server.
    async fn proxy_request(&self, mut req: Request<Body>) -> Result<Response<Body>, Infallible> {
        trace!("Proxying request method={} uri={}", req.method(), req.uri());
        trace!("Original headers headers={:?}", req.headers());

        // Check if this is a WebSocket upgrade request
        if Self::is_websocket_upgrade(req.headers()) {
            trace!("Detected WebSocket upgrade request");
            match self.handle_websocket(req).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    error!("Failed to handle WebSocket upgrade: {}", e);
                    return Ok(Response::builder()
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
                    return Ok(Response::builder()
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
            req = Request::from_parts(parts, Body::from(buffered_body.as_ref().unwrap().clone()));
        }

        loop {
            let forward_req = {
                let mut builder = Request::builder()
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
                    let bytes = buffered_body
                        .as_ref()
                        .unwrap_or(&bytes_crate::Bytes::new())
                        .clone();
                    Self::create_box_body(bytes)
                } else {
                    // For streaming mode, we take ownership of the body and convert it
                    let (parts, body) = req.into_parts();
                    req = Request::from_parts(parts, Body::empty());

                    // Convert the axum Body into a BoxBody
                    let bytes = body
                        .collect()
                        .await
                        .map(|collected| collected.to_bytes())
                        .unwrap_or_else(|_| bytes_crate::Bytes::new());
                    Self::create_box_body(bytes)
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

                    // Convert the response body into a streaming axum Body
                    let bytes = body
                        .collect()
                        .await
                        .map(|collected| collected.to_bytes())
                        .unwrap_or_else(|_| bytes_crate::Bytes::new());
                    let boxed = Self::create_box_body(bytes);
                    let body = Body::new(boxed);

                    let mut response = Response::new(body);
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
                        return Ok(Response::builder()
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

/// Enables conversion from a `ReverseProxy` into an Axum `Router`.
///
/// This implementation allows the reverse proxy to be easily integrated into an Axum
/// application. It handles:
///
/// - Path-based routing using the configured base path
/// - State management using `Arc` for thread-safety
/// - Fallback handling for all HTTP methods
///
/// # Example
///
/// ```rust
/// use axum::Router;
/// use axum_reverse_proxy::ReverseProxy;
///
/// let proxy = ReverseProxy::new("/api", "https://api.example.com");
/// let app: Router = proxy.into();
/// ```
impl<S> From<ReverseProxy> for Router<S>
where
    S: Send + Sync + Clone + 'static,
{
    fn from(proxy: ReverseProxy) -> Self {
        let path = proxy.path.clone();
        let proxy_router = Router::new()
            .fallback(|State(proxy): State<ReverseProxy>, req| async move {
                proxy.proxy_request(req).await
            })
            .with_state(proxy);

        if ["", "/"].contains(&path.as_str()) {
            proxy_router
        } else {
            Router::new().nest(&path, proxy_router)
        }
    }
}
