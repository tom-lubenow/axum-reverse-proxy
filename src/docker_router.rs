use crate::{DiscoverableBalancedProxy, DockerDiscovery, LoadBalancingStrategy};
use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::RwLock;
use tower::{BoxError, Service, discover::Change};
use tracing::{debug, info, warn};

/// A router that dynamically creates proxies for Docker-discovered services
pub struct DockerRouter<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    client: Client<C, Body>,
    strategy: LoadBalancingStrategy,
    proxies: Arc<RwLock<HashMap<String, DiscoverableBalancedProxy<C, SingleServiceDiscovery>>>>,
    #[allow(dead_code)]
    discovery_task: Option<tokio::task::JoinHandle<()>>,
}

impl<C> DockerRouter<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Create a new DockerRouter that discovers all services
    pub async fn everything(client: Client<C, Body>) -> Result<Self, BoxError> {
        let discovery = DockerDiscovery::everything().await?;
        Self::new(client, discovery).await
    }

    /// Create a new DockerRouter with custom discovery
    pub async fn new(
        client: Client<C, Body>,
        mut discovery: DockerDiscovery,
    ) -> Result<Self, BoxError> {
        let proxies = Arc::new(RwLock::new(HashMap::new()));
        let proxies_clone = proxies.clone();
        let client_clone = client.clone();
        let strategy = LoadBalancingStrategy::RoundRobin;

        // Start discovery task
        let discovery_task = tokio::spawn(async move {
            while let Some(change_result) = discovery.next().await {
                match change_result {
                    Ok(Change::Insert(path, service)) => {
                        info!("Docker router: adding route {} -> {}", path, service);

                        // Create a single-service discovery for this specific service
                        let service_discovery = SingleServiceDiscovery::new(service.clone());

                        // Create a proxy for this specific path
                        let mut proxy = DiscoverableBalancedProxy::new_with_client_and_strategy(
                            &path,
                            client_clone.clone(),
                            service_discovery,
                            strategy,
                        );

                        proxy.start_discovery().await;

                        proxies_clone.write().await.insert(path, proxy);
                    }
                    Ok(Change::Remove(path)) => {
                        info!("Docker router: removing route {}", path);
                        proxies_clone.write().await.remove(&path);
                    }
                    Err(e) => {
                        warn!("Docker discovery error: {}", e);
                    }
                }
            }
        });

        Ok(Self {
            client,
            strategy,
            proxies,
            discovery_task: Some(discovery_task),
        })
    }

    /// Set the load balancing strategy
    pub fn with_strategy(mut self, strategy: LoadBalancingStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

impl<C> Service<Request> for DockerRouter<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let proxies = self.proxies.clone();

        Box::pin(async move {
            let path = req.uri().path();

            // Find the best matching proxy
            let proxies_read = proxies.read().await;

            // Try to find exact match first
            for (prefix, proxy) in proxies_read.iter() {
                if path.starts_with(prefix) {
                    debug!("Routing {} to proxy with prefix {}", path, prefix);
                    let mut proxy = proxy.clone();
                    return proxy.call(req).await.map_err(|_| unreachable!());
                }
            }

            // No matching proxy found
            Ok((StatusCode::NOT_FOUND, "No service found for this path").into_response())
        })
    }
}

impl<C> Clone for DockerRouter<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            strategy: self.strategy,
            proxies: self.proxies.clone(),
            discovery_task: None, // Don't clone the task handle
        }
    }
}

/// A simple discovery stream that emits a single service
#[derive(Clone)]
struct SingleServiceDiscovery {
    service: String,
    emitted: Arc<RwLock<bool>>,
}

impl SingleServiceDiscovery {
    fn new(service: String) -> Self {
        Self {
            service,
            emitted: Arc::new(RwLock::new(false)),
        }
    }
}

impl futures_util::Stream for SingleServiceDiscovery {
    type Item = Result<Change<usize, String>, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let emitted = self.emitted.clone();
        let service = self.service.clone();

        let fut = async move {
            let mut emitted = emitted.write().await;
            if !*emitted {
                *emitted = true;
                Some(Ok(Change::Insert(0, service)))
            } else {
                None
            }
        };

        let mut fut = Box::pin(fut);
        fut.as_mut().poll(cx)
    }
}

/// Extension trait for axum::Router to easily integrate Docker discovery
pub trait DockerRouterExt {
    /// Add Docker-based service discovery that automatically creates routes
    fn docker_proxy<C>(self, docker_router: DockerRouter<C>) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static;
}

impl DockerRouterExt for axum::Router {
    fn docker_proxy<C>(self, docker_router: DockerRouter<C>) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        self.fallback_service(docker_router)
    }
}
