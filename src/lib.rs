//! A flexible and efficient reverse proxy implementation for Axum web applications.
//!
//! This crate provides a reverse proxy that can be easily integrated into Axum applications,
//! allowing for seamless forwarding of HTTP requests and WebSocket connections to upstream servers.
//! It supports:
//!
//! - Path-based routing
//! - Optional retry mechanism via a [`tower::Layer`]
//! - Header forwarding
//! - Configurable HTTP client settings
//! - Round-robin load balancing across multiple upstreams
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
//! # Load Balanced Example
//!
//! ```rust
//! use axum::Router;
//! use axum_reverse_proxy::BalancedProxy;
//!
//! let proxy = BalancedProxy::new("/api", vec!["https://api1.example.com", "https://api2.example.com"]);
//! let app: Router = proxy.into();
//! ```
//!
//! # Service Discovery Example
//!
//! For dynamic service discovery, use the `DiscoverableBalancedProxy` with any implementation
//! of the `tower::discover::Discover` trait:
//!
//! ```rust,no_run
//! use axum::Router;
//! use axum_reverse_proxy::{DiscoverableBalancedProxy, LoadBalancingStrategy};
//! use futures_util::stream::Stream;
//! use tower::discover::Change;
//! use std::pin::Pin;
//! use std::task::{Context, Poll};
//!
//! // Example discovery stream that implements Stream<Item = Result<Change<Key, Service>, Error>>
//! #[derive(Clone)]
//! struct MyDiscoveryStream {
//!     // Your discovery implementation here
//! }
//!
//! impl Stream for MyDiscoveryStream {
//!     type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;
//!
//!     fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//!         // Poll your service discovery system here
//!         Poll::Pending
//!     }
//! }
//!
//! # async fn example() {
//! use hyper_util::client::legacy::{connect::HttpConnector, Client};
//!
//! let discovery = MyDiscoveryStream { /* ... */ };
//! let connector = HttpConnector::new();
//! let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);
//!
//! // Use round-robin load balancing (default)
//! let mut proxy = DiscoverableBalancedProxy::new_with_client("/api", client.clone(), discovery.clone());
//!
//! // Or specify a load balancing strategy
//! let mut proxy_p2c = DiscoverableBalancedProxy::new_with_client_and_strategy(
//!     "/api",
//!     client,
//!     discovery,
//!     LoadBalancingStrategy::P2cPendingRequests,
//! );
//!
//! proxy.start_discovery().await;
//!
//! let app: Router = Router::new().nest_service("/", proxy);
//! # }
//! ```
//!
//! # DNS-Based Service Discovery
//!
//! For DNS-based service discovery, use the built-in `DnsDiscovery` (requires the `dns` feature):
//!
//! ```toml
//! [dependencies]
//! axum-reverse-proxy = { version = "*", features = ["dns"] }
//! # Or to enable all features:
//! # axum-reverse-proxy = { version = "*", features = ["full"] }
//! ```
//!
//! ```rust,no_run
//! # #[cfg(feature = "dns")]
//! # {
//! use axum::Router;
//! use axum_reverse_proxy::{DiscoverableBalancedProxy, DnsDiscovery, DnsDiscoveryConfig};
//! use std::time::Duration;
//!
//! # async fn example() {
//! // Create DNS discovery configuration
//! let dns_config = DnsDiscoveryConfig::new("api.example.com", 80)
//!     .with_refresh_interval(Duration::from_secs(30))
//!     .with_https(false);
//!
//! // Create the DNS discovery instance
//! let discovery = DnsDiscovery::new(dns_config).expect("Failed to create DNS discovery");
//!
//! // Create the discoverable balanced proxy with DNS discovery
//! let mut proxy = DiscoverableBalancedProxy::new_with_client("/api",
//!     hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
//!         .build(hyper_util::client::legacy::connect::HttpConnector::new()),
//!     discovery);
//!
//! // Start the discovery process
//! proxy.start_discovery().await;
//!
//! let app: Router = Router::new().nest_service("/", proxy);
//! # }
//! # }
//! ```
//!
//! # Load Balancing Strategies
//!
//! The `DiscoverableBalancedProxy` supports multiple load balancing strategies:
//!
//! - **`RoundRobin`** (default): Simple round-robin distribution, good for homogeneous services
//! - **`P2cPendingRequests`**: Power of Two Choices algorithm using pending request count as load metric
//! - **`P2cPeakEwma`**: Power of Two Choices algorithm using peak EWMA latency as load metric
//!
//! ```rust,no_run
//! # use axum_reverse_proxy::{DiscoverableBalancedProxy, LoadBalancingStrategy};
//! # use hyper_util::client::legacy::{connect::HttpConnector, Client};
//! # use futures_util::stream::Stream;
//! # use tower::discover::Change;
//! # use std::pin::Pin;
//! # use std::task::{Context, Poll};
//! # #[derive(Clone)]
//! # struct MyDiscoveryStream;
//! # impl Stream for MyDiscoveryStream {
//! #     type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;
//! #     fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//! #         Poll::Pending
//! #     }
//! # }
//! # let discovery = MyDiscoveryStream;
//! # let connector = HttpConnector::new();
//! # let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);
//!
//! // Round-robin (default)
//! let proxy_rr = DiscoverableBalancedProxy::new_with_client("/api", client.clone(), discovery.clone());
//!
//! // P2C with pending requests load measurement
//! let proxy_p2c = DiscoverableBalancedProxy::new_with_client_and_strategy(
//!     "/api", client, discovery, LoadBalancingStrategy::P2cPendingRequests
//! );
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
//!
//! # TLS Configuration
//!
//! By default, this library uses [rustls](https://github.com/rustls/rustls) for TLS connections,
//! which provides a pure-Rust, secure, and modern TLS implementation.
//!
//! ## Default TLS (rustls)
//!
//! ```toml
//! [dependencies]
//! axum-reverse-proxy = "1.0"
//! # or explicitly enable the default TLS feature
//! axum-reverse-proxy = { version = "1.0", features = ["tls"] }
//! ```
//!
//! ## Using native-tls
//!
//! If you need to use the system's native TLS implementation (OpenSSL on Linux,
//! Secure Transport on macOS, SChannel on Windows), you can opt into the `native-tls` feature:
//!
//! ```toml
//! [dependencies]
//! axum-reverse-proxy = { version = "1.0", features = ["native-tls"] }
//! ```
//!
//! ## Feature Combinations
//!
//! - `default = ["tls"]` - Uses rustls (recommended)
//! - `features = ["native-tls"]` - Uses native TLS implementation
//! - `features = ["tls", "native-tls"]` - Both available, native-tls takes precedence
//! - `features = ["full"]` - Includes `tls` (rustls) and `dns` features
//! - `features = []` - No TLS support (HTTP only)
//!
//! **Note:** When both `tls` and `native-tls` features are enabled, `native-tls` takes precedence
//! since explicit selection of native-tls indicates a preference for the system's TLS implementation.
//! The `native-tls` feature is a separate opt-in and is not included in the `full` feature set.

mod balanced_proxy;
#[cfg(any(feature = "tls", feature = "native-tls"))]
mod danger;
#[cfg(feature = "dns")]
mod dns_discovery;
mod proxy;
mod retry;
mod rfc9110;
mod router;
mod websocket;

pub use balanced_proxy::BalancedProxy;
pub use balanced_proxy::DiscoverableBalancedProxy;
pub use balanced_proxy::LoadBalancingStrategy;
pub use balanced_proxy::StandardBalancedProxy;
pub use balanced_proxy::StandardDiscoverableBalancedProxy;
#[cfg(feature = "native-tls")]
pub use danger::create_dangerous_native_tls_connector;
#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub use danger::create_dangerous_rustls_config;
#[cfg(feature = "dns")]
pub use dns_discovery::{DnsDiscovery, DnsDiscoveryConfig, StaticDnsDiscovery};
pub use proxy::ReverseProxy;
pub use retry::RetryLayer;
pub use rfc9110::{Rfc9110Config, Rfc9110Layer};
