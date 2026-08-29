use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
use axum_reverse_proxy::ReverseProxy;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn test_proxy_timeout() {
    // Create a test server
    let app = Router::new().route(
        "/delay",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            "Delayed response"
        }),
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

    // Create a client with a short timeout
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{proxy_addr}/delay"))
        .send()
        .await;

    assert!(response.is_err());
    assert!(response.unwrap_err().is_timeout());

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_error_handling() {
    // Create a client with short timeouts
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();

    // Test connection refused (use a port that's likely not in use)
    let proxy = ReverseProxy::new("/", "http://127.0.0.1:59999");
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let response = client
        .get(format!("http://{proxy_addr}/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    // Clean up
    proxy_server.abort();
}

#[tokio::test]
async fn test_proxy_upstream_errors() {
    // Create a test server that returns various error codes
    let app = Router::new()
        .route("/400", get(|| async { StatusCode::BAD_REQUEST }))
        .route(
            "/401",
            get(|| async { (StatusCode::UNAUTHORIZED, "Unauthorized access").into_response() }),
        )
        .route(
            "/403",
            get(|| async { (StatusCode::FORBIDDEN, "Forbidden resource").into_response() }),
        )
        .route("/404", get(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/500",
            get(|| async {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            }),
        )
        .route(
            "/503",
            get(|| async {
                (StatusCode::SERVICE_UNAVAILABLE, "Service unavailable").into_response()
            }),
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

    // Test various error codes
    let test_cases = vec![
        (400, "Bad Request"),
        (401, "Unauthorized access"),
        (403, "Forbidden resource"),
        (404, "Not Found"),
        (500, "Internal server error"),
        (503, "Service unavailable"),
    ];

    for (status_code, expected_body) in test_cases {
        let response = client
            .get(format!("http://{proxy_addr}/{status_code}"))
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status().as_u16(),
            status_code,
            "Expected status code {} but got {}",
            status_code,
            response.status()
        );

        if status_code != 400 && status_code != 404 {
            let body = response.text().await.unwrap();
            assert_eq!(body, expected_body);
        }
    }

    // Clean up
    proxy_server.abort();
    test_server.abort();
}
