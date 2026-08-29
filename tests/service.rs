use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Json,
    http::{Request, StatusCode},
    routing::{get, post},
};
use axum_reverse_proxy::ReverseProxy;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

#[tokio::test]
async fn test_proxy_service_get() {
    let app = Router::new().route(
        "/test",
        get(|| async { Json(json!({"message": "Hello from test server!"})) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));

    let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

    let response = proxy.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    test_server.abort();
}

#[tokio::test]
async fn test_proxy_service_post() {
    let app = Router::new().route(
        "/echo",
        post(|body: Json<Value>| async move { Json(body.0) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));

    let test_body = json!({"message": "Hello, proxy!"});
    let request = Request::builder()
        .method("POST")
        .uri("/echo")
        .header("content-type", "application/json")
        .body(Body::from(test_body.to_string()))
        .unwrap();

    let response = proxy.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body, test_body);

    test_server.abort();
}
