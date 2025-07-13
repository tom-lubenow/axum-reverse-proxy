use axum::{Router, response::Html, routing::get};
use axum_reverse_proxy::{DockerRouter, DockerRouterExt};
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

    // Create a Docker router that discovers all running containers
    let docker_router = DockerRouter::everything(client)
        .await
        .expect("Failed to create Docker router");

    info!("Docker discovery started - containers will be available at their service names");
    info!("For example, if you have a service named 'api', it will be available at /api");

    // Create the main application with a welcome page
    let app = Router::new()
        .route("/", get(welcome))
        .docker_proxy(docker_router)
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()));

    // Start the server
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Reverse proxy listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn welcome() -> Html<&'static str> {
    Html(
        r#"
        <h1>Docker Discovery Proxy</h1>
        <p>This proxy automatically discovers and routes to Docker containers.</p>
        <p>Services are available at their container/service names:</p>
        <ul>
            <li>If you have a container named 'api', access it at <a href="/api">/api</a></li>
            <li>Docker Compose services use their service names</li>
            <li>Container names are cleaned up (prefixes and numbers removed)</li>
        </ul>
        <p>Check the logs to see discovered services.</p>
        "#,
    )
}
