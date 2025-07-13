use axum::{Router, response::Html, routing::get};
use axum_reverse_proxy::{
    DockerDiscovery, DockerRouter, DockerRouterExt, LoadBalancingStrategy, PortDetectionStrategy,
};
use hyper_util::client::legacy::Client;
use std::net::SocketAddr;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::info;

#[tokio::main]
async fn main() {
    // Enable tracing
    tracing_subscriber::fmt()
        .with_env_filter("axum_reverse_proxy=debug,docker_discovery=info")
        .init();

    // Create HTTP client
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();

    // Example 1: Discover only labeled containers
    let labeled_discovery = DockerDiscovery::labeled("proxy.enable=true")
        .await
        .expect("Failed to create labeled discovery");

    // Example 2: Custom configuration with builder
    let custom_discovery = DockerDiscovery::builder()
        .label_prefix("myapp")
        .network("my-network")
        .port_detection(PortDetectionStrategy::FromLabel("port".to_string()))
        .strip_prefix("myproject_")
        .build()
        .await
        .expect("Failed to create custom discovery");

    // Example 3: Everything with custom strategy
    let everything_discovery = DockerDiscovery::everything()
        .await
        .expect("Failed to create discovery");

    // Create router with P2C load balancing
    let docker_router = DockerRouter::new(client, everything_discovery)
        .await
        .expect("Failed to create Docker router")
        .with_strategy(LoadBalancingStrategy::P2cPendingRequests);

    info!("Advanced Docker discovery started");

    // Create the main application
    let app = Router::new()
        .route("/", get(advanced_welcome))
        .docker_proxy(docker_router)
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()));

    // Start the server
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Reverse proxy listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn advanced_welcome() -> Html<&'static str> {
    Html(
        r#"
        <h1>Advanced Docker Discovery Proxy</h1>
        <h2>Configuration Options</h2>
        
        <h3>1. Label-Based Discovery</h3>
        <pre><code>DockerDiscovery::labeled("proxy.enable=true")</code></pre>
        <p>Only discovers containers with specific labels.</p>
        
        <h3>2. Custom Configuration</h3>
        <pre><code>DockerDiscovery::builder()
    .label_prefix("myapp")
    .network("my-network")
    .port_detection(PortDetectionStrategy::FromLabel("port"))
    .build()</code></pre>
        
        <h3>3. Everything Mode</h3>
        <pre><code>DockerDiscovery::everything()</code></pre>
        <p>Discovers all running containers automatically.</p>
        
        <h3>Load Balancing Strategies</h3>
        <ul>
            <li><strong>RoundRobin</strong>: Simple round-robin (default)</li>
            <li><strong>P2cPendingRequests</strong>: Power of Two Choices with pending request count</li>
            <li><strong>P2cPeakEwma</strong>: Power of Two Choices with EWMA latency</li>
        </ul>
        
        <h3>Container Labels</h3>
        <p>You can configure containers with these labels:</p>
        <ul>
            <li><code>axum-proxy.enable=true</code> - Enable discovery</li>
            <li><code>axum-proxy.port=8080</code> - Specify port</li>
            <li><code>axum-proxy.path=/api</code> - Custom path</li>
        </ul>
        "#,
    )
}
