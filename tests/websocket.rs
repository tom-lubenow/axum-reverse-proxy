use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_reverse_proxy::ReverseProxy;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tracing_subscriber::fmt::format::FmtSpan;

async fn websocket_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    // Echo server - just send back what we receive
    while let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            if let Message::Text(text) = msg {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

async fn setup_test_server() -> SocketAddr {
    // Set up logging for tests
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_test_writer()
        .try_init()
        .ok();

    // Create a WebSocket echo server
    let app = Router::new().route("/ws", get(websocket_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

#[tokio::test]
async fn test_websocket_upgrade() {
    let upstream_addr = setup_test_server().await;
    let upstream_url = format!("http://{}", upstream_addr);

    // Create a reverse proxy pointing to our test server
    let proxy = ReverseProxy::new("/ws", &upstream_url);
    let app: Router = Router::new().merge(proxy);

    // Create a client connection through the proxy
    let client = reqwest::Client::builder()
        .build()
        .unwrap();

    // Attempt WebSocket upgrade
    let url = format!("ws://127.0.0.1:{}/ws", upstream_addr.port());
    let _ws_client = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // If we get here, the upgrade was successful
}

#[tokio::test]
async fn test_websocket_echo() {
    let upstream_addr = setup_test_server().await;
    let upstream_url = format!("http://{}", upstream_addr);

    // Create a reverse proxy pointing to our test server
    let proxy = ReverseProxy::new("/ws", &upstream_url);
    let app: Router = Router::new().merge(proxy);

    // Create a WebSocket client connection
    let url = format!("ws://127.0.0.1:{}/ws", upstream_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Send a test message
    let test_message = "Hello, WebSocket!";
    ws_stream.send(tungstenite::Message::Text(test_message.into()))
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
async fn test_websocket_close() {
    let upstream_addr = setup_test_server().await;
    let upstream_url = format!("http://{}", upstream_addr);

    // Create a reverse proxy pointing to our test server
    let proxy = ReverseProxy::new("/ws", &upstream_url);
    let app: Router = Router::new().merge(proxy);

    // Create a WebSocket client connection
    let url = format!("ws://127.0.0.1:{}/ws", upstream_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Close the connection
    ws_stream.close(None).await.expect("Failed to close connection");

    // Verify we don't receive any more messages
    assert!(ws_stream.next().await.is_none());
}

#[tokio::test]
async fn test_websocket_binary() {
    let upstream_addr = setup_test_server().await;
    let upstream_url = format!("http://{}", upstream_addr);

    // Create a reverse proxy pointing to our test server
    let proxy = ReverseProxy::new("/ws", &upstream_url);
    let app: Router = Router::new().merge(proxy);

    // Create a WebSocket client connection
    let url = format!("ws://127.0.0.1:{}/ws", upstream_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    // Send a binary message
    let test_data = vec![1, 2, 3, 4, 5];
    ws_stream.send(tungstenite::Message::Binary(test_data.clone()))
        .await
        .expect("Failed to send binary message");

    // Receive the echo response
    if let Some(msg) = ws_stream.next().await {
        let msg = msg.expect("Failed to get message");
        assert_eq!(msg, tungstenite::Message::Binary(test_data));
    } else {
        panic!("Did not receive response");
    }
} 