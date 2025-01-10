use axum::{
    body::Body,
    extract::Json,
    http::StatusCode,
    routing::{get, post},
    Router,
};
use axum_reverse_proxy::ReverseProxy;
use serde_json::{json, Value};
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
    let proxy = ReverseProxy::new("/", &format!("http://{}", test_addr));
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
        .get(format!("http://{}/test", proxy_addr))
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
    let proxy = ReverseProxy::new("/", &format!("http://{}", test_addr));
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
        .post(format!("http://{}/echo", proxy_addr))
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
    let proxy = ReverseProxy::new("/", &format!("http://{}", test_addr));
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
        .post(format!("http://{}/echo", proxy_addr))
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
    let proxy = ReverseProxy::new("/", &format!("http://{}", test_addr));
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
        .get(format!("http://{}/test", proxy_addr))
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
async fn test_proxy_uses_http2_with_backend() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Create a flag to track if HTTP/2 was used
    let used_http2 = Arc::new(AtomicBool::new(false));
    let used_http2_clone = used_http2.clone();

    // Generate a self-signed certificate for testing
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = cert.serialize_pem().unwrap();
    let key_pem = cert.serialize_private_key_pem();

    // Create a test server that supports HTTP/2
    let app = Router::new().route(
        "/test",
        get(move |req: axum::extract::Request<_>| {
            let used_http2 = used_http2_clone.clone();
            async move {
                // Check if we're using HTTP/2 by looking at the request version
                if req.version() == http::Version::HTTP_2 {
                    used_http2.store(true, Ordering::SeqCst);
                }
                Json(json!({"message": "Hello from test server!"}))
            }
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert_pem.as_bytes().to_vec(),
            key_pem.as_bytes().to_vec(),
        )
        .await
        .unwrap();

        axum_server::bind_rustls(test_addr, config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", &format!("https://{}", test_addr));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client that trusts our self-signed certificate
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{}/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Verify that HTTP/2 was used with the backend
    assert!(
        used_http2.load(Ordering::SeqCst),
        "HTTP/2 was not used with the backend"
    );

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_http2_alpn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Create a flag to track if HTTP/2 was used
    let used_http2 = Arc::new(AtomicBool::new(false));
    let used_http2_clone = used_http2.clone();

    // Generate a self-signed certificate for testing
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = cert.serialize_pem().unwrap();
    let key_pem = cert.serialize_private_key_pem();

    // Create a test server that supports HTTP/2
    let app = Router::new().route(
        "/test",
        get(move |req: axum::extract::Request<_>| {
            let used_http2 = used_http2_clone.clone();
            async move {
                // Check if we're using HTTP/2 by looking at the request version
                if req.version() == http::Version::HTTP_2 {
                    used_http2.store(true, Ordering::SeqCst);
                }
                Json(json!({"message": "Hello from test server!"}))
            }
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert_pem.as_bytes().to_vec(),
            key_pem.as_bytes().to_vec(),
        )
        .await
        .unwrap();

        axum_server::bind_rustls(test_addr, config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", &format!("https://{}", test_addr));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client that trusts our self-signed certificate and explicitly enables HTTP/2
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http2_prior_knowledge()
        .build()
        .unwrap();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{}/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Verify that HTTP/2 was used with the backend
    assert!(
        used_http2.load(Ordering::SeqCst),
        "HTTP/2 was not used with the backend"
    );

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_http1_client_to_http2_backend() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Create flags to track protocol versions
    let used_http2_backend = Arc::new(AtomicBool::new(false));
    let used_http2_backend_clone = used_http2_backend.clone();
    let used_http1_client = Arc::new(AtomicBool::new(false));
    let used_http1_client_clone = used_http1_client.clone();

    // Generate a self-signed certificate for testing
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = cert.serialize_pem().unwrap();
    let key_pem = cert.serialize_private_key_pem();

    // Create a test server that supports HTTP/2
    let app = Router::new().route(
        "/test",
        get(move |req: axum::extract::Request<_>| {
            let used_http2 = used_http2_backend_clone.clone();
            async move {
                // Check if we're using HTTP/2 by looking at the request version
                if req.version() == http::Version::HTTP_2 {
                    used_http2.store(true, Ordering::SeqCst);
                }
                Json(json!({"message": "Hello from test server!"}))
            }
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert_pem.as_bytes().to_vec(),
            key_pem.as_bytes().to_vec(),
        )
        .await
        .unwrap();

        axum_server::bind_rustls(test_addr, config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    // Create a reverse proxy that can track the client's protocol version
    let proxy = ReverseProxy::new("/", &format!("https://{}", test_addr));
    let app = Router::new().route(
        "/*path",
        get(move |req: axum::extract::Request<Body>| {
            let used_http1 = used_http1_client_clone.clone();
            async move {
                if req.version() == http::Version::HTTP_11 {
                    used_http1.store(true, Ordering::SeqCst);
                }
                proxy.proxy_request(req).await
            }
        }),
    );

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client that explicitly uses HTTP/1.1 and trusts our self-signed certificate
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only() // Force HTTP/1.1
        .build()
        .unwrap();

    // Send a request through the proxy
    let response = client
        .get(format!("http://{}/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), StatusCode::OK.as_u16());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Verify that HTTP/1.1 was used by the client and HTTP/2 was used with the backend
    assert!(
        used_http1_client.load(Ordering::SeqCst),
        "HTTP/1.1 was not used by the client"
    );
    assert!(
        used_http2_backend.load(Ordering::SeqCst),
        "HTTP/2 was not used with the backend"
    );

    // Clean up
    proxy_server.abort();
    test_server.abort();
}
