use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::Uri,
    response::IntoResponse,
    routing::get,
};
use axum_reverse_proxy::ReverseProxy;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tracing::{debug, info};
use tracing_subscriber::fmt::format::FmtSpan;

async fn websocket_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    // Echo server - just send back what we receive
    while let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            match msg {
                Message::Text(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Message::Binary(data) => {
                    if socket.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                _ => {}
            }
        } else {
            break;
        }
    }
}

async fn websocket_handler_with_path(
    ws: WebSocketUpgrade,
    uri: Uri,
    State(tx): State<tokio::sync::mpsc::Sender<String>>,
) -> impl IntoResponse {
    let _ = tx.send(uri.path().to_string()).await;
    ws.on_upgrade(handle_socket)
}

async fn setup_test_server(target_prefix: &str) -> (SocketAddr, SocketAddr) {
    // Set up logging for tests
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_test_writer()
        .try_init()
        .ok();

    // Create a WebSocket echo server (register with and without trailing slash)
    let app = Router::new()
        .route("/ws", get(websocket_handler))
        .route("/ws/", get(websocket_handler));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    info!("Upstream server listening on {}", upstream_addr);

    tokio::spawn(async move {
        axum::serve(upstream_listener, app).await.unwrap();
    });

    // Create the proxy server
    let proxy = ReverseProxy::new("/", format!("{target_prefix}{upstream_addr}"));
    let proxy_app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    info!("Proxy server listening on {}", proxy_addr);

    tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });

    (upstream_addr, proxy_addr)
}

#[tokio::test]
async fn test_websocket_upgrade() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Attempt WebSocket upgrade through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let _ws_client = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // If we get here, the upgrade was successful
}

#[tokio::test]
async fn test_websocket_echo() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Create a WebSocket client connection through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Send a test message
    let test_message = "Hello, WebSocket!";
    ws_stream
        .send(tungstenite::Message::Text(test_message.into()))
        .await
        .expect("Failed to send message");

    // Receive the echo response
    if let Some(msg) = ws_stream.next().await {
        let msg = msg.expect("Failed to get message");
        assert_eq!(msg, tungstenite::Message::Text(test_message.into()));
    } else {
        panic!("Did not receive response");
    }
}

#[tokio::test]
async fn test_websocket_path_query_join_parity() {
    use tokio::sync::mpsc;

    // Upstream echoes path via channel on upgrade
    let (tx, mut rx) = mpsc::channel::<String>(1);
    let app = Router::new()
        .route("/api/ws", get(websocket_handler_with_path))
        .with_state(tx);
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, app).await.unwrap();
    });

    // Mount mode
    let mount_proxy = ReverseProxy::new("/base", format!("http://{upstream_addr}/api"));
    let mount_app: Router = mount_proxy.into();
    let mount_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mount_addr = mount_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mount_listener, mount_app).await.unwrap();
    });

    // Connect via proxy with query-only remainder
    let url = format!("ws://127.0.0.1:{}/base/ws?foo=bar", mount_addr.port());
    let (_ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect via mount");
    let seen = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("no path received")
        .expect("channel closed");
    assert_eq!(seen, "/api/ws");

    // Fallback mode
    let (tx2, mut rx2) = mpsc::channel::<String>(1);
    let app2 = Router::new()
        .route("/api/ws", get(websocket_handler_with_path))
        .with_state(tx2.clone());
    let upstream_listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr2 = upstream_listener2.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener2, app2).await.unwrap();
    });

    let fb_proxy = ReverseProxy::new("/base", format!("http://{upstream_addr2}/api"));
    let fb_app = Router::new().fallback_service(fb_proxy);
    let fb_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fb_addr = fb_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(fb_listener, fb_app).await.unwrap();
    });

    let url2 = format!("ws://127.0.0.1:{}/base/ws?x=y", fb_addr.port());
    let (_ws2, _) = tokio_tungstenite::connect_async(&url2)
        .await
        .expect("ws connect via fallback");
    let seen2 = tokio::time::timeout(std::time::Duration::from_secs(2), rx2.recv())
        .await
        .expect("no path received")
        .expect("channel closed");
    assert_eq!(seen2, "/api/ws");
}

#[tokio::test]
async fn test_websocket_close() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Create a WebSocket client connection through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    info!("Connecting to WebSocket at {}", url);
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Close the connection
    ws_stream
        .close(None)
        .await
        .expect("Failed to close connection");

    // Wait for the close frame response
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    // Verify we don't receive any more messages
    assert!(ws_stream.next().await.is_none());
}

#[tokio::test]
async fn test_websocket_with_complex_connection_header() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Create a WebSocket client with a complex Connection header
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let url_parsed = url::Url::parse(&url).unwrap();
    let host = url_parsed.host_str().unwrap();
    let port = url_parsed.port().unwrap_or(80);
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };

    let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
        .uri(url)
        .header("Host", host_header)
        .header("Connection", "keep-alive, Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Version", "13")
        .body(())
        .unwrap();

    let (ws_stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("Failed to connect with complex Connection header");

    debug!("Successfully connected with complex Connection header");
    drop(ws_stream);
}

#[tokio::test]
async fn test_websocket_with_https_returns_error() {
    // This upstream speaks plaintext, so a TLS handshake must fail.
    // WebSockets now use the configured hyper client's TLS implementation.
    let (_upstream_addr, proxy_addr) = setup_test_server("https://").await;

    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let result = tokio_tungstenite::connect_async(&url).await;

    assert!(
        result.is_err(),
        "Expected error when using TLS against a plaintext upstream"
    );
    debug!("Correctly rejected TLS against a plaintext upstream");
}

#[tokio::test]
async fn test_websocket_with_explicit_ws() {
    let (_upstream_addr, proxy_addr) = setup_test_server("ws://").await;

    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect with explicit WS upstream");

    debug!("Successfully connected with explicit WS upstream");
    drop(ws_stream);
}

#[tokio::test]
async fn test_websocket_with_trailing_slash() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Test with a path that includes a trailing slash
    let url = format!("ws://127.0.0.1:{}/ws/", proxy_addr.port());
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect with trailing slash");

    debug!("Successfully connected with trailing slash");
    drop(ws_stream);
}

#[tokio::test]
async fn test_websocket_binary() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Create a WebSocket client connection through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Send a binary message
    let test_data = vec![1, 2, 3, 4, 5];
    ws_stream
        .send(tungstenite::Message::Binary(test_data.clone().into()))
        .await
        .expect("Failed to send binary message");

    // Receive the echo response
    if let Some(msg) = ws_stream.next().await {
        let msg = msg.expect("Failed to get message");
        assert_eq!(msg, tungstenite::Message::Binary(test_data.into()));
    } else {
        panic!("Did not receive response");
    }
}

#[tokio::test]
async fn test_websocket_ping_pong() {
    let (_upstream_addr, proxy_addr) = setup_test_server("http://").await;

    // Create a WebSocket client connection through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Send a ping frame and expect a pong response with the same payload
    let ping_payload = b"hello".to_vec();
    ws_stream
        .send(tungstenite::Message::Ping(ping_payload.clone().into()))
        .await
        .expect("Failed to send ping message");

    loop {
        if let Some(msg) = ws_stream.next().await {
            let msg = msg.expect("Failed to get message");
            match msg {
                tungstenite::Message::Pong(payload) => {
                    assert_eq!(payload, ping_payload);
                    break;
                }
                _ => continue,
            }
        } else {
            panic!("Did not receive pong response");
        }
    }

    // Send a pong frame which should be ignored by the server
    ws_stream
        .send(tungstenite::Message::Pong(Vec::new().into()))
        .await
        .expect("Failed to send pong message");

    // Ensure the connection stays open by sending another text message
    let text = "still alive";
    ws_stream
        .send(tungstenite::Message::Text(text.into()))
        .await
        .expect("Failed to send text message");

    loop {
        if let Some(msg) = ws_stream.next().await {
            let msg = msg.expect("Failed to get message");
            match msg {
                tungstenite::Message::Text(t) => {
                    assert_eq!(t, text);
                    break;
                }
                tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => {
                    continue;
                }
                other => panic!("Unexpected message: {other:?}"),
            }
        } else {
            panic!("Connection closed unexpectedly");
        }
    }
}

/// Upstream WebSocket handler that reports every `host` header value it
/// received on the upgrade handshake over a channel, then echoes frames.
async fn ws_host_capture_handler(
    ws: WebSocketUpgrade,
    State(tx): State<tokio::sync::mpsc::Sender<Vec<String>>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let hosts: Vec<String> = headers
        .get_all("host")
        .iter()
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .collect();
    let _ = tx.send(hosts).await;
    ws.on_upgrade(handle_socket)
}

/// Stand up a host-capturing upstream and a proxy with `policy`, returning
/// (upstream_addr, proxy_addr, host receiver).
async fn setup_ws_host_policy_proxy(
    policy: axum_reverse_proxy::ProxyPolicy,
) -> (
    SocketAddr,
    SocketAddr,
    tokio::sync::mpsc::Receiver<Vec<String>>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<String>>(1);
    let app = Router::new()
        .route("/ws", get(ws_host_capture_handler))
        .with_state(tx);
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, app).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{upstream_addr}")).with_policy(policy);
    let proxy_app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });

    (upstream_addr, proxy_addr, rx)
}

/// Connect to the proxy with an explicit client `Host` header and return the
/// host header values the upstream saw on the handshake.
async fn upstream_ws_hosts_seen(
    proxy_addr: SocketAddr,
    client_host: &str,
    rx: &mut tokio::sync::mpsc::Receiver<Vec<String>>,
) -> Vec<String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("host", client_host.parse().unwrap());

    let (_ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect via proxy");

    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("no host captured before timeout")
        .expect("host channel closed")
}

#[tokio::test]
async fn websocket_host_header_with_default_policy_replaces_host_with_upstream_authority() {
    use axum_reverse_proxy::ProxyPolicy;

    let (upstream_addr, proxy_addr, mut rx) =
        setup_ws_host_policy_proxy(ProxyPolicy::default()).await;

    let hosts = upstream_ws_hosts_seen(proxy_addr, "client.example.com", &mut rx).await;

    // Exactly one Host header (regression guard against duplicate emission)...
    assert_eq!(
        hosts.len(),
        1,
        "expected exactly one host header, got {hosts:?}"
    );
    // ...set to the upstream authority under the default Replace behaviour.
    assert_eq!(hosts[0], upstream_addr.to_string());
}

#[tokio::test]
async fn websocket_host_header_with_preserve_policy_forwards_single_client_host() {
    use axum_reverse_proxy::{HostBehaviour, ProxyPolicy};

    let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
    let (_upstream_addr, proxy_addr, mut rx) = setup_ws_host_policy_proxy(policy).await;

    let hosts = upstream_ws_hosts_seen(proxy_addr, "client.example.com", &mut rx).await;

    // Still exactly one Host header — Preserve must not also append the
    // upstream authority, which would emit a duplicate.
    assert_eq!(
        hosts.len(),
        1,
        "expected exactly one host header, got {hosts:?}"
    );
    assert_eq!(hosts[0], "client.example.com");
}

/// Close frames must be forwarded with their original code and reason
/// (regression test — the proxy used to replace them with an empty close).
#[tokio::test]
async fn test_websocket_close_code_forwarded() {
    use axum::extract::ws::CloseFrame;

    // Upstream server that closes immediately with a specific code and reason.
    async fn closing_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 4001,
                    reason: "going away for tests".into(),
                })))
                .await;
        })
    }

    let app = Router::new().route("/ws", get(closing_handler));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, app).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{upstream_addr}"));
    let proxy_app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });

    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // The first (and only) frame should be the upstream's close, verbatim.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
        .await
        .expect("timed out waiting for close frame")
        .expect("connection ended without a close frame")
        .expect("websocket error");

    match msg {
        tungstenite::Message::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 4001);
            assert_eq!(frame.reason, "going away for tests");
        }
        other => panic!("expected close frame with code/reason, got {other:?}"),
    }
}

/// A custom transport redirects an otherwise unreachable authority. Both the
/// handshake and subsequent frames must use that connection.
#[tokio::test]
async fn websocket_uses_custom_connector_and_selected_subprotocol() {
    use hyper_util::{
        client::legacy::Client,
        rt::{TokioExecutor, TokioIo},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async { ws.protocols(["chat"]).on_upgrade(handle_socket) }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let connector = tower::service_fn(move |uri: Uri| {
        assert_eq!(uri.host(), Some("unused.invalid"));
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::net::TcpStream::connect(address)
                .await
                .map(TokioIo::new)
        })
    });
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let proxy = ReverseProxy::new_with_client("/", "http://unused.invalid", client);
    let app: Router = proxy.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "other, chat".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "chat");
    socket
        .send(tungstenite::Message::Text("via connector".into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(message, tungstenite::Message::Text("via connector".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(socket);
    proxy_server.abort();
    upstream.abort();
}

/// A private CA trusted only by the configured client must also work for WSS.
#[cfg(all(feature = "tls", not(feature = "native-tls")))]
#[tokio::test]
async fn websocket_uses_configured_tls_trust() {
    use hyper_util::{
        client::legacy::Client,
        rt::{TokioExecutor, TokioIo},
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    use tower::ServiceExt;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let certificate = CertificateDer::from(cert.cert);
    let key = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async { ws.on_upgrade(handle_socket) }),
        );
        hyper::server::conn::http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                hyper::service::service_fn(move |req| {
                    let app = app.clone();
                    async move { app.oneshot(req).await }
                }),
            )
            .with_upgrades()
            .await
            .unwrap();
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let proxy =
        ReverseProxy::new_with_client("/", format!("https://localhost:{}", address.port()), client);
    let app: Router = proxy.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::connect_async(format!("ws://{address}/ws")),
    )
    .await
    .unwrap()
    .unwrap();
    socket
        .send(tungstenite::Message::Text("private TLS".into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echoed, tungstenite::Message::Text("private TLS".into()));
    drop(socket);
    proxy_server.abort();
    upstream.abort();
}
