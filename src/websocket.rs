use axum::{body::Body, http::Request, response::Response};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{stream::StreamExt, SinkExt};
use http::{HeaderMap, HeaderValue, StatusCode};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message},
};
use tracing::{error, trace};
use url::Url;

/// Check if a request is a WebSocket upgrade request by examining the headers.
///
/// According to the WebSocket protocol specification (RFC 6455), a WebSocket upgrade request must have:
/// - An "Upgrade: websocket" header (case-insensitive)
/// - A "Connection: Upgrade" header (case-insensitive)
/// - A "Sec-WebSocket-Key" header with a base64-encoded 16-byte value
/// - A "Sec-WebSocket-Version" header
pub(crate) fn is_websocket_upgrade(headers: &HeaderMap<HeaderValue>) -> bool {
    // Check for required WebSocket upgrade headers
    let has_upgrade = headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    let has_connection = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    let has_websocket_key = headers.contains_key("sec-websocket-key");
    let has_websocket_version = headers.contains_key("sec-websocket-version");

    trace!("is_websocket_upgrade - upgrade: {has_upgrade}, connection: {has_connection}, websocket key: {has_websocket_key}, websocket version: {has_websocket_version}");
    has_upgrade && has_connection && has_websocket_key && has_websocket_version
}

#[cfg(test)]
pub(crate) fn compute_host_header(url: &str) -> (String, u16) {
    let url = Url::parse(url).unwrap();
    let scheme = url.scheme();
    let host = url.host_str().unwrap();
    let port = match url.port() {
        Some(p) => p,
        None => {
            if scheme == "wss" {
                443
            } else {
                80
            }
        }
    };
    let header = if (scheme == "wss" && port == 443) || (scheme == "ws" && port == 80) {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };
    (header, port)
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
pub(crate) async fn handle_websocket(
    req: Request<Body>,
    target: &str,
) -> Result<Response<Body>, Box<dyn std::error::Error + Send + Sync>> {
    trace!("Handling WebSocket upgrade request");

    // Get the WebSocket key before upgrading
    let ws_key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|key| key.to_str().ok())
        .ok_or("Missing or invalid Sec-WebSocket-Key header")?;

    // Calculate the WebSocket accept key
    let mut hasher = Sha1::new();
    hasher.update(ws_key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let ws_accept = STANDARD.encode(hasher.finalize());

    // Get the path and query from the request
    let path_and_query = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("");

    trace!("Original path: {}", path_and_query);

    // Convert the target URL to WebSocket URL
    let upstream_url = if target.starts_with("ws://") || target.starts_with("wss://") {
        format!("{}{}", target, path_and_query)
    } else {
        let (scheme, rest) = if target.starts_with("https://") {
            ("wss://", target.trim_start_matches("https://"))
        } else {
            ("ws://", target.trim_start_matches("http://"))
        };
        format!("{}{}{}", scheme, rest.trim_end_matches('/'), path_and_query)
    };

    trace!("Connecting to upstream WebSocket at {}", upstream_url);

    // Parse the URL to get the host and scheme
    let url = Url::parse(&upstream_url)?;
    let scheme = url.scheme();
    let host = url.host_str().ok_or("Missing host in URL")?;
    let port = match url.port() {
        Some(p) => p,
        None => {
            if scheme == "wss" {
                443
            } else {
                80
            }
        }
    };
    let host_header = if (scheme == "wss" && port == 443) || (scheme == "ws" && port == 80) {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    // Forward all headers except host to upstream
    let mut request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
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
        match handle_websocket_connection(req, request).await {
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

#[cfg(test)]
mod tests {
    use super::{compute_host_header, is_websocket_upgrade};
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn host_header_ws_default_port() {
        let (host, port) = compute_host_header("ws://example.com/path");
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn host_header_wss_default_port() {
        let (host, port) = compute_host_header("wss://example.com/path");
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn host_header_wss_custom_port() {
        let (host, port) = compute_host_header("wss://example.com:8443/path");
        assert_eq!(host, "example.com:8443");
        assert_eq!(port, 8443);
    }

    #[test]
    fn websocket_upgrade_valid_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("Upgrade", HeaderValue::from_static("websocket"));
        headers.insert(
            "Connection",
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        headers.insert(
            "Sec-WebSocket-Key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("Sec-WebSocket-Version", HeaderValue::from_static("13"));

        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn websocket_upgrade_missing_upgrade_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("Upgrade"));
        headers.insert(
            "Sec-WebSocket-Key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("Sec-WebSocket-Version", HeaderValue::from_static("13"));

        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn websocket_upgrade_invalid_connection_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Upgrade", HeaderValue::from_static("websocket"));
        headers.insert("Connection", HeaderValue::from_static("keep-alive"));
        headers.insert(
            "Sec-WebSocket-Key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("Sec-WebSocket-Version", HeaderValue::from_static("13"));

        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn websocket_upgrade_missing_key_or_version() {
        let mut headers = HeaderMap::new();
        headers.insert("Upgrade", HeaderValue::from_static("websocket"));
        headers.insert("Connection", HeaderValue::from_static("Upgrade"));

        // Missing Sec-WebSocket-Key
        headers.insert("Sec-WebSocket-Version", HeaderValue::from_static("13"));
        assert!(!is_websocket_upgrade(&headers));

        headers.insert(
            "Sec-WebSocket-Key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.remove("Sec-WebSocket-Version");
        assert!(!is_websocket_upgrade(&headers));
    }
}
