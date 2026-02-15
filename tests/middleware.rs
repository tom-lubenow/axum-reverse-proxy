use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    response::Json,
    routing::get,
};
use axum_reverse_proxy::ReverseProxy;
use http::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use tower_http::validate_request::ValidateRequestHeaderLayer;

#[allow(deprecated)] // ValidateRequestHeaderLayer::bearer is deprecated in tower-http 0.6.8
#[tokio::test]
async fn test_proxy_with_middleware() {
    // Create a test server that echoes headers
    let app = Router::new().route(
        "/headers",
        get(|req: Request<Body>| async move {
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
                .collect::<Vec<_>>();
            Json(json!({ "headers": headers }))
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", &format!("http://{test_addr}"));
    let proxy_router: Router = proxy.into();

    // Add middleware stack
    let app = proxy_router.layer(
        ServiceBuilder::new()
            // Add request timeout
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(10),
            ))
            // Require API key
            .layer(ValidateRequestHeaderLayer::bearer("test-token"))
            // Add custom header
            .map_request(|mut req: Request<Body>| {
                req.headers_mut().insert(
                    HeaderName::from_static("x-custom-header"),
                    HeaderValue::from_static("custom-value"),
                );
                req
            }),
    );

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test 1: Request without auth token should fail
    let response = client
        .get(format!("http://{proxy_addr}/headers"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        StatusCode::UNAUTHORIZED.as_u16()
    );

    // Test 2: Request with auth token should succeed and include custom header
    let response = client
        .get(format!("http://{proxy_addr}/headers"))
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());

    let body: Value = response.json().await.unwrap();
    let headers = body.get("headers").unwrap().as_array().unwrap();

    // Verify custom header was added
    let has_custom_header = headers.iter().any(|h| {
        h.as_array()
            .unwrap()
            .first()
            .unwrap()
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case("x-custom-header")
            && h.as_array()
                .unwrap()
                .get(1)
                .unwrap()
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("custom-value")
    });
    assert!(has_custom_header, "Custom header not found in response");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_timeout_middleware() {
    // Create a test server with artificial delay
    let app = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            "Done"
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy with a short timeout
    let proxy = ReverseProxy::new("/", &format!("http://{test_addr}"));
    let proxy_router: Router = proxy.into();

    // Add timeout middleware
    let app = proxy_router.layer(TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_millis(100),
    ));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test: Request should timeout
    let response = client
        .get(format!("http://{proxy_addr}/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        StatusCode::REQUEST_TIMEOUT.as_u16()
    );

    // Clean up
    proxy_server.abort();
    test_server.abort();
}
