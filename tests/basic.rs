use axum::{
    Router,
    extract::Json,
    http::StatusCode,
    routing::{get, post},
};
use axum_reverse_proxy::ReverseProxy;
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_proxy_get_request() {
    // Create a test server
    let app = Router::new().route(
        "/test",
        get(|| async { Json(json!({"message": "Hello from test server!"})) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_post_request() {
    // Create a test server
    let app = Router::new().route(
        "/echo",
        post(|body: Json<Value>| async move { Json(body.0) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Send a request through the proxy
    let test_body = json!({"message": "Hello, proxy!"});
    let response = client
        .post(format!("http://{proxy_addr}/echo"))
        .json(&test_body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, test_body);

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_large_payload() {
    // Create a test server
    let app = Router::new().route(
        "/echo",
        post(|body: Json<Value>| async move { Json(body.0) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Create a large payload (1MB)
    let large_data = "x".repeat(1024 * 1024);
    let test_body = json!({"data": large_data});

    // Send a request through the proxy
    let response = client
        .post(format!("http://{proxy_addr}/echo"))
        .json(&test_body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, test_body);

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_http2_support() {
    // Create a test server
    let app = Router::new().route(
        "/test",
        get(|| async { Json(json!({"message": "Hello from test server!"})) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create an HTTP/2 client
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}
