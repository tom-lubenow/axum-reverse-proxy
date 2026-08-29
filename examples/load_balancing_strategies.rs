use axum::{Router, serve};
use axum_reverse_proxy::{DiscoverableBalancedProxy, LoadBalancingStrategy};
use futures_util::stream::Stream;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::discover::Change;

/// A simple discovery stream that adds services dynamically
#[derive(Clone)]
struct SimpleDiscoveryStream {
    services: Vec<String>,
    index: usize,
}

impl SimpleDiscoveryStream {
    fn new(services: Vec<String>) -> Self {
        Self { services, index: 0 }
    }
}

impl Stream for SimpleDiscoveryStream {
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

async fn create_proxy_with_strategy(
    path: &str,
    services: Vec<String>,
    strategy: LoadBalancingStrategy,
) -> DiscoverableBalancedProxy<HttpConnector, SimpleDiscoveryStream> {
    // Create discovery stream
    let discovery_stream = SimpleDiscoveryStream::new(services);

    // Create HTTP client
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.enforce_http(false);

    let client = Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .retry_canceled_requests(true)
        .set_host(true)
        .build(connector);

    // Create the discoverable balanced proxy with the specified strategy
    let proxy = DiscoverableBalancedProxy::new_with_client_and_strategy(
        path,
        client,
        discovery_stream,
        strategy,
    );

    // Start the discovery process
    proxy.start_discovery().await;

    // Give discovery a moment to find services
    tokio::time::sleep(Duration::from_millis(100)).await;

    proxy
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    println!("Load Balancing Strategies Demo");
    println!("==============================");

    let services = vec![
        "https://httpbin.org".to_string(),
        "https://api.github.com".to_string(),
        "https://jsonplaceholder.typicode.com".to_string(),
    ];

    // Demonstrate Round Robin strategy
    println!("\n1. Round Robin Load Balancing (Default)");
    println!("----------------------------------------");
    let rr_proxy =
        create_proxy_with_strategy("/rr", services.clone(), LoadBalancingStrategy::RoundRobin)
            .await;
    println!("Strategy: {:?}", rr_proxy.strategy());
    println!("Services discovered: {}", rr_proxy.service_count().await);
    println!("Description: Distributes requests evenly in a circular order");
    println!("Best for: Homogeneous services with similar capacity and response times");

    // Demonstrate P2C with Pending Requests
    println!("\n2. P2C with Pending Requests Load Balancing");
    println!("--------------------------------------------");
    let p2c_pending_proxy = create_proxy_with_strategy(
        "/p2c-pending",
        services.clone(),
        LoadBalancingStrategy::P2cPendingRequests,
    )
    .await;
    println!("Strategy: {:?}", p2c_pending_proxy.strategy());
    println!(
        "Services discovered: {}",
        p2c_pending_proxy.service_count().await
    );
    println!(
        "Description: Uses Power of Two Choices algorithm with pending request count as load metric"
    );
    println!("Best for: Services with varying request processing times");
    println!("Note: Currently falls back to round-robin (P2C implementation pending)");

    // Demonstrate P2C with Peak EWMA
    println!("\n3. P2C with Peak EWMA Load Balancing");
    println!("-------------------------------------");
    let p2c_ewma_proxy = create_proxy_with_strategy(
        "/p2c-ewma",
        services.clone(),
        LoadBalancingStrategy::P2cPeakEwma,
    )
    .await;
    println!("Strategy: {:?}", p2c_ewma_proxy.strategy());
    println!(
        "Services discovered: {}",
        p2c_ewma_proxy.service_count().await
    );
    println!(
        "Description: Uses Power of Two Choices algorithm with peak EWMA latency as load metric"
    );
    println!("Best for: Services with varying response times and latency characteristics");
    println!("Note: Currently falls back to round-robin (P2C implementation pending)");

    // Create a router that demonstrates all strategies
    let app = Router::new()
        .nest_service("/rr", rr_proxy)
        .nest_service("/p2c-pending", p2c_pending_proxy)
        .nest_service("/p2c-ewma", p2c_ewma_proxy);

    println!("\n🚀 Server starting on http://localhost:3000");
    println!("\nTry these endpoints:");
    println!("  • http://localhost:3000/rr/get - Round Robin");
    println!("  • http://localhost:3000/p2c-pending/get - P2C Pending Requests");
    println!("  • http://localhost:3000/p2c-ewma/get - P2C Peak EWMA");
    println!("\nPress Ctrl+C to stop the server");

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    serve(listener, app).await.unwrap();
}
