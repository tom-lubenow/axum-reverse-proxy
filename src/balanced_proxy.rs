use axum::body::Body;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tower::discover::{Change, Discover};
use tracing::{debug, error, trace, warn};

use crate::forward::{ProxyConnector, create_http_connector};
use crate::proxy::{ProxyPolicy, ReverseProxy};

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

    /// Apply a [`ProxyPolicy`] to every upstream of this proxy.
    #[must_use]
    pub fn with_policy(mut self, policy: ProxyPolicy) -> Self {
        self.proxies = self
            .proxies
            .into_iter()
            .map(|p| p.with_policy(policy.clone()))
            .collect();
        self
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

/// A single discovered upstream: its proxy plus the load metrics used by the
/// P2C strategies. Metrics travel with the endpoint, so service removals and
/// re-inserts can never attribute one endpoint's load to another.
struct Endpoint<C: Connect + Clone + Send + Sync + 'static> {
    proxy: ReverseProxy<C>,
    metrics: Arc<ServiceMetrics>,
}

impl<C: Connect + Clone + Send + Sync + 'static> Clone for Endpoint<C> {
    fn clone(&self) -> Self {
        Self {
            proxy: self.proxy.clone(),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

type Snapshot<C> = Arc<Vec<Endpoint<C>>>;

/// A balanced proxy that supports dynamic service discovery.
///
/// This proxy uses the tower::discover trait to dynamically add and remove
/// upstream services. Services are load-balanced using a configurable strategy.
///
/// Features:
/// - High-performance request handling with minimal overhead
/// - Atomic service updates that don't block ongoing requests
/// - Round-robin and Power-of-Two-Choices load balancing
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
    snapshot: Arc<std::sync::RwLock<Snapshot<C>>>,
    counter: Arc<AtomicUsize>,
    discover: D,
    strategy: LoadBalancingStrategy,
    policy: ProxyPolicy,
    discovery_started: Arc<AtomicBool>,
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
        Self {
            path: path.into(),
            client,
            snapshot: Arc::new(std::sync::RwLock::new(Arc::new(Vec::new()))),
            counter: Arc::new(AtomicUsize::new(0)),
            discover,
            strategy,
            policy: ProxyPolicy::default(),
            discovery_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Apply a [`ProxyPolicy`] to every discovered upstream.
    ///
    /// Must be called before [`start_discovery`](Self::start_discovery);
    /// endpoints discovered earlier keep the policy in effect at the time they
    /// were added.
    #[must_use]
    pub fn with_policy(mut self, policy: ProxyPolicy) -> Self {
        self.policy = policy;
        self
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
    ///
    /// Subsequent calls (on this instance or any clone) are no-ops: the
    /// discovery stream is consumed by a single background task.
    pub async fn start_discovery(&self) {
        if self.discovery_started.swap(true, Ordering::SeqCst) {
            warn!("start_discovery called more than once; ignoring");
            return;
        }

        let discover = self.discover.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let client = self.client.clone();
        let path = self.path.clone();
        let policy = self.policy.clone();

        tokio::spawn(async move {
            use futures_util::future::poll_fn;

            let mut discover = Box::pin(discover);
            // Insertion-ordered list of discovered endpoints, owned by this
            // task. The service snapshot is rebuilt from it on every change.
            let mut endpoints: Vec<(D::Key, Endpoint<C>)> = Vec::new();

            loop {
                let change_result =
                    poll_fn(|cx: &mut Context<'_>| discover.as_mut().poll_discover(cx)).await;

                match change_result {
                    Some(Ok(change)) => {
                        match change {
                            Change::Insert(key, service) => {
                                let target: String = service.into();
                                debug!("Discovered service: {:?} -> {}", key, target);

                                let endpoint = Endpoint {
                                    proxy: ReverseProxy::new_with_client(
                                        path.clone(),
                                        target,
                                        client.clone(),
                                    )
                                    .with_policy(policy.clone()),
                                    metrics: Arc::new(ServiceMetrics::new()),
                                };

                                // Per tower's Discover contract, an Insert for
                                // an existing key replaces that service.
                                if let Some(existing) =
                                    endpoints.iter_mut().find(|(k, _)| *k == key)
                                {
                                    existing.1 = endpoint;
                                } else {
                                    endpoints.push((key, endpoint));
                                }
                            }
                            Change::Remove(key) => {
                                debug!("Removing service: {:?}", key);
                                endpoints.retain(|(k, _)| *k != key);
                            }
                        }

                        let new_snapshot: Snapshot<C> =
                            Arc::new(endpoints.iter().map(|(_, e)| e.clone()).collect());
                        *snapshot.write().unwrap() = new_snapshot;
                    }
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
        self.snapshot.read().unwrap().len()
    }

    fn current_snapshot(&self) -> Snapshot<C> {
        Arc::clone(&self.snapshot.read().unwrap())
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
        let snapshot = self.current_snapshot();
        let counter = Arc::clone(&self.counter);
        let strategy = self.strategy;

        Box::pin(async move {
            if snapshot.is_empty() {
                warn!("No upstream services available");
                return Ok(axum::http::Response::builder()
                    .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from("No upstream services available"))
                    .unwrap());
            }

            let idx = match strategy {
                LoadBalancingStrategy::RoundRobin => {
                    counter.fetch_add(1, Ordering::Relaxed) % snapshot.len()
                }
                LoadBalancingStrategy::P2cPendingRequests | LoadBalancingStrategy::P2cPeakEwma => {
                    p2c_select(&snapshot, strategy)
                }
            };
            let endpoint = &snapshot[idx];

            // Track pending requests for the P2C pending-requests strategy;
            // the guard decrements on drop (including panics/cancellation).
            let _pending_guard = if strategy == LoadBalancingStrategy::P2cPendingRequests {
                endpoint
                    .metrics
                    .pending_requests
                    .fetch_add(1, Ordering::Relaxed);
                Some(PendingRequestGuard {
                    metrics: Arc::clone(&endpoint.metrics),
                })
            } else {
                None
            };

            let start = Instant::now();
            let mut proxy = endpoint.proxy.clone();
            let result = proxy.call(req).await;

            if strategy == LoadBalancingStrategy::P2cPeakEwma {
                endpoint.metrics.update_ewma(start.elapsed());
            }

            result
        })
    }
}

/// Pick an endpoint index using Power of Two Choices: sample two distinct
/// endpoints at random and take the one with the lower load.
fn p2c_select<C: Connect + Clone + Send + Sync + 'static>(
    snapshot: &[Endpoint<C>],
    strategy: LoadBalancingStrategy,
) -> usize {
    if snapshot.len() == 1 {
        return 0;
    }
    let mut rng = rand::rng();
    let idx1 = rng.random_range(0..snapshot.len());
    let idx2 = loop {
        let i = rng.random_range(0..snapshot.len());
        if i != idx1 {
            break i;
        }
    };

    let load1 = snapshot[idx1].metrics.load(strategy);
    let load2 = snapshot[idx2].metrics.load(strategy);

    if load1 <= load2 { idx1 } else { idx2 }
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

    fn load(&self, strategy: LoadBalancingStrategy) -> u64 {
        match strategy {
            LoadBalancingStrategy::P2cPendingRequests => {
                self.pending_requests.load(Ordering::Relaxed) as u64
            }
            LoadBalancingStrategy::P2cPeakEwma => {
                // Apply decay based on time since last update
                let last_update = *self.last_update.lock().unwrap();
                let elapsed = last_update.elapsed();

                // Simple exponential decay: reduce by ~50% every 5 seconds
                let current = self.peak_ewma_micros.load(Ordering::Relaxed);
                let decay_factor = (-elapsed.as_secs_f64() / 5.0).exp();
                (current as f64 * decay_factor) as u64
            }
            LoadBalancingStrategy::RoundRobin => {
                unreachable!("load() is only used by P2C strategies")
            }
        }
    }

    fn update_ewma(&self, latency: Duration) {
        let latency_micros = latency.as_micros() as u64;

        // Update with exponential weighted moving average
        // Using compare-and-swap loop for lock-free update
        loop {
            let current = self.peak_ewma_micros.load(Ordering::Relaxed);

            // If this is the first measurement, just set it
            if current == 0 {
                if self
                    .peak_ewma_micros
                    .compare_exchange(0, latency_micros, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    *self.last_update.lock().unwrap() = Instant::now();
                    break;
                }
                continue;
            }

            // Apply decay based on time since last update
            let mut last_update_guard = self.last_update.lock().unwrap();
            let elapsed = last_update_guard.elapsed();

            // Decay factor: reduce by ~50% every 5 seconds
            let decay_factor = (-elapsed.as_secs_f64() / 5.0).exp();
            let decayed_current = (current as f64 * decay_factor) as u64;

            // Peak EWMA: take the maximum of the decayed value and the new measurement
            let peak = decayed_current.max(latency_micros);

            // EWMA with alpha = 0.25 (25% new value, 75% old value)
            // This gives more weight to recent measurements
            let ewma = ((peak as f64 * 0.25) + (decayed_current as f64 * 0.75)) as u64;

            if self
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

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_util::client::legacy::connect::HttpConnector;

    fn endpoint(pending: usize, ewma_micros: u64) -> Endpoint<HttpConnector> {
        let client =
            Client::builder(hyper_util::rt::TokioExecutor::new()).build(HttpConnector::new());
        let metrics = ServiceMetrics::new();
        metrics.pending_requests.store(pending, Ordering::Relaxed);
        metrics
            .peak_ewma_micros
            .store(ewma_micros, Ordering::Relaxed);
        Endpoint {
            proxy: ReverseProxy::new_with_client("/", "http://127.0.0.1:1", client),
            metrics: Arc::new(metrics),
        }
    }

    #[test]
    fn p2c_single_endpoint_is_always_selected() {
        let endpoints = vec![endpoint(100, 0)];
        assert_eq!(
            p2c_select(&endpoints, LoadBalancingStrategy::P2cPendingRequests),
            0
        );
    }

    #[test]
    fn p2c_pending_requests_prefers_less_loaded_endpoint() {
        // With exactly two endpoints, P2C always compares both, so the choice
        // is deterministic: the endpoint with fewer pending requests wins.
        let endpoints = vec![endpoint(10, 0), endpoint(0, 0)];
        for _ in 0..100 {
            assert_eq!(
                p2c_select(&endpoints, LoadBalancingStrategy::P2cPendingRequests),
                1
            );
        }
    }

    #[test]
    fn p2c_peak_ewma_prefers_lower_latency_endpoint() {
        let endpoints = vec![endpoint(0, 500_000), endpoint(0, 1_000)];
        for _ in 0..100 {
            assert_eq!(
                p2c_select(&endpoints, LoadBalancingStrategy::P2cPeakEwma),
                1
            );
        }
    }

    #[test]
    fn pending_request_guard_decrements_on_drop() {
        let metrics = Arc::new(ServiceMetrics::new());
        metrics.pending_requests.fetch_add(1, Ordering::Relaxed);
        {
            let _guard = PendingRequestGuard {
                metrics: Arc::clone(&metrics),
            };
            assert_eq!(metrics.pending_requests.load(Ordering::Relaxed), 1);
        }
        assert_eq!(metrics.pending_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ewma_first_measurement_sets_latency_directly() {
        let metrics = ServiceMetrics::new();
        metrics.update_ewma(Duration::from_millis(50));
        assert_eq!(
            metrics.peak_ewma_micros.load(Ordering::Relaxed),
            50_000,
            "first measurement should be recorded as-is"
        );
    }

    #[test]
    fn ewma_load_decays_over_time() {
        let metrics = ServiceMetrics::new();
        metrics.peak_ewma_micros.store(100_000, Ordering::Relaxed);
        *metrics.last_update.lock().unwrap() = Instant::now() - Duration::from_secs(10);
        let load = metrics.load(LoadBalancingStrategy::P2cPeakEwma);
        // ~50% decay every 5s => after 10s the load should be well below half
        assert!(
            load < 20_000,
            "expected decayed load well below stored value, got {load}"
        );
    }
}
