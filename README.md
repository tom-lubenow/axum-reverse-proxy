# axum-reverse-proxy

[![CI](https://github.com/tom-lubenow/axum-reverse-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/tom-lubenow/axum-reverse-proxy/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/axum-reverse-proxy.svg)](https://crates.io/crates/axum-reverse-proxy)
[![Documentation](https://docs.rs/axum-reverse-proxy/badge.svg)](https://docs.rs/axum-reverse-proxy)

A flexible and efficient reverse proxy implementation for [Axum](https://github.com/tokio-rs/axum) web applications. This library provides a simple way to forward HTTP requests from your Axum application to upstream servers. It is intended to be a simple implementation sitting on top of axum and hyper.

The eventual goal would be to benchmark ourselves against common reverse proxy libraries like nginx, traefik, haproxy, etc. We hope to achieve comparable (or better) performance but with significantly better developer ergonomics, using Rust code to configure the proxy instead of various configuration files with their own DSLs.

## Features

- 🛣 Path-based routing
- 🔄 Optional retry mechanism with configurable delay
- 📨 Header forwarding (with host header management)
- ⚙ Configurable HTTP client settings
- 🔀 Round-robin load balancing across multiple upstreams
- 🔌 Easy integration with Axum's Router
- 🧰 Custom client configuration support
- 🔒 HTTPS support
- 📋 Optional RFC9110 compliance layer
- 🔧 Full Tower middleware support

## Installation

Run `cargo add axum-reverse-proxy` to add the library to your project.

### Cargo Features

This crate comes with a couple of optional features:

- `tls` – enables HTTPS support via `hyper-rustls` (this is on by default)
- `dns` – enables DNS-based service discovery
- `full` – enables all features (`tls` and `dns`)

To enable features explicitly in `Cargo.toml`:

```toml
[dependencies]
axum-reverse-proxy = { version = "*", features = ["dns"] }
```

## Quick Start

Here's a simple example that forwards all requests under `/api` to `httpbin.org`:

```rust
use axum::Router;
use axum_reverse_proxy::ReverseProxy;

// Create a reverse proxy that forwards requests from /api to httpbin.org
let proxy = ReverseProxy::new("/api", "https://httpbin.org");

// Convert the proxy to a router and use it in your Axum application
let app: Router = proxy.into();
```


### Load Balanced Upstreams

```rust
use axum::Router;
use axum_reverse_proxy::BalancedProxy;

let proxy = BalancedProxy::new("/api", vec!["https://api1.example.com", "https://api2.example.com"]);
let app: Router = proxy.into();
```
### Discoverable Balanced Proxy and DNS Discovery

`DiscoverableBalancedProxy` works with any [`tower::discover::Discover`] implementation to update its upstream list at runtime. The crate provides `DnsDiscovery` and `StaticDnsDiscovery` (requires the `dns` feature) for automatic resolution of hostnames.

`DnsDiscovery` periodically resolves a hostname, while `StaticDnsDiscovery` performs a single lookup at startup.

```rust,no_run
use axum::Router;
use axum_reverse_proxy::{DiscoverableBalancedProxy, DnsDiscovery, DnsDiscoveryConfig};
use std::time::Duration;

let dns_config = DnsDiscoveryConfig::new("api.example.com", 443)
    .with_https(true)
    .with_refresh_interval(Duration::from_secs(30));
let discovery = DnsDiscovery::new(dns_config).unwrap();

let mut proxy = DiscoverableBalancedProxy::new("/api", discovery);
proxy.start_discovery().await;

let app: Router = Router::new().nest_service("/", proxy);
```


## Advanced Usage

### Using Tower Middleware

The proxy integrates seamlessly with Tower middleware. Common use cases include:

- Authentication and authorization
- Rate limiting
- Request validation
- Logging and tracing
- Timeouts and retries
- Caching
- Compression
- Request buffering (via tower-buffer)

Example using tower-buffer for request buffering:

```rust
use axum::Router;
use axum_reverse_proxy::ReverseProxy;
use tower::ServiceBuilder;
use tower_buffer::BufferLayer;

let proxy = ReverseProxy::new("/api", "https://api.example.com");
let app: Router = proxy.into();


// Add buffering middleware
let app = app.layer(ServiceBuilder::new().layer(BufferLayer::new(100)));
```

### Path Transform Semantics

This crate follows clear rules for joining the incoming request path with the configured base path and the upstream target:

- Base terms: `B` = configured base path, `T` = upstream target (may include a path), `P?Q` = incoming path+query.
- Trim target trailing slash: treat `T` with no trailing `/` for joining; if `T` has no path, it is treated as empty (`/` is used when necessary for URI validity).
- Boundary-aware base stripping: strip `B` only when `P` is exactly `B` or starts with `B/`. Never strip similar prefixes (e.g., `B=/api` doesn’t strip `/apiary`).
- Nested vs fallback: when mounted via `Router::nest(B, ...)`, Axum already strips `B` before your service sees the request. The proxy detects this and handles both mounting styles consistently.
- Empty remainder: if the remainder is effectively empty (e.g., `P` is `/` under a non-empty `B`), the proxy does not insert an extra slash before a query string. Example: `/base?x=1` → `T?x=1` (not `T/?x=1`).
- Join: append the remainder to `T`’s path verbatim; do not re-encode segments; preserve case and interior slashes.
- Query: append `?Q` exactly as received; the presence of a query does not force a `/` when the remainder is empty.
- Dot-segments: normalization is left to underlying HTTP stacks; the proxy does not add its own normalization.
- WebSocket: the same path+query join logic applies; schemes are mapped `http→ws` and `https→wss`.

These rules are enforced by a single code path that builds the upstream URI using `http::Uri` to avoid stringly-typed errors. See tests under `tests/routing.rs` and `tests/websocket.rs` for coverage of edge cases (queries, trailing slashes, boundary stripping, mount vs fallback, and WebSocket parity).

### Merging with Existing Router

You can merge the proxy with your existing Axum router:

```rust
use axum::{routing::get, Router, response::IntoResponse, extract::State};
use axum_reverse_proxy::ReverseProxy;

#[derive(Clone)]
struct AppState { foo: usize }

async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
    (axum::http::StatusCode::OK, format!("Hello, World! {}", state.foo))
}

let app: Router<AppState> = Router::new()
    .route("/", get(root_handler))
    .merge(ReverseProxy::new("/api", "https://httpbin.org"))
    .with_state(AppState { foo: 42 });
```

## RFC9110 Compliance

The library includes an optional RFC9110 compliance layer that implements key requirements from [RFC9110 (HTTP Semantics)](https://www.rfc-editor.org/rfc/rfc9110.html). To use it:

```rust
use axum_reverse_proxy::{ReverseProxy, Rfc9110Config, Rfc9110Layer};
use std::collections::HashSet;

// Create a config for RFC9110 compliance
let mut server_names = HashSet::new();
server_names.insert("example.com".to_string());

let config = Rfc9110Config {
    server_names: Some(server_names),  // For loop detection
    pseudonym: Some("myproxy".to_string()),  // For Via headers
    combine_via: true,  // Combine Via headers with same protocol
    preserve_websocket_headers: true,  // Preserve WebSocket upgrade headers
};

// Create a proxy with RFC9110 compliance
let proxy = ReverseProxy::new("/api", "https://api.example.com")
    .layer(Rfc9110Layer::with_config(config));
```

The RFC9110 layer provides:

- **Connection Header Processing**: Properly handles Connection headers and removes hop-by-hop headers
- **Via Header Management**: Adds and combines Via headers according to spec, with optional firewall mode
- **Max-Forwards Processing**: Handles Max-Forwards header for TRACE/OPTIONS methods
- **Loop Detection**: Detects request loops using Via headers and server names
- **End-to-end Header Preservation**: Preserves end-to-end headers while removing hop-by-hop headers

### Custom Client Configuration

For more control over the HTTP client behavior:

```rust
use axum_reverse_proxy::ReverseProxy;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use axum::body::Body;

let mut connector = HttpConnector::new();
connector.set_nodelay(true);
connector.enforce_http(false);
connector.set_keepalive(Some(std::time::Duration::from_secs(60)));

let client = Client::builder(hyper_util::rt::TokioExecutor::new())
    .pool_idle_timeout(std::time::Duration::from_secs(60))
    .pool_max_idle_per_host(32)
    .build(connector);

let proxy = ReverseProxy::new_with_client("/api", "https://api.example.com", client);
```

## Configuration

The default configuration includes:
- 60-second keepalive timeout
- 10-second connect timeout
- TCP nodelay enabled
- Connection pooling with 32 idle connections per host
- Automatic host header management

### TLS Configuration

By default, this library uses [rustls](https://github.com/rustls/rustls) for TLS connections, which provides a pure-Rust, secure, and modern TLS implementation.

#### Default TLS (rustls)

```toml
[dependencies]
axum-reverse-proxy = "1.0"
# or explicitly enable the default TLS feature
axum-reverse-proxy = { version = "1.0", features = ["tls"] }
```

#### Using native-tls

If you need to use the system's native TLS implementation (OpenSSL on Linux, Secure Transport on macOS, SChannel on Windows), you can opt into the `native-tls` feature:

```toml
[dependencies]
axum-reverse-proxy = { version = "1.0", features = ["native-tls"] }
```

#### Feature Combinations

- `default = ["tls"]` - Uses rustls (recommended)
- `features = ["native-tls"]` - Uses native TLS implementation
- `features = ["tls", "native-tls"]` - Both available, native-tls takes precedence
- `features = ["full"]` - Includes `tls` (rustls) and `dns` features
- `features = []` - No TLS support (HTTP only)

**Note:** When both `tls` and `native-tls` features are enabled, `native-tls` takes precedence since explicit selection of native-tls indicates a preference for the system's TLS implementation. The `native-tls` feature is a separate opt-in and is not included in the `full` feature set.

## Examples

Check out the [examples](examples/) directory for more usage examples:

- [Basic Proxy](examples/basic.rs) - Shows how to set up a basic reverse proxy with path-based routing
- [Retry Proxy](examples/retry.rs) - Demonstrates enabling retries via `RetryLayer`
- [Balanced Proxy](examples/balanced.rs) - Forward to multiple upstream servers with round-robin load balancing
- **Note:** very large requests may still need buffering depending on the body wrapper's strategy.

## Development

### Running Tests

This project uses [cargo-nextest](https://nexte.st/) for faster test execution. To install nextest:

```bash
# Install pre-built binary (recommended)
cargo install cargo-nextest --locked
```

To run tests:

```bash
# Run all tests
cargo nextest run

# Run tests with all features enabled
cargo nextest run --features full

# Run doctests separately (nextest doesn't support doctests yet)
cargo test --doc
```

### CI Configuration

The CI pipeline automatically runs tests using nextest. See `.github/workflows/ci.yml` for the full configuration.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
