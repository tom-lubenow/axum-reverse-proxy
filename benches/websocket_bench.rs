use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use axum_reverse_proxy::ReverseProxy;
use criterion::{Criterion, criterion_group, criterion_main};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite;
use tracing::error;

async fn websocket_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
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

async fn setup_test_server() -> (SocketAddr, SocketAddr) {
    // Create a WebSocket echo server
    let app = Router::new().route("/ws", get(websocket_handler));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(upstream_listener, app).await.unwrap();
    });

    // Create the proxy server
    let proxy = ReverseProxy::new("/", &format!("http://{upstream_addr}"));
    let proxy_app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });

    // Give the servers a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    (upstream_addr, proxy_addr)
}

async fn bench_websocket_echo(
    proxy_addr: SocketAddr,
    message_size: usize,
    iterations: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Create a WebSocket client connection through the proxy
    let url = format!("ws://127.0.0.1:{}/ws", proxy_addr.port());
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url).await?;

    // Create a test message of the specified size
    let test_message = "x".repeat(message_size);

    for _ in 0..iterations {
        // Send the test message
        ws_stream
            .send(tungstenite::Message::Text(test_message.clone().into()))
            .await?;

        // Receive the echo response
        match ws_stream.next().await {
            Some(Ok(_)) => (),
            Some(Err(e)) => return Err(Box::new(e)),
            None => return Err("Connection closed unexpectedly".into()),
        }
    }

    // Close the connection
    ws_stream.close(None).await?;
    Ok(())
}

fn websocket_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Set up the test server
    let (_upstream_addr, proxy_addr) = rt.block_on(setup_test_server());

    // Benchmark different message sizes
    let message_sizes = [10, 100, 1000, 10000];
    let mut group = c.benchmark_group("websocket_echo");
    for size in message_sizes {
        group.bench_function(format!("{size}_bytes"), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        bench_websocket_echo(proxy_addr, size, 100).await.unwrap();
                    }
                    start.elapsed()
                })
            });
        });
    }
    group.finish();

    // Benchmark concurrent connections with smaller iteration counts
    let concurrent_counts = [1, 5, 10, 20];
    let mut group = c.benchmark_group("websocket_concurrent");
    for count in concurrent_counts {
        group.bench_function(format!("{count}_connections"), |b| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        let mut handles = Vec::new();
                        for _ in 0..count {
                            handles.push(tokio::spawn(bench_websocket_echo(proxy_addr, 100, 5)));
                        }
                        for handle in handles {
                            if let Err(e) = handle.await.unwrap() {
                                error!("Benchmark error: {}", e);
                            }
                        }
                    }
                    start.elapsed()
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, websocket_benchmark);
criterion_main!(benches);
