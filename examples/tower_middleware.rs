use axum::{Router, body::Body, extract::State, response::IntoResponse, routing::get, serve};
use axum_reverse_proxy::ReverseProxy;
use http::Request;
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{timeout::TimeoutLayer, validate_request::ValidateRequestHeaderLayer};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

// Our app state type
#[derive(Clone)]
struct AppState {
    app_name: String,
}

#[allow(deprecated)] // ValidateRequestHeaderLayer::bearer
#[tokio::main]
async fn main() {
    // Initialize tracing
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .compact()
        .init();

    // Create our app state
    let state = AppState {
        app_name: "Tower Middleware Example".to_string(),
    };

    // Create a reverse proxy that forwards requests to httpbin.org
    let proxy = ReverseProxy::new("/api", "https://httpbin.org");

    // Create our main router with app state
    let app = Router::new()
        .route("/", get(root_handler))
        .with_state(state.clone());

    // Convert proxy to router
    let proxy_router: Router = proxy.into();

    // Add middleware stack
    let app = app.merge(
        proxy_router.layer(
            ServiceBuilder::new()
                // Add request timeout
                .layer(TimeoutLayer::with_status_code(
                    http::StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(10),
                ))
                // Require API key for /api routes
                .layer(ValidateRequestHeaderLayer::bearer("secret-api-key"))
                // Add custom header
                .map_request(|mut req: Request<Body>| {
                    req.headers_mut()
                        .insert("X-Custom-Header", "custom-value".parse().unwrap());
                    req
                }),
        ),
    );

    // Create a TCP listener
    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Server running on http://localhost:3000");
    info!("Try:");
    info!("  - GET /         -> App info");
    info!("  - GET /api/ip   -> Proxied to httpbin.org/ip (requires Bearer token)");
    info!("  - GET /api/uuid -> Proxied to httpbin.org/uuid (requires Bearer token)");
    info!("");
    info!("Example curl commands:");
    info!("  curl http://localhost:3000/");
    info!("  curl -H 'Authorization: Bearer secret-api-key' http://localhost:3000/api/ip");

    // Run the server
    serve(listener, app).await.unwrap();
}

async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({
        "app": state.app_name,
        "endpoints": {
            "/": "This info",
            "/api/*": "Proxied to httpbin.org (requires Bearer token)"
        }
    }))
}
