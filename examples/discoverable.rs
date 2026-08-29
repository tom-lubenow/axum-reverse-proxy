use axum::{Router, serve};
use axum_reverse_proxy::DiscoverableBalancedProxy;
use futures_util::stream::Stream;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::discover::Change;

/// A simple discovery stream that simulates services being added
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Create a discovery stream with some example services
    let discovery_stream = SimpleDiscoveryStream::new(vec![
        "https://httpbin.org".to_string(),
        "https://api.github.com".to_string(),
    ]);

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

    // Create the discoverable balanced proxy
    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);

    // Start the discovery process
    proxy.start_discovery().await;

    // Give discovery a moment to find services
    tokio::time::sleep(Duration::from_millis(100)).await;

    let app: Router = Router::new().nest_service("/", proxy);

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Server running on http://localhost:3000");
    println!("Try: curl http://localhost:3000/api/get");
    serve(listener, app).await.unwrap();
}
