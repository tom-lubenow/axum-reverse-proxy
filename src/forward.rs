//! Shared request forwarding logic used by both `ReverseProxy` and `ProxyRouterExt`.

use axum::body::Body;
use axum::extract::ConnectInfo;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Request, Response, StatusCode, Uri};
#[cfg(all(feature = "tls", not(feature = "native-tls")))]
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
#[cfg(feature = "native-tls")]
use hyper_tls::HttpsConnector as NativeTlsHttpsConnector;
use hyper_util::client::legacy::{Client, connect::Connect, connect::HttpConnector};
use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use tracing::{error, trace};

use crate::{
    proxy::{ProxyPolicy, XForwardedFor},
    websocket,
};

#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub(crate) type ProxyClient = Client<HttpsConnector<HttpConnector>, Body>;

#[cfg(feature = "native-tls")]
pub(crate) type ProxyClient = Client<NativeTlsHttpsConnector<HttpConnector>, Body>;

#[cfg(all(not(feature = "tls"), not(feature = "native-tls")))]
pub(crate) type ProxyClient = Client<HttpConnector, Body>;

#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub(crate) type ProxyConnector = HttpsConnector<HttpConnector>;

#[cfg(feature = "native-tls")]
pub(crate) type ProxyConnector = NativeTlsHttpsConnector<HttpConnector>;

#[cfg(all(not(feature = "tls"), not(feature = "native-tls")))]
pub(crate) type ProxyConnector = HttpConnector;

/// Standard hop-by-hop headers (RFC 9110 §7.6.1). These are meaningful only
/// for a single connection and are never forwarded. `te` is special-cased in
/// [`hop_by_hop_headers`]: the value `trailers` is preserved because gRPC
/// requires it end-to-end.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// Error details for a request the proxy failed to forward.
///
/// When forwarding fails, the proxy responds with a generic `502 Bad Gateway`
/// (the underlying error text is logged but not exposed to the client) and
/// inserts a `ProxyError` into the response's
/// [extensions](http::Response::extensions). Middleware such as
/// [`RetryLayer`](crate::RetryLayer) uses it to distinguish a failure to reach
/// the upstream from a genuine 502 returned *by* the upstream.
#[derive(Clone, Debug)]
pub struct ProxyError {
    message: String,
    is_connect: bool,
}

impl ProxyError {
    /// Whether the error occurred while establishing the upstream connection,
    /// meaning the request was never sent and is safe to retry regardless of
    /// method.
    pub fn is_connect(&self) -> bool {
        self.is_connect
    }

    /// The underlying error message. This may contain internal details
    /// (addresses, hostnames); expose it to clients deliberately, if at all.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Create an HTTP connector configured for proxying with standard settings.
///
/// This is the shared connector configuration used by all proxy types.
pub(crate) fn create_http_connector() -> ProxyConnector {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.enforce_http(false);
    connector.set_keepalive(Some(std::time::Duration::from_secs(60)));
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
    connector.set_reuse_address(true);

    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_all_versions()
        .wrap_connector(connector);

    #[cfg(feature = "native-tls")]
    let connector = NativeTlsHttpsConnector::new_with_connector(connector);

    connector
}

/// Create a new HTTP client configured for proxying.
pub(crate) fn create_proxy_client() -> ProxyClient {
    Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .retry_canceled_requests(true)
        .set_host(true)
        .build(create_http_connector())
}

/// Compute the set of hop-by-hop headers to strip: the standard set plus any
/// header nominated by the `Connection` header (RFC 9110 §7.6.1).
///
/// `te: trailers` is preserved (gRPC requires it to reach the server).
fn hop_by_hop_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut strip: HashSet<HeaderName> = HOP_BY_HOP_HEADERS
        .iter()
        .map(|name| HeaderName::from_static(name))
        .collect();

    if let Some(te) = headers.get("te")
        && te
            .to_str()
            .map(|v| v.trim().eq_ignore_ascii_case("trailers"))
            .unwrap_or(false)
    {
        strip.remove(&HeaderName::from_static("te"));
    }

    for connection in headers.get_all(http::header::CONNECTION) {
        if let Ok(connection_str) = connection.to_str() {
            for nominated in connection_str.split(',') {
                if let Ok(name) = HeaderName::try_from(nominated.trim()) {
                    strip.insert(name);
                }
            }
        }
    }

    strip
}

/// Strip hop-by-hop headers from a response's header map in place.
fn strip_hop_by_hop_response_headers(headers: &mut HeaderMap) {
    for name in hop_by_hop_headers(headers) {
        headers.remove(&name);
    }
}

/// Build the header map to send upstream: client headers minus hop-by-hop
/// headers, with `Host` and `X-Forwarded-*` handled per policy.
fn build_forward_headers(
    client_headers: &HeaderMap,
    client_addr: Option<SocketAddr>,
    policy: &ProxyPolicy,
) -> HeaderMap {
    let strip = hop_by_hop_headers(client_headers);
    let mut headers = HeaderMap::with_capacity(client_headers.len());

    for (key, value) in client_headers.iter() {
        // When the policy replaces the host, drop the client's host so the
        // hyper client backfills it from the upstream authority.
        if key == http::header::HOST && !policy.forwards_client_host() {
            continue;
        }
        if strip.contains(key) {
            continue;
        }
        headers.append(key.clone(), value.clone());
    }

    if policy.x_forwarded_for == XForwardedFor::Append
        && let Some(addr) = client_addr
    {
        let client_ip = addr.ip().to_string();
        let x_forwarded_for = HeaderName::from_static("x-forwarded-for");
        let existing: Vec<String> = headers
            .get_all(&x_forwarded_for)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        let combined = if existing.is_empty() {
            client_ip
        } else {
            format!("{}, {}", existing.join(", "), client_ip)
        };
        if let Ok(value) = HeaderValue::from_str(&combined) {
            headers.insert(x_forwarded_for, value);
        }

        let x_forwarded_host = HeaderName::from_static("x-forwarded-host");
        if !headers.contains_key(&x_forwarded_host)
            && let Some(host) = client_headers.get(http::header::HOST)
        {
            headers.insert(x_forwarded_host, host.clone());
        }
    }

    headers
}

/// Forward a request to the given upstream URI.
///
/// Handles both regular HTTP requests and WebSocket upgrades. Hop-by-hop
/// headers (RFC 9110 §7.6.1) are stripped in both directions.
/// This is generic over the client connector type to support different client configurations.
pub(crate) async fn forward_request<C>(
    upstream_uri: Uri,
    req: Request<Body>,
    client: &Client<C, Body>,
    policy: &ProxyPolicy,
) -> Result<Response<Body>, Infallible>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    trace!(
        "Forwarding request method={} to={}",
        req.method(),
        upstream_uri
    );

    // Check if this is a WebSocket upgrade request
    if websocket::is_websocket_upgrade(req.headers()) {
        trace!("Detected WebSocket upgrade request");
        match websocket::handle_websocket_with_upstream_uri(req, upstream_uri, policy).await {
            Ok(response) => return Ok(response),
            Err(e) => {
                error!("Failed to handle WebSocket upgrade: {}", e);
                let mut response = Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("WebSocket upgrade failed"))
                    .unwrap();
                response.extensions_mut().insert(ProxyError {
                    message: e.to_string(),
                    is_connect: false,
                });
                return Ok(response);
            }
        }
    }

    // Build the forwarding request
    let (parts, body) = req.into_parts();
    let client_addr = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    let headers = build_forward_headers(&parts.headers, client_addr, policy);

    let mut forward_req = Request::builder()
        .method(parts.method.clone())
        .uri(upstream_uri)
        .body(body)
        .unwrap();
    *forward_req.headers_mut() = headers;

    // Send the request
    match client.request(forward_req).await {
        Ok(res) => {
            trace!(
                "Received response status={} version={:?}",
                res.status(),
                res.version()
            );

            let (mut parts, body) = res.into_parts();
            strip_hop_by_hop_response_headers(&mut parts.headers);

            // Wrap the upstream body directly (rather than as a data stream)
            // so trailer frames survive — gRPC needs them.
            let mut response = Response::new(Body::new(body));
            *response.status_mut() = parts.status;
            *response.version_mut() = parts.version;
            *response.headers_mut() = parts.headers;
            Ok(response)
        }
        Err(e) => {
            error!(
                "Proxy error forwarding {} request: {}",
                parts.method,
                error_chain(&e)
            );
            let mut response = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .unwrap();
            response.extensions_mut().insert(ProxyError {
                message: error_chain(&e),
                is_connect: e.is_connect(),
            });
            Ok(response)
        }
    }
}

/// Render an error with its source chain, since the top-level legacy client
/// error is often just "error trying to connect".
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{HostBehaviour, ProxyPolicy};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn strips_standard_and_nominated_hop_by_hop_headers() {
        let client_headers = headers(&[
            ("connection", "close, x-internal"),
            ("keep-alive", "timeout=5"),
            ("x-internal", "secret"),
            ("x-app", "value"),
        ]);
        let forwarded = build_forward_headers(&client_headers, None, &ProxyPolicy::default());
        assert!(!forwarded.contains_key("connection"));
        assert!(!forwarded.contains_key("keep-alive"));
        assert!(!forwarded.contains_key("x-internal"));
        assert_eq!(forwarded.get("x-app").unwrap(), "value");
    }

    #[test]
    fn preserves_te_trailers_for_grpc() {
        let client_headers = headers(&[("te", "trailers"), ("content-type", "application/grpc")]);
        let forwarded = build_forward_headers(&client_headers, None, &ProxyPolicy::default());
        assert_eq!(forwarded.get("te").unwrap(), "trailers");

        let client_headers = headers(&[("te", "gzip")]);
        let forwarded = build_forward_headers(&client_headers, None, &ProxyPolicy::default());
        assert!(!forwarded.contains_key("te"));
    }

    #[test]
    fn appends_client_ip_to_x_forwarded_for() {
        let client_headers = headers(&[("x-forwarded-for", "10.0.0.1"), ("host", "example.com")]);
        let addr: SocketAddr = "192.168.1.5:12345".parse().unwrap();
        let forwarded = build_forward_headers(&client_headers, Some(addr), &ProxyPolicy::default());
        assert_eq!(
            forwarded.get("x-forwarded-for").unwrap(),
            "10.0.0.1, 192.168.1.5"
        );
        assert_eq!(forwarded.get("x-forwarded-host").unwrap(), "example.com");
    }

    #[test]
    fn x_forwarded_for_preserve_mode_leaves_headers_untouched() {
        let client_headers = headers(&[("x-forwarded-for", "10.0.0.1")]);
        let addr: SocketAddr = "192.168.1.5:12345".parse().unwrap();
        let policy = ProxyPolicy::new().with_x_forwarded_for(XForwardedFor::Preserve);
        let forwarded = build_forward_headers(&client_headers, Some(addr), &policy);
        assert_eq!(forwarded.get("x-forwarded-for").unwrap(), "10.0.0.1");
        assert!(!forwarded.contains_key("x-forwarded-host"));
    }

    #[test]
    fn host_dropped_unless_preserved() {
        let client_headers = headers(&[("host", "client.example.com")]);
        let forwarded = build_forward_headers(&client_headers, None, &ProxyPolicy::default());
        assert!(!forwarded.contains_key("host"));

        let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
        let forwarded = build_forward_headers(&client_headers, None, &policy);
        assert_eq!(forwarded.get("host").unwrap(), "client.example.com");
    }
}
