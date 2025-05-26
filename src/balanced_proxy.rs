use axum::body::Body;
#[cfg(feature = "tls")]
use hyper_rustls::HttpsConnector;
#[cfg(feature = "native-tls")]
use hyper_tls::HttpsConnector as NativeTlsHttpsConnector;
use hyper_util::client::legacy::{
    connect::{Connect, HttpConnector},
    Client,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tower::balance::p2c::Balance;
use tower::discover::{Change, Discover};
use tower::load::CompleteOnResponse;
use tower::load::{peak_ewma::PeakEwmaDiscover, pending_requests::PendingRequestsDiscover};
use tower::util::BoxService;
use tower::BoxError;
use tracing::{debug, error, trace, warn};

use crate::proxy::ReverseProxy;

type ProxyService = BoxService<axum::http::Request<Body>, axum::http::Response<Body>, BoxError>;

/// Load balancing strategy for distributing requests across discovered services
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadBalancingStrategy {
    /// Simple round-robin distribution (default)
    RoundRobin,
    /// Power of Two Choices with pending request count as load metric
    P2cPendingRequests,
    /// Power of Two Choices with peak EWMA latency as load metric
    P2cPeakEwma,
}

impl Default for LoadBalancingStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

#[derive(Clone)]
pub struct BalancedProxy<C: Connect + Clone + Send + Sync + 'static> {
    path: String,
    proxies: Vec<ReverseProxy<C>>,
    counter: Arc<AtomicUsize>,
}

#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub type StandardBalancedProxy = BalancedProxy<HttpsConnector<HttpConnector>>;
#[cfg(all(feature = "native-tls", not(feature = "tls")))]
pub type StandardBalancedProxy = BalancedProxy<NativeTlsHttpsConnector<HttpConnector>>;
#[cfg(all(feature = "tls", feature = "native-tls"))]
pub type StandardBalancedProxy = BalancedProxy<HttpsConnector<HttpConnector>>;
#[cfg(not(any(feature = "tls", feature = "native-tls")))]
pub type StandardBalancedProxy = BalancedProxy<HttpConnector>;

impl StandardBalancedProxy {
    pub fn new<S>(path: S, targets: Vec<S>) -> Self
    where
        S: Into<String> + Clone,
    {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.enforce_http(false);
        connector.set_keepalive(Some(std::time::Duration::from_secs(60)));
        connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
        connector.set_reuse_address(true);

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

        #[cfg(all(feature = "native-tls", not(feature = "tls")))]
        let connector = NativeTlsHttpsConnector::new_with_connector(connector);

        #[cfg(all(feature = "tls", feature = "native-tls"))]
        let connector = {
            use hyper_rustls::HttpsConnectorBuilder;
            HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .wrap_connector(connector)
        };

        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(connector);

        Self::new_with_client(path, targets, client)
    }
}

impl<C> BalancedProxy<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    pub fn new_with_client<S>(path: S, targets: Vec<S>, client: Client<C, Body>) -> Self
    where
        S: Into<String> + Clone,
    {
        let path = path.into();
        let proxies = targets
            .into_iter()
            .map(|t| ReverseProxy::new_with_client(path.clone(), t.into(), client.clone()))
            .collect();

        Self {
            path,
            proxies,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn next_proxy(&self) -> Option<ReverseProxy<C>> {
        if self.proxies.is_empty() {
            None
        } else {
            let idx = self.counter.fetch_add(1, Ordering::SeqCst) % self.proxies.len();
            Some(self.proxies[idx].clone())
        }
    }
}

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;

impl<C> Service<axum::http::Request<Body>> for BalancedProxy<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    type Response = axum::http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        if let Some(mut proxy) = self.next_proxy() {
            trace!("balanced proxying via upstream {}", proxy.target());
            Box::pin(async move { proxy.call(req).await })
        } else {
            warn!("No upstream services available");
            Box::pin(async move {
                Ok(axum::http::Response::builder()
                    .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from("No upstream services available"))
                    .unwrap())
            })
        }
    }
}

/// A balanced proxy that supports dynamic service discovery.
///
/// This proxy uses the tower::discover trait to dynamically add and remove
/// upstream services. Services are load-balanced using a configurable strategy.
#[derive(Clone)]
pub struct DiscoverableBalancedProxy<C, D>
where
    C: Connect + Clone + Send + Sync + 'static,
    D: Discover + Clone + Send + Sync + 'static,
    D::Service: Into<String> + Send,
    D::Key: Clone + std::fmt::Debug + Send + Sync + std::hash::Hash,
    D::Error: std::fmt::Debug + Send,
{
    path: String,
    client: Client<C, Body>,
    proxies: Arc<RwLock<HashMap<D::Key, ReverseProxy<C>>>>,
    proxy_list: Arc<RwLock<Vec<D::Key>>>,
    counter: Arc<AtomicUsize>,
    discover: D,
    strategy: LoadBalancingStrategy,
    /// Cached P2C balancer updated when services change
    p2c_balancer: Arc<tokio::sync::Mutex<Option<ProxyService>>>,
}

#[cfg(all(feature = "tls", not(feature = "native-tls")))]
pub type StandardDiscoverableBalancedProxy<D> =
    DiscoverableBalancedProxy<HttpsConnector<HttpConnector>, D>;
#[cfg(all(feature = "native-tls", not(feature = "tls")))]
pub type StandardDiscoverableBalancedProxy<D> =
    DiscoverableBalancedProxy<NativeTlsHttpsConnector<HttpConnector>, D>;
#[cfg(all(feature = "tls", feature = "native-tls"))]
pub type StandardDiscoverableBalancedProxy<D> =
    DiscoverableBalancedProxy<HttpsConnector<HttpConnector>, D>;
#[cfg(not(any(feature = "tls", feature = "native-tls")))]
pub type StandardDiscoverableBalancedProxy<D> = DiscoverableBalancedProxy<HttpConnector, D>;

impl<C, D> DiscoverableBalancedProxy<C, D>
where
    C: Connect + Clone + Send + Sync + 'static,
    D: Discover + Clone + Send + Sync + 'static,
    D::Service: Into<String> + Send,
    D::Key: Clone + std::fmt::Debug + Send + Sync + std::hash::Hash,
    D::Error: std::fmt::Debug + Send,
{
    /// Creates a new discoverable balanced proxy with a custom client and discover implementation.
    /// Uses round-robin load balancing by default.
    pub fn new_with_client<S>(path: S, client: Client<C, Body>, discover: D) -> Self
    where
        S: Into<String>,
    {
        Self::new_with_client_and_strategy(path, client, discover, LoadBalancingStrategy::default())
    }

    /// Creates a new discoverable balanced proxy with a custom client, discover implementation, and load balancing strategy.
    pub fn new_with_client_and_strategy<S>(
        path: S,
        client: Client<C, Body>,
        discover: D,
        strategy: LoadBalancingStrategy,
    ) -> Self
    where
        S: Into<String>,
    {
        let path = path.into();

        Self {
            path,
            client,
            proxies: Arc::new(RwLock::new(HashMap::new())),
            proxy_list: Arc::new(RwLock::new(Vec::new())),
            counter: Arc::new(AtomicUsize::new(0)),
            discover: discover.clone(),
            strategy,
            p2c_balancer: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Get the base path this proxy is configured to handle
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the load balancing strategy being used
    pub fn strategy(&self) -> LoadBalancingStrategy {
        self.strategy
    }

    /// Start the discovery process in the background.
    /// This should be called once to begin monitoring for service changes.
    pub async fn start_discovery(&mut self) {
        let discover = self.discover.clone();
        let proxies = Arc::clone(&self.proxies);
        let proxy_list = Arc::clone(&self.proxy_list);
        let client = self.client.clone();
        let path = self.path.clone();
        let balancer = Arc::clone(&self.p2c_balancer);
        let strategy = self.strategy;

        tokio::spawn(async move {
            use futures_util::future::poll_fn;

            let mut discover = Box::pin(discover);

            loop {
                let change_result =
                    poll_fn(|cx: &mut Context<'_>| discover.as_mut().poll_discover(cx)).await;

                match change_result {
                    Some(Ok(change)) => match change {
                        Change::Insert(key, service) => {
                            let target: String = service.into();
                            debug!("Discovered new service: {:?} -> {}", key, target);

                            let proxy =
                                ReverseProxy::new_with_client(path.clone(), target, client.clone());

                            {
                                let mut proxies_guard = proxies.write().await;
                                let mut list_guard = proxy_list.write().await;

                                proxies_guard.insert(key.clone(), proxy);
                                list_guard.push(key);
                            }
                            if matches!(
                                strategy,
                                LoadBalancingStrategy::P2cPendingRequests
                                    | LoadBalancingStrategy::P2cPeakEwma
                            ) {
                                rebuild_p2c_balancer(&proxies, &balancer, strategy).await;
                            }
                        }
                        Change::Remove(key) => {
                            debug!("Removing service: {:?}", key);

                            {
                                let mut proxies_guard = proxies.write().await;
                                let mut list_guard = proxy_list.write().await;

                                proxies_guard.remove(&key);
                                list_guard.retain(|k| k != &key);
                            }
                            if matches!(
                                strategy,
                                LoadBalancingStrategy::P2cPendingRequests
                                    | LoadBalancingStrategy::P2cPeakEwma
                            ) {
                                rebuild_p2c_balancer(&proxies, &balancer, strategy).await;
                            }
                        }
                    },
                    Some(Err(e)) => {
                        error!("Discovery error: {:?}", e);
                    }
                    None => {
                        warn!("Discovery stream ended");
                        break;
                    }
                }
            }
        });
    }

    /// Get the current number of discovered services
    pub async fn service_count(&self) -> usize {
        self.proxy_list.read().await.len()
    }
}

impl<C, D> Service<axum::http::Request<Body>> for DiscoverableBalancedProxy<C, D>
where
    C: Connect + Clone + Send + Sync + 'static,
    D: Discover + Clone + Send + Sync + 'static,
    D::Service: Into<String> + Send,
    D::Key: Clone + std::fmt::Debug + Send + Sync + std::hash::Hash,
    D::Error: std::fmt::Debug + Send,
{
    type Response = axum::http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let proxies = Arc::clone(&self.proxies);
        let proxy_list = Arc::clone(&self.proxy_list);
        let counter = Arc::clone(&self.counter);
        let balancer = Arc::clone(&self.p2c_balancer);
        let strategy = self.strategy;

        Box::pin(async move {
            match strategy {
                LoadBalancingStrategy::RoundRobin => {
                    // Use round-robin load balancing
                    let proxy_opt = {
                        let list_guard = proxy_list.read().await;
                        if list_guard.is_empty() {
                            None
                        } else {
                            let idx = counter.fetch_add(1, Ordering::SeqCst) % list_guard.len();
                            let key = &list_guard[idx];

                            let proxies_guard = proxies.read().await;
                            proxies_guard.get(key).cloned()
                        }
                    };

                    match proxy_opt {
                        Some(mut proxy) => {
                            trace!("Round-robin proxying via upstream {}", proxy.target());
                            proxy.call(req).await
                        }
                        None => {
                            warn!("No upstream services available");
                            Ok(axum::http::Response::builder()
                                .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                                .body(Body::from("No upstream services available"))
                                .unwrap())
                        }
                    }
                }
                LoadBalancingStrategy::P2cPendingRequests | LoadBalancingStrategy::P2cPeakEwma => {
                    let mut guard = balancer.lock().await;

                    if guard.is_none() {
                        drop(guard);
                        rebuild_p2c_balancer(&proxies, &balancer, strategy).await;
                        guard = balancer.lock().await;
                    }

                    if let Some(service) = guard.as_mut() {
                        let fut = service.call(req);
                        drop(guard);
                        match fut.await {
                            Ok(resp) => Ok(resp),
                            Err(err) => {
                                error!(?err, "Balancer call failed");
                                Ok(axum::http::Response::builder()
                                    .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                                    .body(Body::from("Upstream call failed"))
                                    .unwrap())
                            }
                        }
                    } else {
                        warn!("No upstream services available for P2C balancer");
                        Ok(axum::http::Response::builder()
                            .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("No upstream services available"))
                            .unwrap())
                    }
                }
            }
        })
    }
}

async fn rebuild_p2c_balancer<C, K>(
    proxies: &Arc<RwLock<HashMap<K, ReverseProxy<C>>>>,
    balancer: &Arc<tokio::sync::Mutex<Option<ProxyService>>>,
    strategy: LoadBalancingStrategy,
) where
    C: Connect + Clone + Send + Sync + 'static,
    K: Clone + Eq + std::hash::Hash + Send + Sync + std::fmt::Debug,
{
    let services_vec: Vec<ReverseProxy<C>> = {
        let guard = proxies.read().await;
        guard.values().cloned().collect()
    };

    let mut bal_guard = balancer.lock().await;

    if services_vec.is_empty() {
        *bal_guard = None;
        return;
    }

    let discover = tower::discover::ServiceList::new::<axum::http::Request<Body>>(services_vec);

    let svc = match strategy {
        LoadBalancingStrategy::P2cPendingRequests => {
            let wrapped = PendingRequestsDiscover::new(discover, CompleteOnResponse::default());
            BoxService::new(Balance::new(wrapped))
        }
        LoadBalancingStrategy::P2cPeakEwma => {
            let wrapped = PeakEwmaDiscover::new(
                discover,
                Duration::from_millis(50),
                Duration::from_secs(30),
                CompleteOnResponse::default(),
            );
            BoxService::new(Balance::new(wrapped))
        }
        _ => unreachable!(),
    };

    *bal_guard = Some(svc);
}
