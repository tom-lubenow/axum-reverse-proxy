use axum::{Router, serve};
use axum_reverse_proxy::{DiscoverableBalancedProxy, DnsDiscovery, DnsDiscoveryConfig};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Create DNS discovery configuration
    // This will resolve "httpbin.org" every 30 seconds and treat each IP as a separate service
    let dns_config = DnsDiscoveryConfig::new("httpbin.org", 80)
        .with_refresh_interval(Duration::from_secs(30))
        .with_https(false); // Use HTTP since we're connecting to port 80

    // Create the DNS discovery instance
    let discovery = DnsDiscovery::new(dns_config).expect("Failed to create DNS discovery");

    // Create an HTTP client
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.enforce_http(false);

    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    let connector = {
        use hyper_rustls::HttpsConnectorBuilder;
        HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector)
    };

    #[cfg(feature = "native-tls")]
    let connector = {
        use hyper_tls::HttpsConnector;
        HttpsConnector::new_with_connector(connector)
    };

    let client = Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .retry_canceled_requests(true)
        .set_host(true)
        .build(connector);

    // Create the discoverable balanced proxy with DNS discovery
    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery);

    // Start the discovery process
    proxy.start_discovery().await;

    // Give discovery a moment to find services
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("Discovered {} services", proxy.service_count().await);

    let app: Router = Router::new().nest_service("/", proxy);

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Server running on http://localhost:3000");
    println!("Try: curl http://localhost:3000/api/get");
    println!(
        "The proxy will automatically load balance across all IP addresses resolved for httpbin.org"
    );
    serve(listener, app).await.unwrap();
}
