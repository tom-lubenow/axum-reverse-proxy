use axum_reverse_proxy::{DiscoverableBalancedProxy, LoadBalancingStrategy};
use futures_util::stream::Stream;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::discover::Change;

/// A test discovery stream that yields a few services
#[derive(Clone)]
struct TestDiscoveryStream {
    services: Vec<String>,
    index: usize,
}

impl TestDiscoveryStream {
    fn new(services: Vec<String>) -> Self {
        Self { services, index: 0 }
    }
}

impl Stream for TestDiscoveryStream {
    type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.index < self.services.len() {
            let service = self.services[self.index].clone();
            let key = self.index;
            self.index += 1;
            Poll::Ready(Some(Ok(Change::Insert(key, service))))
        } else {
            Poll::Pending
        }
    }
}

/// A more advanced discovery stream that can simulate service additions and removals
#[derive(Clone)]
struct AdvancedDiscoveryStream {
    changes: Vec<(bool, usize, Option<String>)>, // (is_insert, key, service)
    index: usize,
}

impl AdvancedDiscoveryStream {
    fn new_with_changes(changes: Vec<(bool, usize, Option<String>)>) -> Self {
        Self { changes, index: 0 }
    }

    #[allow(dead_code)]
    fn with_inserts_and_removes(inserts: Vec<(usize, String)>, removes: Vec<usize>) -> Self {
        let mut changes = Vec::new();

        // Add all inserts first
        for (key, service) in inserts {
            changes.push((true, key, Some(service)));
        }

        // Then add removes
        for key in removes {
            changes.push((false, key, None));
        }

        Self { changes, index: 0 }
    }
}

impl Stream for AdvancedDiscoveryStream {
    type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.index < self.changes.len() {
            let current_index = self.index;
            self.index += 1;
            let (is_insert, key, service) = &self.changes[current_index];

            let change = if *is_insert {
                Change::Insert(*key, service.as_ref().unwrap().clone())
            } else {
                Change::Remove(*key)
            };

            Poll::Ready(Some(Ok(change)))
        } else {
            Poll::Pending
        }
    }
}

/// A discovery stream that yields errors
#[derive(Clone)]
struct ErrorDiscoveryStream {
    remaining_errors: usize,
}

impl ErrorDiscoveryStream {
    fn new(max_errors: usize) -> Self {
        Self {
            remaining_errors: max_errors,
        }
    }
}

impl Stream for ErrorDiscoveryStream {
    type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining_errors > 0 {
            self.remaining_errors -= 1;
            Poll::Ready(Some(Err("Discovery error".into())))
        } else {
            Poll::Pending
        }
    }
}

#[tokio::test]
async fn test_discoverable_proxy_creation() {
    let discovery_stream = TestDiscoveryStream::new(vec![
        "http://example1.com".to_string(),
        "http://example2.com".to_string(),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Test that the proxy can be created
    assert_eq!(proxy.path(), "/api");
    assert_eq!(proxy.service_count().await, 0);

    // Start discovery
    proxy.start_discovery().await;

    // Give discovery a moment to work
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Check that services were discovered
    assert!(proxy.service_count().await > 0);
}

#[tokio::test]
async fn test_discoverable_proxy_service_count() {
    let discovery_stream = TestDiscoveryStream::new(vec![
        "http://example1.com".to_string(),
        "http://example2.com".to_string(),
        "http://example3.com".to_string(),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/test", client, discovery_stream);

    // Initially no services
    assert_eq!(proxy.service_count().await, 0);

    // Start discovery
    proxy.start_discovery().await;

    // Give discovery time to find all services
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should have discovered all 3 services
    assert_eq!(proxy.service_count().await, 3);
}

#[tokio::test]
async fn test_discoverable_proxy_service_addition_and_removal() {
    let changes = vec![
        (true, 0, Some("http://service1.com".to_string())),
        (true, 1, Some("http://service2.com".to_string())),
        (true, 2, Some("http://service3.com".to_string())),
        (false, 1, None), // Remove service2
        (true, 3, Some("http://service4.com".to_string())),
        (false, 0, None), // Remove service1
    ];

    let discovery_stream = AdvancedDiscoveryStream::new_with_changes(changes);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Give time for all changes to be processed
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should have 2 services remaining (service3 and service4)
    assert_eq!(proxy.service_count().await, 2);
}

#[tokio::test]
async fn test_discoverable_proxy_error_handling() {
    let discovery_stream = ErrorDiscoveryStream::new(3); // Will produce 3 errors

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Give time for errors to be processed
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should still have no services (errors don't add services)
    assert_eq!(proxy.service_count().await, 0);
}

#[tokio::test]
async fn test_discoverable_proxy_no_services_response() {
    use axum::body::Body;
    use tower::Service;

    let discovery_stream = TestDiscoveryStream::new(vec![]); // No services

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let mut proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Give time for discovery to complete (should find no services)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a test request
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/test")
        .body(Body::empty())
        .unwrap();

    // Call the service
    let response = proxy.call(req).await.unwrap();

    // Should return SERVICE_UNAVAILABLE when no services are available
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn test_discoverable_proxy_path_handling() {
    let discovery_stream = TestDiscoveryStream::new(vec!["http://example.com".to_string()]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy =
        DiscoverableBalancedProxy::new_with_client("/custom/path", client, discovery_stream);

    // Test path getter
    assert_eq!(proxy.path(), "/custom/path");
}

#[tokio::test]
async fn test_discoverable_proxy_concurrent_access() {
    let discovery_stream = TestDiscoveryStream::new(vec![
        "http://service1.com".to_string(),
        "http://service2.com".to_string(),
        "http://service3.com".to_string(),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Give time for services to be discovered
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test concurrent access to service_count
    let proxy_clone = proxy.clone();
    let handle1 = tokio::spawn(async move { proxy_clone.service_count().await });

    let proxy_clone2 = proxy.clone();
    let handle2 = tokio::spawn(async move { proxy_clone2.service_count().await });

    let count1 = handle1.await.unwrap();
    let count2 = handle2.await.unwrap();

    // Both should return the same count
    assert_eq!(count1, count2);
    assert_eq!(count1, 3);
}

#[tokio::test]
async fn test_discoverable_proxy_service_replacement() {
    let changes = vec![
        (true, 0, Some("http://old-service.com".to_string())),
        (false, 0, None),                                      // Remove old service
        (true, 0, Some("http://new-service.com".to_string())), // Add new service with same key
    ];

    let discovery_stream = AdvancedDiscoveryStream::new_with_changes(changes);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start discovery
    proxy.start_discovery().await;

    // Give time for all changes to be processed
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Should have 1 service (the replacement)
    assert_eq!(proxy.service_count().await, 1);
}

#[tokio::test]
async fn test_discoverable_proxy_load_balancing_strategies() {
    let discovery_stream = TestDiscoveryStream::new(vec![
        "http://example1.com".to_string(),
        "http://example2.com".to_string(),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    // Test round-robin strategy (default)
    let proxy_rr = DiscoverableBalancedProxy::new_with_client(
        "/api",
        client.clone(),
        discovery_stream.clone(),
    );
    assert_eq!(proxy_rr.strategy(), LoadBalancingStrategy::RoundRobin);

    // Test explicit round-robin strategy
    let proxy_rr_explicit = DiscoverableBalancedProxy::new_with_client_and_strategy(
        "/api",
        client.clone(),
        discovery_stream.clone(),
        LoadBalancingStrategy::RoundRobin,
    );
    assert_eq!(
        proxy_rr_explicit.strategy(),
        LoadBalancingStrategy::RoundRobin
    );

    // Test P2C pending requests strategy
    let proxy_p2c_pending = DiscoverableBalancedProxy::new_with_client_and_strategy(
        "/api",
        client.clone(),
        discovery_stream.clone(),
        LoadBalancingStrategy::P2cPendingRequests,
    );
    assert_eq!(
        proxy_p2c_pending.strategy(),
        LoadBalancingStrategy::P2cPendingRequests
    );

    // Test P2C peak EWMA strategy
    let proxy_p2c_ewma = DiscoverableBalancedProxy::new_with_client_and_strategy(
        "/api",
        client,
        discovery_stream,
        LoadBalancingStrategy::P2cPeakEwma,
    );
    assert_eq!(
        proxy_p2c_ewma.strategy(),
        LoadBalancingStrategy::P2cPeakEwma
    );
}

#[tokio::test]
async fn test_load_balancing_strategy_enum() {
    // Test default strategy
    assert_eq!(
        LoadBalancingStrategy::default(),
        LoadBalancingStrategy::RoundRobin
    );

    // Test strategy equality
    assert_eq!(
        LoadBalancingStrategy::RoundRobin,
        LoadBalancingStrategy::RoundRobin
    );
    assert_ne!(
        LoadBalancingStrategy::RoundRobin,
        LoadBalancingStrategy::P2cPendingRequests
    );
    assert_ne!(
        LoadBalancingStrategy::P2cPendingRequests,
        LoadBalancingStrategy::P2cPeakEwma
    );

    // Test debug formatting
    let strategy = LoadBalancingStrategy::P2cPendingRequests;
    let debug_str = format!("{strategy:?}");
    assert!(debug_str.contains("P2cPendingRequests"));
}
#[tokio::test]
async fn test_p2c_pending_requests_prefers_fast_service() {
    use axum::{
        Router,
        body::{Body, to_bytes},
        routing::get,
    };
    use hyper_util::client::legacy::{Client, connect::HttpConnector};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    // Fast responder
    let fast_app = Router::new().route("/", get(|| async { "fast" }));
    let fast_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fast_addr = fast_listener.local_addr().unwrap();
    let fast_server = tokio::spawn(async move {
        axum::serve(fast_listener, fast_app).await.unwrap();
    });

    // Slow responder
    let slow_app = Router::new().route(
        "/",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            "slow"
        }),
    );
    let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_addr = slow_listener.local_addr().unwrap();
    let slow_server = tokio::spawn(async move {
        axum::serve(slow_listener, slow_app).await.unwrap();
    });

    let discovery = TestDiscoveryStream::new(vec![
        format!("http://{}", fast_addr),
        format!("http://{}", slow_addr),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client_and_strategy(
        "/",
        client,
        discovery,
        LoadBalancingStrategy::P2cPendingRequests,
    );
    proxy.start_discovery().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a few warm-up requests sequentially to help the algorithm learn latencies
    for _ in 0..5 {
        let svc = proxy.clone();
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let _ = svc.oneshot(req).await.unwrap();
        // Small delay to ensure measurements are spaced out
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut handles = Vec::new();
    for _ in 0..20 {
        let svc = proxy.clone();
        handles.push(tokio::spawn(async move {
            let req = axum::http::Request::builder()
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = svc.oneshot(req).await.unwrap();
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }));
        // Small delay between spawning requests to let the algorithm observe pending counts
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let mut fast = 0;
    let mut slow = 0;
    for h in handles {
        match h.await.unwrap().as_str() {
            "fast" => fast += 1,
            "slow" => slow += 1,
            other => panic!("unexpected response {other}"),
        }
    }

    assert!(fast > slow, "fast {fast} slow {slow}");

    fast_server.abort();
    slow_server.abort();
}

#[tokio::test]
async fn test_p2c_peak_ewma_prefers_fast_service() {
    use axum::{
        Router,
        body::{Body, to_bytes},
        routing::get,
    };
    use hyper_util::client::legacy::{Client, connect::HttpConnector};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    let fast_app = Router::new().route("/", get(|| async { "fast" }));
    let fast_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fast_addr = fast_listener.local_addr().unwrap();
    let fast_server = tokio::spawn(async move {
        axum::serve(fast_listener, fast_app).await.unwrap();
    });

    let slow_app = Router::new().route(
        "/",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            "slow"
        }),
    );
    let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_addr = slow_listener.local_addr().unwrap();
    let slow_server = tokio::spawn(async move {
        axum::serve(slow_listener, slow_app).await.unwrap();
    });

    let discovery = TestDiscoveryStream::new(vec![
        format!("http://{}", fast_addr),
        format!("http://{}", slow_addr),
    ]);

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client_and_strategy(
        "/",
        client,
        discovery,
        LoadBalancingStrategy::P2cPeakEwma,
    );
    proxy.start_discovery().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a few warm-up requests sequentially to help the algorithm learn latencies
    for _ in 0..5 {
        let svc = proxy.clone();
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let _ = svc.oneshot(req).await.unwrap();
        // Small delay to ensure measurements are spaced out
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut handles = Vec::new();
    for _ in 0..20 {
        let svc = proxy.clone();
        handles.push(tokio::spawn(async move {
            let req = axum::http::Request::builder()
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = svc.oneshot(req).await.unwrap();
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }));
    }

    let mut fast = 0;
    let mut slow = 0;
    for h in handles {
        match h.await.unwrap().as_str() {
            "fast" => fast += 1,
            "slow" => slow += 1,
            other => panic!("unexpected response {other}"),
        }
    }

    assert!(fast > slow, "fast {fast} slow {slow}");

    fast_server.abort();
    slow_server.abort();
}
