use axum::{
    Router,
    routing::{get, post},
};
use axum_reverse_proxy::{ReverseProxy, Rfc9110Config, Rfc9110Layer};
use std::collections::HashSet;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    // Create a test server
    let app = Router::new()
        .route("/test", get(|| async { "Hello from test server!" }))
        .route("/echo", post(|body: String| async move { body }));

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy with RFC9110 compliance
    let proxy = ReverseProxy::new("/", &format!("http://{test_addr}"));
    let mut server_names = HashSet::new();
    server_names.insert("localhost".to_string());
    server_names.insert("127.0.0.1".to_string());

    let config = Rfc9110Config {
        server_names: Some(server_names),
        pseudonym: Some("example-proxy".to_string()),
        combine_via: true,
        preserve_websocket_headers: true,
    };

    // Apply the RFC9110 middleware to the proxy
    let app: Router = Router::new()
        .layer(Rfc9110Layer::with_config(config))
        .merge(proxy);

    // Start the proxy server
    let proxy_listener = TcpListener::bind("127.0.0.1:3000").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    println!("Proxy server listening on {proxy_addr}");

    axum::serve(proxy_listener, app).await.unwrap();
}
