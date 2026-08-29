use axum::{Router, body::Body, response::Response, routing::get};
use axum_reverse_proxy::DiscoverableBalancedProxy;
use futures_util::stream::Stream;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::Service;
use tower::discover::Change;

/// A discovery stream that adds services dynamically
#[derive(Clone)]
struct DynamicDiscoveryStream {
    services: Vec<String>,
    index: usize,
    counter: Arc<AtomicUsize>,
}

impl DynamicDiscoveryStream {
    fn new(services: Vec<String>) -> Self {
        Self {
            services,
            index: 0,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Stream for DynamicDiscoveryStream {
    type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Add services gradually
        let current_count = self.counter.load(Ordering::SeqCst);
        if current_count < self.services.len() {
            // Only add a service every few polls to simulate real discovery
            if self.index.is_multiple_of(3) && current_count < self.services.len() {
                let service = self.services[current_count].clone();
                self.counter.store(current_count + 1, Ordering::SeqCst);
                Poll::Ready(Some(Ok(Change::Insert(current_count, service))))
            } else {
                self.index += 1;
                Poll::Pending
            }
        } else {
            Poll::Pending
        }
    }
}

async fn create_test_server(port: u16, response_body: String) -> String {
    let app = Router::new().route(
        "/test",
        get(move || {
            let body = response_body.clone();
            async move { Response::new(Body::from(body)) }
        }),
    );

    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(Duration::from_millis(10)).await;

    format!("http://127.0.0.1:{}", addr.port())
}

#[tokio::test]
async fn test_discoverable_proxy_http_requests() {
    // Create test servers
    let server1_url = create_test_server(0, "Response from server 1".to_string()).await;
    let server2_url = create_test_server(0, "Response from server 2".to_string()).await;

    // Create discovery stream
    let discovery_stream = DynamicDiscoveryStream::new(vec![server1_url, server2_url]);

    // Create proxy
    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let mut proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Wait for services to be discovered
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify services were discovered
    assert!(proxy.service_count().await > 0);

    // Test making requests
    for i in 0..4 {
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/api/test")
            .body(Body::empty())
            .unwrap();

        let response = proxy.call(req).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Read response body
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Should get responses from different servers due to load balancing
        assert!(
            body_str.contains("Response from server 1")
                || body_str.contains("Response from server 2")
        );

        println!("Request {}: {}", i + 1, body_str);
    }
}

#[tokio::test]
async fn test_discoverable_proxy_load_balancing() {
    // Create test servers that return different responses
    let server1_url = create_test_server(0, "server1".to_string()).await;
    let server2_url = create_test_server(0, "server2".to_string()).await;
    let server3_url = create_test_server(0, "server3".to_string()).await;

    // Create discovery stream with all servers
    let discovery_stream = DynamicDiscoveryStream::new(vec![server1_url, server2_url, server3_url]);

    // Create proxy
    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let mut proxy = DiscoverableBalancedProxy::new_with_client("/", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Wait for all services to be discovered
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify all services were discovered
    assert_eq!(proxy.service_count().await, 3);

    // Make multiple requests and track which servers respond
    let mut server_responses = std::collections::HashMap::new();

    for _ in 0..9 {
        // Make 9 requests (3 per server in round-robin)
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        let response = proxy.call(req).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        *server_responses.entry(body_str).or_insert(0) += 1;
    }

    // Each server should have received exactly 3 requests (round-robin)
    assert_eq!(server_responses.len(), 3);
    for (server, count) in server_responses {
        println!("Server '{server}' received {count} requests");
        assert_eq!(count, 3);
    }
}

#[tokio::test]
async fn test_discoverable_proxy_service_unavailable() {
    // Create discovery stream with no services
    let discovery_stream = DynamicDiscoveryStream::new(vec![]);

    // Create proxy
    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let mut proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Wait a bit
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Should have no services
    assert_eq!(proxy.service_count().await, 0);

    // Make a request
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/test")
        .body(Body::empty())
        .unwrap();

    let response = proxy.call(req).await.unwrap();

    // Should return SERVICE_UNAVAILABLE
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body_str.contains("No upstream services available"));
}
