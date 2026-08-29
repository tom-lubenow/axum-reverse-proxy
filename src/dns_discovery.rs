use futures_util::stream::Stream;
use hickory_resolver::{
    Resolver,
    config::{ResolverConfig, ResolverOpts},
    name_server::TokioConnectionProvider,
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tower::discover::Change;
use tracing::{debug, error, trace};

type TokioResolver = Resolver<TokioConnectionProvider>;
type DiscoveryResult = Result<Change<IpAddr, String>, Box<dyn std::error::Error + Send + Sync>>;
type DiscoveryReceiver = mpsc::UnboundedReceiver<DiscoveryResult>;
type DiscoverySender = mpsc::UnboundedSender<DiscoveryResult>;

/// Configuration for DNS-based service discovery
#[derive(Debug, Clone)]
pub struct DnsDiscoveryConfig {
    /// The hostname to resolve
    pub hostname: String,
    /// Port to use for discovered services
    pub port: u16,
    /// How often to re-resolve the hostname (default: 30 seconds)
    pub refresh_interval: Duration,
    /// Whether to use HTTPS scheme for discovered services (default: false)
    pub use_https: bool,
    /// DNS resolver configuration (uses system default if None)
    pub resolver_config: Option<ResolverConfig>,
    /// DNS resolver options (uses default if None)
    pub resolver_opts: Option<ResolverOpts>,
}

impl DnsDiscoveryConfig {
    /// Create a new DNS discovery configuration
    pub fn new<S: Into<String>>(hostname: S, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            port,
            refresh_interval: Duration::from_secs(30),
            use_https: false,
            resolver_config: None,
            resolver_opts: None,
        }
    }

    /// Set the refresh interval for DNS resolution
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Enable HTTPS scheme for discovered services
    pub fn with_https(mut self, use_https: bool) -> Self {
        self.use_https = use_https;
        self
    }

    /// Set custom DNS resolver configuration
    pub fn with_resolver_config(mut self, config: ResolverConfig) -> Self {
        self.resolver_config = Some(config);
        self
    }

    /// Set custom DNS resolver options
    pub fn with_resolver_opts(mut self, opts: ResolverOpts) -> Self {
        self.resolver_opts = Some(opts);
        self
    }
}

/// Build a resolver honouring the config's optional `resolver_config` and
/// `resolver_opts`, independently of one another. Falls back to the system
/// configuration when no `resolver_config` is given.
fn build_resolver(
    config: &DnsDiscoveryConfig,
) -> Result<TokioResolver, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = match &config.resolver_config {
        Some(resolver_config) => Resolver::builder_with_config(
            resolver_config.clone(),
            TokioConnectionProvider::default(),
        ),
        None => Resolver::builder_tokio()
            .map_err(|e| format!("Failed to create resolver from system config: {e}"))?,
    };
    if let Some(opts) = &config.resolver_opts {
        builder = builder.with_options(opts.clone());
    }
    Ok(builder.build())
}

/// DNS-based service discovery implementation
///
/// This discoverer resolves A/AAAA records for a hostname and treats each IP address
/// as a separate service endpoint. It periodically re-resolves the hostname to detect
/// changes in the available services.
#[derive(Clone)]
pub struct DnsDiscovery {
    receiver: Arc<tokio::sync::Mutex<DiscoveryReceiver>>,
    _handle: Arc<tokio::task::JoinHandle<()>>,
}

impl DnsDiscovery {
    /// Create a new DNS discovery instance
    pub fn new(
        config: DnsDiscoveryConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let resolver = build_resolver(&config)?;

        let (sender, receiver) = mpsc::unbounded_channel();

        // Spawn background task to handle DNS resolution
        let handle = tokio::spawn(Self::discovery_task(config, resolver, sender));

        Ok(Self {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            _handle: Arc::new(handle),
        })
    }

    /// Background task that performs periodic DNS resolution
    async fn discovery_task(
        config: DnsDiscoveryConfig,
        resolver: TokioResolver,
        sender: DiscoverySender,
    ) {
        let mut current_services = HashMap::new();
        let mut interval = interval(config.refresh_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Perform initial resolution
        if let Err(e) =
            Self::resolve_and_send(&config, &resolver, &mut current_services, &sender).await
        {
            error!("Initial DNS resolution failed: {}", e);
            let _ = sender.send(Err(e));
        }

        // Periodic resolution
        loop {
            interval.tick().await;

            if let Err(e) =
                Self::resolve_and_send(&config, &resolver, &mut current_services, &sender).await
            {
                error!("DNS resolution refresh failed: {}", e);
                let _ = sender.send(Err(e));
            }
        }
    }

    /// Perform DNS resolution and send changes
    async fn resolve_and_send(
        config: &DnsDiscoveryConfig,
        resolver: &TokioResolver,
        current_services: &mut HashMap<IpAddr, String>,
        sender: &DiscoverySender,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        trace!("Resolving DNS for hostname: {}", config.hostname);

        let lookup_result = resolver
            .lookup_ip(&config.hostname)
            .await
            .map_err(|e| format!("DNS lookup failed for {}: {}", config.hostname, e))?;

        let mut new_services = HashMap::new();
        let scheme = if config.use_https { "https" } else { "http" };

        // Build new service map from DNS results
        for ip in lookup_result.iter() {
            let service_url = format!("{}://{}:{}", scheme, ip, config.port);
            new_services.insert(ip, service_url);
        }

        // Find services to remove (in current but not in new)
        for ip in current_services.keys() {
            if !new_services.contains_key(ip) {
                debug!("Removing service: {}", ip);
                let _ = sender.send(Ok(Change::Remove(*ip)));
            }
        }

        // Find services to add (in new but not in current)
        for (ip, service_url) in &new_services {
            if !current_services.contains_key(ip) {
                debug!("Adding service: {} -> {}", ip, service_url);
                let _ = sender.send(Ok(Change::Insert(*ip, service_url.clone())));
            }
        }

        // Update current services
        *current_services = new_services;

        Ok(())
    }
}

impl Stream for DnsDiscovery {
    type Item = DiscoveryResult;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Try to lock the receiver without blocking
        match self.receiver.try_lock() {
            Ok(mut receiver) => receiver.poll_recv(cx),
            Err(_) => {
                // Another clone holds the lock. Wake ourselves so this task is
                // polled again rather than sleeping with no registered waker.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

// Note: DnsDiscovery automatically implements Discover via Tower's blanket implementation
// since it implements Stream<Item = Result<Change<Key, Service>, Error>>

/// A simpler DNS discoverer that performs one-time resolution
///
/// This is useful when you want to resolve a hostname once at startup
/// rather than continuously monitoring for changes.
pub struct StaticDnsDiscovery {
    receiver: DiscoveryReceiver,
    _handle: tokio::task::JoinHandle<()>,
}

impl StaticDnsDiscovery {
    /// Create a new static DNS discovery instance
    pub fn new(
        config: DnsDiscoveryConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let resolver = build_resolver(&config)?;

        let (sender, receiver) = mpsc::unbounded_channel();

        // Spawn task to perform one-time resolution
        let handle = tokio::spawn(Self::static_discovery_task(config, resolver, sender));

        Ok(Self {
            receiver,
            _handle: handle,
        })
    }

    /// Task that performs one-time DNS resolution
    async fn static_discovery_task(
        config: DnsDiscoveryConfig,
        resolver: TokioResolver,
        sender: DiscoverySender,
    ) {
        trace!(
            "Performing static DNS resolution for hostname: {}",
            config.hostname
        );

        match resolver.lookup_ip(&config.hostname).await {
            Ok(lookup_result) => {
                let scheme = if config.use_https { "https" } else { "http" };

                for ip in lookup_result.iter() {
                    let service_url = format!("{}://{}:{}", scheme, ip, config.port);
                    debug!(
                        "Discovered service: {} -> {}://{}:{}",
                        ip, scheme, ip, config.port
                    );
                    let _ = sender.send(Ok(Change::Insert(ip, service_url)));
                }
            }
            Err(e) => {
                error!(
                    "Static DNS resolution failed for {}: {}",
                    config.hostname, e
                );
                let _ = sender.send(Err(format!(
                    "DNS lookup failed for {}: {}",
                    config.hostname, e
                )
                .into()));
            }
        }
    }
}

impl Stream for StaticDnsDiscovery {
    type Item = DiscoveryResult;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

// Note: StaticDnsDiscovery automatically implements Discover via Tower's blanket implementation
// since it implements Stream<Item = Result<Change<Key, Service>, Error>>

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_dns_discovery_config() {
        let config = DnsDiscoveryConfig::new("example.com", 8080)
            .with_refresh_interval(Duration::from_secs(60))
            .with_https(true);

        assert_eq!(config.hostname, "example.com");
        assert_eq!(config.port, 8080);
        assert_eq!(config.refresh_interval, Duration::from_secs(60));
        assert!(config.use_https);
    }

    #[tokio::test]
    async fn test_static_dns_discovery_creation() {
        let config = DnsDiscoveryConfig::new("localhost", 8080);
        let discovery = StaticDnsDiscovery::new(config);
        assert!(discovery.is_ok());
    }

    #[tokio::test]
    async fn test_dns_discovery_creation() {
        let config = DnsDiscoveryConfig::new("localhost", 8080);
        let discovery = DnsDiscovery::new(config);
        assert!(discovery.is_ok());
    }
}
