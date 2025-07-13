use bollard::{
    Docker,
    container::ListContainersOptions,
    service::{ContainerSummary, EventMessage, EventMessageTypeEnum},
    system::EventsOptions,
};
use futures_util::{Stream, StreamExt, stream::BoxStream};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::RwLock;
use tower::{BoxError, discover::Change};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct DockerDiscoveryConfig {
    /// Label prefix for configuration (e.g., "axum-proxy")
    pub label_prefix: String,
    /// If true, discovers all containers regardless of labels
    pub discover_all: bool,
    /// Required labels for container discovery (if discover_all is false)
    pub required_labels: Vec<(String, String)>,
    /// Network filter - only discover containers on this network
    pub network_filter: Option<String>,
    /// How to detect the port to use
    pub port_detection: PortDetectionStrategy,
    /// Prefix to strip from service names (e.g., project name in compose)
    pub strip_prefix: Option<String>,
    /// Whether to use container names or service names for routing
    pub use_service_names: bool,
}

#[derive(Debug, Clone)]
pub enum PortDetectionStrategy {
    /// Use the first exposed port
    FirstExposed,
    /// Use the lowest numbered exposed port
    LowestPort,
    /// Use a specific port number
    Fixed(u16),
    /// Use port from label (e.g., "axum-proxy.port")
    FromLabel(String),
}

impl Default for DockerDiscoveryConfig {
    fn default() -> Self {
        Self {
            label_prefix: "axum-proxy".to_string(),
            discover_all: false,
            required_labels: vec![],
            network_filter: None,
            port_detection: PortDetectionStrategy::FirstExposed,
            strip_prefix: None,
            use_service_names: true,
        }
    }
}

pub struct DockerDiscovery {
    docker: Docker,
    config: DockerDiscoveryConfig,
    services: Arc<RwLock<HashMap<String, String>>>,
    event_stream: Option<BoxStream<'static, Result<Change<String, String>, BoxError>>>,
}

impl DockerDiscovery {
    /// Create a new DockerDiscovery that discovers all services automatically
    pub async fn everything() -> Result<Self, BoxError> {
        let config = DockerDiscoveryConfig {
            discover_all: true,
            use_service_names: true,
            ..Default::default()
        };
        let mut discovery = Self::new(config)?;
        discovery.initialize().await?;
        Ok(discovery)
    }

    /// Create a new DockerDiscovery that only discovers labeled containers
    pub async fn labeled(label: &str) -> Result<Self, BoxError> {
        let parts: Vec<&str> = label.split('=').collect();
        let (key, value) = match parts.as_slice() {
            [key, value] => (*key, *value),
            [key] => (*key, "true"),
            _ => return Err("Invalid label format".into()),
        };

        let config = DockerDiscoveryConfig {
            required_labels: vec![(key.to_string(), value.to_string())],
            ..Default::default()
        };
        let mut discovery = Self::new(config)?;
        discovery.initialize().await?;
        Ok(discovery)
    }

    /// Create a new DockerDiscovery with custom configuration
    pub fn new(config: DockerDiscoveryConfig) -> Result<Self, BoxError> {
        let docker = Docker::connect_with_socket_defaults()?;

        Ok(Self {
            docker,
            config,
            services: Arc::new(RwLock::new(HashMap::new())),
            event_stream: None,
        })
    }

    /// Create a builder for more complex configurations
    pub fn builder() -> DockerDiscoveryBuilder {
        DockerDiscoveryBuilder::default()
    }

    async fn initialize(&mut self) -> Result<(), BoxError> {
        // Perform initial container discovery
        self.discover_containers().await?;

        // Set up event stream
        let docker = self.docker.clone();
        let config = self.config.clone();
        let services = self.services.clone();

        let event_stream = self.create_event_stream(docker, config, services).await?;
        self.event_stream = Some(event_stream);

        Ok(())
    }

    async fn discover_containers(&self) -> Result<(), BoxError> {
        let mut filters = HashMap::new();

        // Add label filters if not discovering all
        if !self.config.discover_all {
            for (key, value) in &self.config.required_labels {
                filters.insert("label".to_string(), vec![format!("{}={}", key, value)]);
            }
        }

        // Add network filter if specified
        if let Some(ref network) = self.config.network_filter {
            filters.insert("network".to_string(), vec![network.clone()]);
        }

        let options = ListContainersOptions {
            all: false, // Only running containers
            filters,
            ..Default::default()
        };

        let containers = self.docker.list_containers(Some(options)).await?;
        let mut new_services = HashMap::new();

        for container in containers {
            if let Some((path, service)) = self.container_to_service(&container).await? {
                info!("Discovered service: {} -> {}", path, service);
                new_services.insert(path, service);
            }
        }

        *self.services.write().await = new_services;
        Ok(())
    }

    async fn container_to_service(
        &self,
        container: &ContainerSummary,
    ) -> Result<Option<(String, String)>, BoxError> {
        Self::container_to_service_static(&self.config, container)
    }

    fn container_to_service_static(
        config: &DockerDiscoveryConfig,
        container: &ContainerSummary,
    ) -> Result<Option<(String, String)>, BoxError> {
        let container_name = container
            .names
            .as_ref()
            .and_then(|names| names.first())
            .ok_or("Container has no name")?
            .trim_start_matches('/');

        // Determine the service path
        let path = Self::determine_service_path_static(config, container, container_name)?;

        // Get container IP address
        let ip = Self::get_container_ip_static(config, container)?;

        // Determine port
        let port = Self::determine_port_static(config, container)?;

        // Build service URI
        let service = format!("http://{ip}:{port}");
        Ok(Some((path, service)))
    }

    fn determine_service_path_static(
        config: &DockerDiscoveryConfig,
        container: &ContainerSummary,
        container_name: &str,
    ) -> Result<String, BoxError> {
        // Check for explicit path label
        if let Some(ref labels) = container.labels {
            let path_label = format!("{}.path", config.label_prefix);
            if let Some(path) = labels.get(&path_label) {
                return Ok(path.clone());
            }
        }

        // Use service name if available and configured
        if config.use_service_names {
            if let Some(ref labels) = container.labels {
                // Docker Compose service name
                if let Some(service_name) = labels.get("com.docker.compose.service") {
                    return Ok(format!("/{service_name}"));
                }
                // Docker Swarm service name
                if let Some(service_name) = labels.get("com.docker.swarm.service.name") {
                    let name = service_name.split('.').next().unwrap_or(service_name);
                    return Ok(format!("/{name}"));
                }
            }
        }

        // Fall back to container name
        let mut name = container_name.to_string();

        // Strip prefix if configured
        if let Some(ref prefix) = config.strip_prefix {
            if name.starts_with(prefix) {
                name = name[prefix.len()..].trim_start_matches('_').to_string();
            }
        }

        // Remove instance numbers (e.g., _1, -1)
        if let Some(pos) = name.rfind(['_', '-']) {
            if name[pos + 1..].chars().all(|c| c.is_numeric()) {
                name.truncate(pos);
            }
        }

        Ok(format!("/{name}"))
    }

    fn get_container_ip_static(
        config: &DockerDiscoveryConfig,
        container: &ContainerSummary,
    ) -> Result<String, BoxError> {
        let networks = container
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
            .ok_or("No network settings found")?;

        // Prefer network from filter
        if let Some(ref network_name) = config.network_filter {
            if let Some(network) = networks.get(network_name) {
                if let Some(ip) = &network.ip_address {
                    if !ip.is_empty() {
                        return Ok(ip.clone());
                    }
                }
            }
        }

        // Fall back to first available network
        for network in networks.values() {
            if let Some(ip) = &network.ip_address {
                if !ip.is_empty() {
                    return Ok(ip.clone());
                }
            }
        }

        Err("No IP address found for container".into())
    }

    fn determine_port_static(
        config: &DockerDiscoveryConfig,
        container: &ContainerSummary,
    ) -> Result<u16, BoxError> {
        match &config.port_detection {
            PortDetectionStrategy::Fixed(port) => Ok(*port),
            PortDetectionStrategy::FromLabel(label_name) => {
                if let Some(ref labels) = container.labels {
                    let full_label = if label_name.contains('.') {
                        label_name.clone()
                    } else {
                        format!("{}.{}", config.label_prefix, label_name)
                    };

                    if let Some(port_str) = labels.get(&full_label) {
                        return port_str.parse().map_err(|_| {
                            format!("Invalid port in label {full_label}: {port_str}").into()
                        });
                    }
                }
                Err(format!("Port label not found: {label_name}").into())
            }
            PortDetectionStrategy::FirstExposed | PortDetectionStrategy::LowestPort => {
                let ports = container.ports.as_ref().ok_or("No ports exposed")?;

                let mut exposed_ports: Vec<u16> = ports
                    .iter()
                    .map(|p| p.private_port)
                    .filter(|&p| p > 0)
                    .collect();

                if exposed_ports.is_empty() {
                    return Err("No exposed ports found".into());
                }

                match config.port_detection {
                    PortDetectionStrategy::FirstExposed => Ok(exposed_ports[0]),
                    PortDetectionStrategy::LowestPort => {
                        exposed_ports.sort_unstable();
                        Ok(exposed_ports[0])
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    async fn create_event_stream(
        &self,
        docker: Docker,
        config: DockerDiscoveryConfig,
        services: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<BoxStream<'static, Result<Change<String, String>, BoxError>>, BoxError> {
        // First, create a stream of the initially discovered services
        let initial_services = services.read().await.clone();
        let initial_stream = futures_util::stream::iter(
            initial_services
                .into_iter()
                .map(|(path, service)| Ok(Change::Insert(path.clone(), service)))
        );

        let options = EventsOptions::<String> {
            filters: HashMap::from([
                ("type".to_string(), vec!["container".to_string()]),
                (
                    "event".to_string(),
                    vec!["start".to_string(), "stop".to_string(), "die".to_string()],
                ),
            ]),
            ..Default::default()
        };

        let events = docker.events(Some(options));

        let event_stream = events
            .filter_map(move |event| {
                let docker = docker.clone();
                let config = config.clone();
                let services = services.clone();

                async move {
                    match event {
                        Ok(event) => Self::handle_event(event, docker, config, services).await,
                        Err(e) => {
                            error!("Docker event error: {}", e);
                            None
                        }
                    }
                }
            });

        // Chain the initial services with the event stream
        let stream = initial_stream.chain(event_stream).boxed();

        Ok(stream)
    }

    async fn handle_event(
        event: EventMessage,
        docker: Docker,
        config: DockerDiscoveryConfig,
        services: Arc<RwLock<HashMap<String, String>>>,
    ) -> Option<Result<Change<String, String>, BoxError>> {
        let container_id = event.actor?.id?;

        match event.typ? {
            EventMessageTypeEnum::CONTAINER => {
                match event.action?.as_str() {
                    "start" => {
                        // Fetch container details and add to services
                        match docker.inspect_container(&container_id, None).await {
                            Ok(_details) => {
                                // Fetch container details and convert to service
                                if let Ok(containers) = docker
                                    .list_containers(Some(ListContainersOptions {
                                        filters: HashMap::from([(
                                            "id".to_string(),
                                            vec![container_id.clone()],
                                        )]),
                                        ..Default::default()
                                    }))
                                    .await
                                {
                                    if let Some(container) = containers.first() {
                                        match Self::container_to_service_static(&config, container)
                                        {
                                            Ok(Some((path, service))) => {
                                                services
                                                    .write()
                                                    .await
                                                    .insert(path.clone(), service.clone());
                                                info!("Added service: {} -> {}", path, service);
                                                Some(Ok(Change::Insert(path, service)))
                                            }
                                            Ok(None) => None,
                                            Err(e) => {
                                                warn!(
                                                    "Failed to convert container to service: {}",
                                                    e
                                                );
                                                None
                                            }
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            Err(e) => {
                                warn!("Failed to inspect container {}: {}", container_id, e);
                                None
                            }
                        }
                    }
                    "stop" | "die" => {
                        // Remove from services
                        let mut services = services.write().await;
                        let to_remove: Vec<String> = services
                            .iter()
                            .filter(|(_path, _)| {
                                // For now, we can't track by container ID without metadata
                                // This is a limitation we'll need to address
                                // TODO: Store container metadata separately
                                false
                            })
                            .map(|(path, _)| path.clone())
                            .collect();

                        if let Some(path) = to_remove.into_iter().next() {
                            services.remove(&path);
                            info!("Removed service: {}", path);
                            return Some(Ok(Change::Remove(path)));
                        }
                        None
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

impl Stream for DockerDiscovery {
    type Item = Result<Change<String, String>, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Initialize if needed
        if self.event_stream.is_none() {
            let this = self.as_mut();
            let _docker = this.docker.clone();
            let _config = this.config.clone();
            let _services = this.services.clone();

            // We need to initialize asynchronously, but poll_next is sync
            // This is a bit tricky - we'll return NotReady and spawn the initialization
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                // Initialize will be called when we implement the full async initialization
                waker.wake();
            });

            return Poll::Pending;
        }

        // Poll the event stream
        if let Some(ref mut stream) = self.event_stream {
            stream.as_mut().poll_next(cx)
        } else {
            Poll::Ready(None)
        }
    }
}

#[derive(Default)]
pub struct DockerDiscoveryBuilder {
    config: DockerDiscoveryConfig,
}

impl DockerDiscoveryBuilder {
    pub fn label_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.config.label_prefix = prefix.into();
        self
    }

    pub fn discover_all_services(mut self) -> Self {
        self.config.discover_all = true;
        self
    }

    pub fn require_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.required_labels.push((key.into(), value.into()));
        self
    }

    pub fn network(mut self, network: impl Into<String>) -> Self {
        self.config.network_filter = Some(network.into());
        self
    }

    pub fn port_detection(mut self, strategy: PortDetectionStrategy) -> Self {
        self.config.port_detection = strategy;
        self
    }

    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.config.strip_prefix = Some(prefix.into());
        self
    }

    pub fn use_container_names(mut self) -> Self {
        self.config.use_service_names = false;
        self
    }

    pub async fn build(self) -> Result<DockerDiscovery, BoxError> {
        let mut discovery = DockerDiscovery::new(self.config)?;
        discovery.initialize().await?;
        Ok(discovery)
    }
}
