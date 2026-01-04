//! Shared request forwarding logic used by both `ReverseProxy` and `ProxyRouterExt`.

use axum::body::Body;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::BodyExt;
#[cfg(all(feature = "tls", not(feature = "native-tls")))]
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
#[cfg(feature = "native-tls")]
use hyper_tls::HttpsConnector as NativeTlsHttpsConnector;
use hyper_util::client::legacy::{Client, connect::Connect, connect::HttpConnector};
use std::convert::Infallible;
use tracing::{error, trace};

use crate::websocket;

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
        .enable_http1()
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

/// Forward a request to the given upstream URI.
///
/// Handles both regular HTTP requests and WebSocket upgrades.
/// This is generic over the client connector type to support different client configurations.
pub(crate) async fn forward_request<C>(
    upstream_uri: Uri,
    req: Request<Body>,
    client: &Client<C, Body>,
) -> Result<Response<Body>, Infallible>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    trace!(
        "Forwarding request method={} to={}",
        req.method(),
        upstream_uri
    );
    trace!("Original headers: {:?}", req.headers());

    // Check if this is a WebSocket upgrade request
    if websocket::is_websocket_upgrade(req.headers()) {
        trace!("Detected WebSocket upgrade request");
        match websocket::handle_websocket_with_upstream_uri(req, upstream_uri).await {
            Ok(response) => return Ok(response),
            Err(e) => {
                error!("Failed to handle WebSocket upgrade: {}", e);
                return Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from(format!("WebSocket upgrade failed: {e}")))
                    .unwrap());
            }
        }
    }

    // Build the forwarding request
    let forward_req = {
        let mut builder = Request::builder()
            .method(req.method().clone())
            .uri(upstream_uri);

        // Forward headers (except host, which will be set by the client)
        for (key, value) in req.headers() {
            if key != "host" {
                builder = builder.header(key, value);
            }
        }

        let (_, body) = req.into_parts();
        builder.body(body).unwrap()
    };

    trace!("Forwarding headers: {:?}", forward_req.headers());

    // Send the request
    match client.request(forward_req).await {
        Ok(res) => {
            trace!(
                "Received response status={} headers={:?} version={:?}",
                res.status(),
                res.headers(),
                res.version()
            );

            let (parts, body) = res.into_parts();
            let body = Body::from_stream(body.into_data_stream());

            let mut response = Response::new(body);
            *response.status_mut() = parts.status;
            *response.version_mut() = parts.version;
            *response.headers_mut() = parts.headers;
            Ok(response)
        }
        Err(e) => {
            let error_msg = e.to_string();
            error!("Proxy error: {}", error_msg);
            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!(
                    "Failed to connect to upstream server: {error_msg}"
                )))
                .unwrap())
        }
    }
}
