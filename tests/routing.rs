use axum::{body::Body, extract::State, http::Request, response::Json, routing::get, Router};
use axum_reverse_proxy::ReverseProxy;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::test]
async fn test_proxy_nested_routing() {
    // Create a test server
    let app = Router::new().route(
        "/test",
        get(|| async { Json(json!({"message": "Hello from test server!"})) }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/proxy", &format!("http://{}", test_addr));

    // Create an app state
    #[derive(Clone)]
    struct AppState {
        name: String,
    }
    let state = AppState {
        name: "test app".to_string(),
    };

    // Create a main router with app state and proxy
    let app = Router::new()
        .route(
            "/",
            get(|State(state): State<AppState>| async move { Json(json!({ "app": state.name })) }),
        )
        .with_state(state);

    // Convert proxy to router and merge
    let proxy_router: Router = proxy.into();
    let app = app.merge(proxy_router);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test root endpoint with app state
    let response = client
        .get(format!("http://{}/", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["app"], "test app");

    // Test proxied endpoint
    let response = client
        .get(format!("http://{}/proxy/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Hello from test server!");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_path_handling() {
    // Create a test server
    let app = Router::new()
        .route("/", get(|| async { "root" }))
        .route("/test//double", get(|| async { "double" }))
        .route("/test/%20space", get(|| async { "space" }))
        .route("/test/special!%40%23%24", get(|| async { "special" }));

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy with empty path
    let proxy = ReverseProxy::new("", &format!("http://{}", test_addr));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test root path
    let response = client
        .get(format!("http://{}/", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "root");

    // Test double slashes
    let response = client
        .get(format!("http://{}/test//double", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "double");

    // Test URL-encoded space
    let response = client
        .get(format!("http://{}/test/%20space", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "space");

    // Test special characters
    let response = client
        .get(format!("http://{}/test/special!%40%23%24", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "special");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_multiple_states() {
    // Create test servers
    let app1 = Router::new().route("/test", get(|| async { "server1" }));
    let app2 = Router::new().route("/test", get(|| async { "server2" }));

    let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr1 = listener1.local_addr().unwrap();
    let server1 = tokio::spawn(async move {
        axum::serve(listener1, app1).await.unwrap();
    });

    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let server2 = tokio::spawn(async move {
        axum::serve(listener2, app2).await.unwrap();
    });

    // Create proxies with different paths
    let proxy1 = ReverseProxy::new("/api1", &format!("http://{}", addr1));
    let proxy2 = ReverseProxy::new("/api2", &format!("http://{}", addr2));

    // Create app state
    #[derive(Clone)]
    struct AppState {
        name: String,
    }
    let state = AppState {
        name: "test app".to_string(),
    };

    // Create main router with state and both proxies
    let app = Router::new()
        .route(
            "/",
            get(|State(state): State<AppState>| async move { Json(json!({ "app": state.name })) }),
        )
        .with_state(state);

    // Convert proxies to routers and merge
    let proxy_router1: Router = proxy1.into();
    let proxy_router2: Router = proxy2.into();
    let app = app.merge(proxy_router1).merge(proxy_router2);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test root endpoint with app state
    let response = client
        .get(format!("http://{}/", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["app"], "test app");

    // Test first proxy
    let response = client
        .get(format!("http://{}/api1/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "server1");

    // Test second proxy
    let response = client
        .get(format!("http://{}/api2/test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "server2");

    // Clean up
    proxy_server.abort();
    server1.abort();
    server2.abort();
}

#[tokio::test]
async fn test_proxy_exact_path_handling() {
    // Initialize tracing for this test
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    // Create a test server that echoes the exact path it receives
    let echo_handler = get(|req: Request<Body>| async move {
        let path = req.uri().path();
        Json(json!({ "received_path": path }))
    });
    let app = Router::new()
        .route("/", echo_handler.clone())
        .route("/{*path}", echo_handler);

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy that maps /api to the test server
    let app: Router = Router::new()
        .merge(ReverseProxy::new("/api", &format!("http://{}", test_addr)))
        .merge(ReverseProxy::new(
            "/_test",
            &format!("http://{}/_test", test_addr),
        ))
        .merge(ReverseProxy::new(
            "/foo",
            &format!("http://{}/bar", test_addr),
        ));

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test that /api/_test gets mapped correctly without extra slashes
    let response = client
        .get(format!("http://{}/api/_test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test".to_string());

    // Test with trailing slash to ensure it's preserved
    let response = client
        .get(format!("http://{}/api/_test/", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test/");

    // Test without trailing slash at base path segment
    let response = client
        .get(format!("http://{}/_test", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test".to_string());

    // Test without trailing slash at base path segment
    let response = client
        .get(format!("http://{}/foo", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/bar".to_string());

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_query_parameters() {
    // Create a test server that echoes query parameters
    let app = Router::new().route(
        "/echo",
        get(|req: Request<Body>| async move {
            let query = req.uri().query().unwrap_or("");
            Json(json!({ "query": query }))
        }),
    );

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Create a reverse proxy
    let proxy = ReverseProxy::new("/", &format!("http://{}", test_addr));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    // Create a client
    let client = reqwest::Client::new();

    // Test simple query parameter
    let response = client
        .get(format!("http://{}/echo?foo=bar", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "foo=bar");

    // Test multiple query parameters
    let response = client
        .get(format!(
            "http://{}/echo?foo=bar&baz=qux&special=hello%20world",
            proxy_addr
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "foo=bar&baz=qux&special=hello%20world");

    // Test empty query parameter
    let response = client
        .get(format!("http://{}/echo?empty=", proxy_addr))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "empty=");

    // Test special characters in query parameters
    let response = client
        .get(format!(
            "http://{}/echo?special=%21%40%23%24%25%5E%26",
            proxy_addr
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "special=%21%40%23%24%25%5E%26");

    // Clean up
    proxy_server.abort();
    test_server.abort();
}

#[tokio::test]
async fn test_proxy_trailing_slash_path() {
    // Create a test server that echoes the exact path it receives
    let echo_handler = get(|req: Request<Body>| async move {
        let path = req.uri().path();
        Json(json!({ "received_path": path }))
    });
    let app = Router::new()
        .route("/", echo_handler.clone())
        .route("/{*path}", echo_handler);

    let test_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let test_addr = test_listener.local_addr().unwrap();
    let test_server = tokio::spawn(async move {
        axum::serve(test_listener, app).await.unwrap();
    });

    // Configure proxy with a trailing slash in the base path
    let proxy = ReverseProxy::new("/api/", &format!("http://{}", test_addr));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // Request without trailing slash should still map to root of upstream
    let response = client
        .get(format!("http://{}/api", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/");

    // Request with query parameters should also work
    let response = client
        .get(format!("http://{}/api?foo=bar", proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/");

    proxy_server.abort();
    test_server.abort();
}
