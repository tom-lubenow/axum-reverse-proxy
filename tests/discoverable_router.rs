use axum::{Router, routing::get};
use axum_reverse_proxy::DiscoverableBalancedProxy;
use futures_util::stream::Stream;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::discover::Change;

#[derive(Clone)]
struct SingleDiscovery {
    service: String,
    yielded: bool,
}

impl SingleDiscovery {
    fn new(service: String) -> Self {
        Self {
            service,
            yielded: false,
        }
    }
}

impl Stream for SingleDiscovery {
    type Item = Result<Change<usize, String>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.yielded {
            self.yielded = true;
            Poll::Ready(Some(Ok(Change::Insert(0, self.service.clone()))))
        } else {
            Poll::Pending
        }
    }
}

#[tokio::test]
async fn test_discoverable_proxy_into_router() {
    let upstream_app = Router::new().route("/test", get(|| async { "upstream" }));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app).await.unwrap();
    });

    let discovery_stream = SingleDiscovery::new(format!("http://{upstream_addr}"));

    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);

    let proxy = DiscoverableBalancedProxy::new_with_client("/api", client, discovery_stream);
    proxy.start_discovery().await;

    // Give discovery some time
    tokio::time::sleep(Duration::from_millis(50)).await;

    let proxy_router: Router = proxy.into();
    let app = Router::new()
        .route("/", get(|| async { "root" }))
        .merge(proxy_router);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "upstream");

    let resp = client.get(format!("http://{addr}/")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "root");

    let resp = client
        .get(format!("http://{addr}/other"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    server.abort();
    upstream_server.abort();
}
