use axum::{Router, body::Body, http::Request, response::Json, routing::get};
use axum_reverse_proxy::{HostBehaviour, ProxyPolicy, ReverseProxy};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Notify};

async fn echo_headers(req: Request<Body>) -> Json<Value> {
    let headers = req.headers().clone();
    Json(json!({ "headers": headers.iter().map(|(k, v)| {
        (k.as_str().to_string(), v.to_str().unwrap().to_string())
    }).collect::<HashMap<String, String>>() }))
}

#[tokio::test]
async fn test_proxy_header_handling() {
    // Create a test server that echoes headers
    let app = Router::new().route("/headers", get(echo_headers));

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let server_ready = Arc::new(Notify::new());
    let server_ready_clone = server_ready.clone();

    let test_server = tokio::spawn(async move {
        server_ready_clone.notify_one();
        axum::serve(test_listener, app).await.unwrap();
    });

    // Wait for test server to be ready
    server_ready.notified().await;

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_ready = Arc::new(Notify::new());
    let proxy_ready_clone = proxy_ready.clone();

    let proxy_server = tokio::spawn(async move {
        proxy_ready_clone.notify_one();
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Wait for proxy server to be ready
    proxy_ready.notified().await;

    // Create a client with a reasonable timeout
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Test with a reasonable number of headers (10 instead of 50)
    let mut headers = HeaderMap::new();
    for i in 0..10 {
        headers.insert(
            HeaderName::from_bytes(format!("x-test-{i}").as_bytes()).unwrap(),
            HeaderValue::from_str(&format!("value-{i}")).unwrap(),
        );
    }

    // Use tokio::time::timeout for the test operations
    let test_result = tokio::time::timeout(Duration::from_secs(10), async {
        println!("Sending request to proxy...");
        let response = match client
            .get(format!("http://{proxy_addr}/headers"))
            .headers(headers.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                println!("Failed to send request: {e}");
                return Err(e);
            }
        };

        println!("Got response with status: {}", response.status());
        assert_eq!(response.status().as_u16(), 200);

        let body = match response.json::<Value>().await {
            Ok(b) => {
                println!("Response body: {b}");
                b
            }
            Err(e) => {
                println!("Failed to parse response as JSON: {e}");
                return Err(e);
            }
        };

        let response_headers = body["headers"].as_object().unwrap();

        // Verify headers were forwarded (excluding host header)
        for (key, value) in headers.iter() {
            if key != "host" {
                println!("Checking header {}: {}", key, value.to_str().unwrap());
                assert_eq!(
                    response_headers[key.as_str()].as_str().unwrap(),
                    value.to_str().unwrap()
                );
            }
        }

        Ok(())
    })
    .await;

    // Clean up servers first
    proxy_server.abort();
    test_server.abort();

    // Then check the test result
    match test_result {
        Ok(result) => {
            if let Err(e) = result {
                panic!("Test failed with error: {e}");
            }
        }
        Err(_) => panic!("Test timed out after 10 seconds"),
    }
}

#[tokio::test]
async fn test_proxy_special_headers() {
    // Create a test server that echoes headers
    let app = Router::new().route("/headers", get(echo_headers));

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let server_ready = Arc::new(Notify::new());
    let server_ready_clone = server_ready.clone();

    let test_server = tokio::spawn(async move {
        server_ready_clone.notify_one();
        axum::serve(test_listener, app).await.unwrap();
    });

    // Wait for test server to be ready
    server_ready.notified().await;

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", format!("http://{test_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_ready = Arc::new(Notify::new());
    let proxy_ready_clone = proxy_ready.clone();

    let proxy_server = tokio::spawn(async move {
        proxy_ready_clone.notify_one();
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Wait for proxy server to be ready
    proxy_ready.notified().await;

    // Create a client with a reasonable timeout
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Use tokio::time::timeout for the test operations
    let test_result = tokio::time::timeout(Duration::from_secs(10), async {
        println!("Testing special headers...");
        let response = client
            .get(format!("http://{proxy_addr}/headers"))
            .header("X-Forwarded-For", "192.168.1.1")
            .header("X-Real-IP", "192.168.1.1")
            .header("X-Request-ID", "test-request-1")
            .header("Accept-Encoding", "gzip, deflate")
            .send()
            .await?;

        println!("Special headers response status: {}", response.status());
        assert_eq!(response.status().as_u16(), 200);

        let body = response.json::<Value>().await?;
        println!("Response body: {body}");

        // Verify the headers were forwarded
        let headers = body["headers"].as_object().unwrap();
        assert_eq!(headers["x-forwarded-for"], "192.168.1.1");
        assert_eq!(headers["x-real-ip"], "192.168.1.1");
        assert_eq!(headers["x-request-id"], "test-request-1");
        assert!(headers.contains_key("accept-encoding"));

        Ok::<(), reqwest::Error>(())
    })
    .await;

    // Clean up servers first
    proxy_server.abort();
    test_server.abort();

    // Then check the test result
    match test_result {
        Ok(result) => {
            if let Err(e) = result {
                panic!("Test failed with error: {e}");
            }
        }
        Err(_) => panic!("Test timed out after 10 seconds"),
    }
}

/// Spin up an echo-headers upstream and a proxy configured with `policy`,
/// returning (upstream_addr, proxy_addr). Both servers are spawned as
/// background tasks that live for the duration of the test process.
async fn setup_host_policy_proxy(policy: ProxyPolicy) -> (SocketAddr, SocketAddr) {
    let app = Router::new().route("/headers", get(echo_headers));
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

    (upstream_addr, proxy_addr)
}

/// Send a request through the proxy with an explicit `Host` header and return
/// the `host` value the upstream echoed back.
async fn upstream_host_seen(proxy_addr: SocketAddr, client_host: &str) -> String {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .header(reqwest::header::HOST, client_host)
        .send()
        .await
        .expect("request to proxy failed")
        .json()
        .await
        .expect("response was not json");

    body["headers"]["host"]
        .as_str()
        .expect("upstream did not echo a host header")
        .to_string()
}

#[tokio::test]
async fn host_header_with_default_policy_replaces_host_with_upstream_authority() {
    let (upstream_addr, proxy_addr) = setup_host_policy_proxy(ProxyPolicy::default()).await;

    let seen = upstream_host_seen(proxy_addr, "client.example.com").await;

    // Default (Replace) behaviour: upstream sees its own authority, not the
    // client-supplied host.
    assert_eq!(seen, upstream_addr.to_string());
}

#[tokio::test]
async fn host_header_with_preserve_policy_forwards_client_host() {
    let policy = ProxyPolicy::new().with_host_behaviour(HostBehaviour::Preserve);
    let (_upstream_addr, proxy_addr) = setup_host_policy_proxy(policy).await;

    let seen = upstream_host_seen(proxy_addr, "client.example.com").await;

    // Preserve behaviour: the client's original host flows through unchanged.
    assert_eq!(seen, "client.example.com");
}

/// Hop-by-hop headers (standard and Connection-nominated) must be stripped by
/// the proxy itself, without needing the RFC9110 layer.
#[tokio::test]
async fn test_hop_by_hop_headers_stripped_by_default() {
    let app = Router::new().route("/headers", get(echo_headers));
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .header("Connection", "keep-alive, x-hop")
        .header("x-hop", "should-not-forward")
        .header("Keep-Alive", "timeout=5")
        .header("Proxy-Connection", "keep-alive")
        .header("x-end-to-end", "kept")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let headers = body["headers"].as_object().unwrap();
    assert!(!headers.contains_key("x-hop"));
    assert!(!headers.contains_key("keep-alive"));
    assert!(!headers.contains_key("proxy-connection"));
    assert_eq!(headers["x-end-to-end"], "kept");
}

/// With connect info available, the proxy appends the client IP to
/// X-Forwarded-For and sets X-Forwarded-Host.
#[tokio::test]
async fn test_x_forwarded_for_appended_with_connect_info() {
    let app = Router::new().route("/headers", get(echo_headers));
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
        axum::serve(
            proxy_listener,
            proxy_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .header("X-Forwarded-For", "203.0.113.7")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let headers = body["headers"].as_object().unwrap();
    assert_eq!(headers["x-forwarded-for"], "203.0.113.7, 127.0.0.1");
    assert_eq!(headers["x-forwarded-host"], format!("{proxy_addr}"));
}
