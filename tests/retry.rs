use axum::{Router, routing::get};
use axum_reverse_proxy::{RetryLayer, ReverseProxy};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceBuilder;

async fn delayed_server(addr: std::net::SocketAddr) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let app = Router::new().route("/test", get(|| async { "ok" }));
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[tokio::test]
async fn test_no_retry_by_default() {
    // Reserve an address and then release it
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = temp.local_addr().unwrap();
    drop(temp);

    let proxy = ReverseProxy::new("/", format!("http://{addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Spawn server after delay
    let server_handle = tokio::spawn(delayed_server(addr));

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);

    proxy_server.abort();
    server_handle.abort();
}

#[tokio::test]
async fn test_retry_layer() {
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = temp.local_addr().unwrap();
    drop(temp);

    let proxy = ReverseProxy::new("/", format!("http://{addr}"));
    let app: Router = proxy.into();
    let app = app.layer(ServiceBuilder::new().layer(RetryLayer::new(5)));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let server_handle = tokio::spawn(delayed_server(addr));

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    proxy_server.abort();
    server_handle.abort();
}

#[tokio::test]
async fn test_retry_layer_zero_attempts() {
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = temp.local_addr().unwrap();
    drop(temp);

    let proxy = ReverseProxy::new("/", format!("http://{addr}"));
    let app: Router = proxy.into();
    let app = app.layer(ServiceBuilder::new().layer(RetryLayer::new(0)));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let server_handle = tokio::spawn(delayed_server(addr));

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);

    proxy_server.abort();
    server_handle.abort();
}

/// A POST body must be replayed in full on retry — the first attempt's
/// connect failure must not truncate or drop the body (regression test).
#[tokio::test]
async fn test_retry_replays_full_post_body() {
    use axum::extract::Request;

    // Reserve an address and then release it
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = temp.local_addr().unwrap();
    drop(temp);

    let proxy = ReverseProxy::new("/", format!("http://{addr}"));
    let app: Router = proxy.into();
    let app = app.layer(ServiceBuilder::new().layer(RetryLayer::new(5)));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Spawn an echo server after a delay so the first attempt fails to connect
    let server_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let echo = Router::new().route(
            "/echo",
            axum::routing::post(|req: Request| async move {
                axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap()
            }),
        );
        let listener = TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, echo).await.unwrap();
    });

    let payload = "x".repeat(64 * 1024);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{proxy_addr}/echo"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), payload);

    proxy_server.abort();
    server_handle.abort();
}

/// A 502 returned by the upstream itself must NOT be retried — only
/// proxy-synthesized connect failures are safe to replay.
#[tokio::test]
async fn test_no_retry_on_upstream_502() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let backend = Router::new().route(
        "/test",
        get(move || {
            let hits = hits_clone.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::BAD_GATEWAY
            }
        }),
    );

    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let backend_server = tokio::spawn(async move {
        axum::serve(backend_listener, backend).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{backend_addr}"));
    let app: Router = proxy.into();
    let app = app.layer(ServiceBuilder::new().layer(RetryLayer::new(5)));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "upstream 502 was retried");

    proxy_server.abort();
    backend_server.abort();
}
