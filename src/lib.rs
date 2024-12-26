//! A flexible and efficient reverse proxy implementation for Axum web applications.
//!
//! This crate provides a reverse proxy that can be easily integrated into Axum applications,
//! allowing for seamless forwarding of HTTP requests and WebSocket connections to upstream servers.
//! It supports:
//!
//! - Path-based routing
//! - Automatic retry mechanism
//! - Header forwarding
//! - Configurable HTTP client settings
//! - WebSocket proxying with:
//!   - Automatic upgrade handling
//!   - Bidirectional message forwarding
//!   - Text and binary message support
//!   - Proper close frame handling
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
//! You can also merge the proxy with an existing router, compatible with arbitrary state:
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
//!
//! # WebSocket Support
//!
//! The proxy automatically detects WebSocket upgrade requests and handles them appropriately:
//!
//! ```rust
//! use axum::Router;
//! use axum_reverse_proxy::ReverseProxy;
//!
//! // Create a reverse proxy that forwards both HTTP and WebSocket requests
//! let proxy = ReverseProxy::new("/ws", "http://websocket.example.com");
//!
//! // WebSocket connections to /ws will be automatically proxied
//! let app: Router = proxy.into();
//! ```
//!
//! The proxy handles:
//! - WebSocket upgrade handshake
//! - Bidirectional message forwarding
//! - Text and binary messages
//! - Ping/Pong frames
//! - Connection close frames
//! - Multiple concurrent connections

use axum::{body::Body, extract::State, http::Request, response::Response, Router};
use bytes as bytes_crate;
use futures_util::SinkExt;
use http::{HeaderMap, HeaderValue, Version};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::StatusCode;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
    rt::TokioIo,
};
use std::convert::Infallible;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::{connect_async, tungstenite::handshake::client::Request as WsRequest};
use tracing::{error, trace};
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
    client: Client<
        HttpConnector,
        BoxBody<bytes_crate::Bytes, Box<dyn std::error::Error + Send + Sync>>,
    >,
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

    /// Check if a request is a WebSocket upgrade request by examining the headers.
    ///
    /// According to the WebSocket protocol specification (RFC 6455), a WebSocket upgrade request must have:
    /// - An "Upgrade: websocket" header (case-insensitive)
    /// - A "Connection: Upgrade" header (case-insensitive)
    /// - A "Sec-WebSocket-Key" header with a base64-encoded 16-byte value
    /// - A "Sec-WebSocket-Version" header
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

    /// Handle a WebSocket upgrade request by:
    /// 1. Validating the upgrade request
    /// 2. Computing the WebSocket accept key
    /// 3. Establishing a connection to the upstream server
    /// 4. Returning an upgrade response to the client
    /// 5. Spawning a task to handle the WebSocket connection
    ///
    /// This function follows the WebSocket protocol specification (RFC 6455) for the upgrade handshake.
    /// It ensures that all required headers are properly handled and forwarded to the upstream server.
    async fn handle_websocket(
        &self,
        req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn std::error::Error + Send + Sync>> {
        trace!("Handling WebSocket upgrade request");

        // Get the WebSocket key before upgrading
        let ws_key = req
            .headers()
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
        let path_and_query = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("");

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

    /// Handle an established WebSocket connection by forwarding frames between the client and upstream server.
    ///
    /// This function:
    /// 1. Upgrades the HTTP connection to a WebSocket connection
    /// 2. Establishes a WebSocket connection to the upstream server
    /// 3. Creates two tasks for bidirectional message forwarding:
    ///    - Client to upstream: forwards messages from the client to the upstream server
    ///    - Upstream to client: forwards messages from the upstream server to the client
    /// 4. Handles various WebSocket message types:
    ///    - Text messages
    ///    - Binary messages
    ///    - Ping/Pong messages
    ///    - Close frames
    ///
    /// The connection is maintained until either:
    /// - A close frame is received from either side
    /// - An error occurs in the connection
    /// - The connection is dropped
    ///
    /// When a close frame is received, it is properly forwarded to ensure clean connection termination.
    async fn handle_websocket_connection(
        req: Request<Body>,
        upstream_request: tokio_tungstenite::tungstenite::handshake::client::Request,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use futures_util::stream::StreamExt;
        use tokio::sync::mpsc;
        use tokio::time::{timeout, Duration};
        use tokio_tungstenite::tungstenite::Message;

        let upgraded = match timeout(Duration::from_secs(5), hyper::upgrade::on(req)).await {
            Ok(Ok(upgraded)) => upgraded,
            Ok(Err(e)) => return Err(Box::new(e)),
            Err(e) => return Err(Box::new(e)),
        };

        let io = TokioIo::new(upgraded);
        let client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        let (upstream_ws, _) =
            match timeout(Duration::from_secs(5), connect_async(upstream_request)).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(e)) => return Err(Box::new(e)),
                Err(e) => return Err(Box::new(e)),
            };

        let (mut client_sender, mut client_receiver) = client_ws.split();
        let (mut upstream_sender, mut upstream_receiver) = upstream_ws.split();

        let (close_tx, mut close_rx) = mpsc::channel::<()>(1);
        let close_tx_upstream = close_tx.clone();

        let client_to_upstream = tokio::spawn(async move {
            let mut client_closed = false;
            while let Some(msg) = client_receiver.next().await {
                let msg = msg?;
                match msg {
                    Message::Close(_) => {
                        if !client_closed {
                            upstream_sender.send(Message::Close(None)).await?;
                            close_tx.send(()).await.ok();
                            client_closed = true;
                            break;
                        }
                    }
                    msg @ Message::Binary(_)
                    | msg @ Message::Text(_)
                    | msg @ Message::Ping(_)
                    | msg @ Message::Pong(_) => {
                        if !client_closed {
                            upstream_sender.send(msg).await?;
                        }
                    }
                    Message::Frame(_) => {}
                }
            }
            if !client_closed {
                upstream_sender.send(Message::Close(None)).await?;
                close_tx.send(()).await.ok();
            }
            Ok::<_, Error>(())
        });

        let upstream_to_client = tokio::spawn(async move {
            let mut upstream_closed = false;
            while let Some(msg) = upstream_receiver.next().await {
                let msg = msg?;
                match msg {
                    Message::Close(_) => {
                        if !upstream_closed {
                            client_sender.send(Message::Close(None)).await?;
                            close_tx_upstream.send(()).await.ok();
                            upstream_closed = true;
                            break;
                        }
                    }
                    msg @ Message::Binary(_)
                    | msg @ Message::Text(_)
                    | msg @ Message::Ping(_)
                    | msg @ Message::Pong(_) => {
                        if !upstream_closed {
                            client_sender.send(msg).await?;
                        }
                    }
                    Message::Frame(_) => {}
                }
            }
            if !upstream_closed {
                client_sender.send(Message::Close(None)).await?;
                close_tx_upstream.send(()).await.ok();
            }
            Ok::<_, Error>(())
        });

        tokio::select! {
            _ = close_rx.recv() => {
                trace!("WebSocket connection closed gracefully");
            }
            res = client_to_upstream => {
                if let Err(e) = res {
                    error!("Client to upstream task failed: {:?}", e);
                }
            }
            res = upstream_to_client => {
                if let Err(e) = res {
                    error!("Upstream to client task failed: {:?}", e);
                }
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
