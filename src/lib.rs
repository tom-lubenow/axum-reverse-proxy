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
//! - Full Tower middleware support
//!
//! # Basic Example
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
//! # Using Tower Middleware
//!
//! The proxy integrates seamlessly with Tower middleware, allowing you to transform requests
//! and responses, add authentication, logging, timeouts, and more:
//!
//! ```rust
//! use axum::{body::Body, Router};
//! use axum_reverse_proxy::ReverseProxy;
//! use http::Request;
//! use tower::ServiceBuilder;
//! use tower_http::{
//!     timeout::TimeoutLayer,
//!     validate_request::ValidateRequestHeaderLayer,
//! };
//! use std::time::Duration;
//!
//! // Create a reverse proxy
//! let proxy = ReverseProxy::new("/api", "https://api.example.com");
//!
//! // Convert to router
//! let proxy_router: Router = proxy.into();
//!
//! // Add middleware layers
//! let app = proxy_router.layer(
//!     ServiceBuilder::new()
//!         // Add request timeout
//!         .layer(TimeoutLayer::new(Duration::from_secs(10)))
//!         // Require API key
//!         .layer(ValidateRequestHeaderLayer::bearer("secret-token"))
//!         // Transform requests
//!         .map_request(|mut req: Request<Body>| {
//!             req.headers_mut().insert(
//!                 "X-Custom-Header",
//!                 "custom-value".parse().unwrap(),
//!             );
//!             req
//!         })
//! );
//! ```
//!
//! Common middleware use cases include:
//! - Request/response transformation
//! - Authentication and authorization
//! - Rate limiting
//! - Request validation
//! - Logging and tracing
//! - Timeouts and retries
//! - Caching
//! - Compression
//!
//! See the `tower_middleware` example for a complete working example.
//!
//! # State Management
//!
//! You can merge the proxy with an existing router that has state:
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

mod proxy;
mod rfc9110;
mod router;
mod websocket;

pub use proxy::{ProxyOptions, ReverseProxy};
pub use rfc9110::{Rfc9110Config, Rfc9110Layer};
