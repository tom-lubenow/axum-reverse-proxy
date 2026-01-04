use axum::body::Body;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tower::discover::{Change, Discover};
use tracing::{debug, error, trace, warn};

use crate::forward::{ProxyConnector, create_http_connector};
use crate::proxy::ReverseProxy;

// For custom P2C implementation
use rand::Rng;

/// Load balancing strategy for distributing requests across discovered services
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadBalancingStrategy {
    /// Simple round-robin distribution (default)
    #[default]
    RoundRobin,
    /// Power of Two Choices with pending request count as load metric
    P2cPendingRequests,
    /// Power of Two Choices with peak EWMA latency as load metric
    P2cPeakEwma,
}

#[derive(Clone)]
pub struct BalancedProxy<C: Connect + Clone + Send + Sync + 'static> {
    path: String,
    proxies: Vec<ReverseProxy<C>>,
    counter: Arc<AtomicUsize>,
}

pub type StandardBalancedProxy = BalancedProxy<ProxyConnector>;

impl StandardBalancedProxy {
    pub fn new<S>(path: S, targets: Vec<S>) -> Self
    where
        S: Into<String> + Clone,
    {
        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(create_http_connector());

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
            let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.proxies.len();
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
///
/// Features:
/// - High-performance request handling with minimal overhead
/// - Atomic service updates that don't block ongoing requests
/// - Efficient round-robin load balancing
/// - Zero-downtime service discovery changes
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
    proxies_snapshot: Arc<std::sync::RwLock<Arc<Vec<ReverseProxy<C>>>>>,
    proxy_keys: Arc<tokio::sync::RwLock<HashMap<D::Key, usize>>>, // key -> index mapping
    counter: Arc<AtomicUsize>,
    discover: D,
    strategy: LoadBalancingStrategy,
    // Custom P2C balancer for strategies that need it
    p2c_balancer: Option<Arc<CustomP2cBalancer<C>>>,
}

pub type StandardDiscoverableBalancedProxy<D> = DiscoverableBalancedProxy<ProxyConnector, D>;

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
        let proxies_snapshot = Arc::new(std::sync::RwLock::new(Arc::new(Vec::new())));

        // Create P2C balancer if needed
        let p2c_balancer = match strategy {
            LoadBalancingStrategy::P2cPendingRequests | LoadBalancingStrategy::P2cPeakEwma => {
                Some(Arc::new(CustomP2cBalancer::new(
                    strategy,
                    Arc::clone(&proxies_snapshot),
                )))
            }
            LoadBalancingStrategy::RoundRobin => None,
        };

        Self {
            path,
            client,
            proxies_snapshot,
            proxy_keys: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            counter: Arc::new(AtomicUsize::new(0)),
            discover: discover.clone(),
            strategy,
            p2c_balancer,
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
        let proxies_snapshot = Arc::clone(&self.proxies_snapshot);
        let proxy_keys = Arc::clone(&self.proxy_keys);
        let client = self.client.clone();
        let path = self.path.clone();

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
                                let mut keys_guard = proxy_keys.write().await;

                                // Get current snapshot and create new one with added service
                                let current_snapshot = {
                                    let snapshot_guard = proxies_snapshot.read().unwrap();
                                    Arc::clone(&*snapshot_guard)
                                };

                                let mut new_proxies = (*current_snapshot).clone();
                                let index = new_proxies.len();
                                new_proxies.push(proxy);
                                keys_guard.insert(key, index);

                                // Atomically update the snapshot
                                {
                                    let mut snapshot_guard = proxies_snapshot.write().unwrap();
                                    *snapshot_guard = Arc::new(new_proxies);
                                }
                            }
                        }
                        Change::Remove(key) => {
                            debug!("Removing service: {:?}", key);

                            {
                                let mut keys_guard = proxy_keys.write().await;

                                if let Some(index) = keys_guard.remove(&key) {
                                    // Get current snapshot and create new one with removed service
                                    let current_snapshot = {
                                        let snapshot_guard = proxies_snapshot.read().unwrap();
                                        Arc::clone(&*snapshot_guard)
                                    };

                                    let mut new_proxies = (*current_snapshot).clone();
                                    new_proxies.remove(index);

                                    // Update indices for all keys after the removed index
                                    for (_, idx) in keys_guard.iter_mut() {
                                        if *idx > index {
                                            *idx -= 1;
                                        }
                                    }

                                    // Atomically update the snapshot
                                    {
                                        let mut snapshot_guard = proxies_snapshot.write().unwrap();
                                        *snapshot_guard = Arc::new(new_proxies);
                                    }
                                }
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
        let snapshot = {
            let guard = self.proxies_snapshot.read().unwrap();
            Arc::clone(&*guard)
        };
        snapshot.len()
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
        // Get current proxy snapshot
        let proxies_snapshot = {
            let guard = self.proxies_snapshot.read().unwrap();
            Arc::clone(&*guard)
        };
        let counter = Arc::clone(&self.counter);
        let strategy = self.strategy;
        let p2c_balancer = self.p2c_balancer.clone();

        Box::pin(async move {
            match strategy {
                LoadBalancingStrategy::RoundRobin => {
                    // Round-robin load balancing
                    if proxies_snapshot.is_empty() {
                        warn!("No upstream services available");
                        Ok(axum::http::Response::builder()
                            .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("No upstream services available"))
                            .unwrap())
                    } else {
                        let idx = counter.fetch_add(1, Ordering::Relaxed) % proxies_snapshot.len();
                        let mut proxy = proxies_snapshot[idx].clone();
                        proxy.call(req).await
                    }
                }
                LoadBalancingStrategy::P2cPendingRequests | LoadBalancingStrategy::P2cPeakEwma => {
                    // Use the custom P2C balancer
                    if let Some(balancer) = p2c_balancer {
                        balancer.call_with_p2c(req).await
                    } else {
                        // Fallback to error if balancer is not available
                        error!("P2C balancer not available for strategy {:?}", strategy);
                        Ok(axum::http::Response::builder()
                            .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("P2C balancer not available"))
                            .unwrap())
                    }
                }
            }
        })
    }
}

/// Custom P2C load balancer that uses atomic operations for low-contention metrics tracking
struct CustomP2cBalancer<C: Connect + Clone + Send + Sync + 'static> {
    strategy: LoadBalancingStrategy,
    proxies_snapshot: Arc<std::sync::RwLock<Arc<Vec<ReverseProxy<C>>>>>,
    /// Metrics for each service, indexed by position in proxies_snapshot
    /// We use Arc<Vec<Arc<ServiceMetrics>>> to allow concurrent access with minimal locking
    metrics: Arc<std::sync::RwLock<Arc<Vec<Arc<ServiceMetrics>>>>>,
}

impl<C: Connect + Clone + Send + Sync + 'static> CustomP2cBalancer<C> {
    fn new(
        strategy: LoadBalancingStrategy,
        proxies_snapshot: Arc<std::sync::RwLock<Arc<Vec<ReverseProxy<C>>>>>,
    ) -> Self {
        let initial_metrics = Arc::new(Vec::new());
        Self {
            strategy,
            proxies_snapshot,
            metrics: Arc::new(std::sync::RwLock::new(initial_metrics)),
        }
    }

    async fn call_with_p2c(
        &self,
        req: axum::http::Request<Body>,
    ) -> Result<axum::http::Response<Body>, Infallible> {
        // Get current proxy snapshot
        let proxies = {
            let guard = self.proxies_snapshot.read().unwrap();
            Arc::clone(&*guard)
        };

        if proxies.is_empty() {
            return Ok(axum::http::Response::builder()
                .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("No upstream services available"))
                .unwrap());
        }

        // Ensure metrics vector is up to date
        self.ensure_metrics_size(proxies.len());

        // Get metrics snapshot
        let metrics = {
            let guard = self.metrics.read().unwrap();
            Arc::clone(&*guard)
        };

        // P2C: Pick two random services and choose the one with lower load
        let selected_idx = if proxies.len() == 1 {
            0
        } else {
            let mut rng = rand::rng();
            let idx1 = rng.random_range(0..proxies.len());
            let idx2 = loop {
                let i = rng.random_range(0..proxies.len());
                if i != idx1 {
                    break i;
                }
            };

            // Compare load metrics based on strategy
            let load1 = self.get_load(&metrics[idx1]);
            let load2 = self.get_load(&metrics[idx2]);

            if load1 <= load2 { idx1 } else { idx2 }
        };

        // Track request start for pending requests
        let request_guard = if matches!(self.strategy, LoadBalancingStrategy::P2cPendingRequests) {
            metrics[selected_idx]
                .pending_requests
                .fetch_add(1, Ordering::Relaxed);
            Some(PendingRequestGuard {
                metrics: Arc::clone(&metrics[selected_idx]),
            })
        } else {
            None
        };

        // Record start time for latency tracking
        let start = Instant::now();

        // Make the actual request
        let mut proxy = proxies[selected_idx].clone();
        let result = proxy.call(req).await;

        // Update latency metrics for EWMA strategy
        if matches!(self.strategy, LoadBalancingStrategy::P2cPeakEwma) {
            let latency = start.elapsed();
            self.update_ewma(&metrics[selected_idx], latency);
        }

        // Request guard will decrement pending count when dropped
        drop(request_guard);

        result
    }

    fn ensure_metrics_size(&self, size: usize) {
        let mut metrics_guard = self.metrics.write().unwrap();
        let current_metrics = Arc::clone(&*metrics_guard);

        if current_metrics.len() != size {
            let mut new_metrics = Vec::with_capacity(size);

            // Copy existing metrics
            for (i, metric) in current_metrics.iter().enumerate() {
                if i < size {
                    new_metrics.push(Arc::clone(metric));
                }
            }

            // Add new metrics if needed
            while new_metrics.len() < size {
                new_metrics.push(Arc::new(ServiceMetrics::new()));
            }

            *metrics_guard = Arc::new(new_metrics);
        }
    }

    fn get_load(&self, metrics: &ServiceMetrics) -> u64 {
        match self.strategy {
            LoadBalancingStrategy::P2cPendingRequests => {
                metrics.pending_requests.load(Ordering::Relaxed) as u64
            }
            LoadBalancingStrategy::P2cPeakEwma => {
                // Apply decay based on time since last update
                let last_update = *metrics.last_update.lock().unwrap();
                let elapsed = last_update.elapsed();

                // Simple exponential decay: reduce by ~50% every 5 seconds
                let current = metrics.peak_ewma_micros.load(Ordering::Relaxed);
                let decay_factor = (-elapsed.as_secs_f64() / 5.0).exp();
                (current as f64 * decay_factor) as u64
            }
            _ => unreachable!("CustomP2cBalancer should only be used with P2C strategies"),
        }
    }

    fn update_ewma(&self, metrics: &ServiceMetrics, latency: Duration) {
        let latency_micros = latency.as_micros() as u64;

        // Update with exponential weighted moving average
        // Using compare-and-swap loop for lock-free update
        loop {
            let current = metrics.peak_ewma_micros.load(Ordering::Relaxed);

            // If this is the first measurement, just set it
            if current == 0 {
                if metrics
                    .peak_ewma_micros
                    .compare_exchange(0, latency_micros, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    *metrics.last_update.lock().unwrap() = Instant::now();
                    break;
                }
                continue;
            }

            // Apply decay based on time since last update
            let mut last_update_guard = metrics.last_update.lock().unwrap();
            let elapsed = last_update_guard.elapsed();

            // Decay factor: reduce by ~50% every 5 seconds
            let decay_factor = (-elapsed.as_secs_f64() / 5.0).exp();
            let decayed_current = (current as f64 * decay_factor) as u64;

            // Peak EWMA: take the maximum of the decayed value and the new measurement
            let peak = decayed_current.max(latency_micros);

            // EWMA with alpha = 0.25 (25% new value, 75% old value)
            // This gives more weight to recent measurements
            let ewma = ((peak as f64 * 0.25) + (decayed_current as f64 * 0.75)) as u64;

            if metrics
                .peak_ewma_micros
                .compare_exchange(current, ewma, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // Update last update time
                *last_update_guard = Instant::now();
                break;
            }
            drop(last_update_guard); // Release lock before retrying
        }
    }
}

/// RAII guard to decrement pending request count when request completes
struct PendingRequestGuard {
    metrics: Arc<ServiceMetrics>,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.metrics
            .pending_requests
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Metrics for a single service used in P2C load balancing
#[derive(Debug)]
struct ServiceMetrics {
    /// Number of pending requests (for P2cPendingRequests strategy)
    pending_requests: AtomicUsize,
    /// Peak EWMA latency in microseconds (for P2cPeakEwma strategy)
    peak_ewma_micros: AtomicU64,
    /// Last update time for EWMA decay calculation
    last_update: std::sync::Mutex<Instant>,
}

impl ServiceMetrics {
    fn new() -> Self {
        Self {
            pending_requests: AtomicUsize::new(0),
            peak_ewma_micros: AtomicU64::new(0),
            last_update: std::sync::Mutex::new(Instant::now()),
        }
    }
}
