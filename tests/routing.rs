use axum::{Router, body::Body, extract::State, http::Request, response::Json, routing::get};
use axum_reverse_proxy::ReverseProxy;
use serde_json::{Value, json};
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
    let proxy = ReverseProxy::new("/proxy", &format!("http://{test_addr}"));

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
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["app"], "test app");

    // Test proxied endpoint
    let response = client
        .get(format!("http://{proxy_addr}/proxy/test"))
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
    let proxy = ReverseProxy::new("", &format!("http://{test_addr}"));
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
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "root");

    // Test double slashes
    let response = client
        .get(format!("http://{proxy_addr}/test//double"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "double");

    // Test URL-encoded space
    let response = client
        .get(format!("http://{proxy_addr}/test/%20space"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "space");

    // Test special characters
    let response = client
        .get(format!("http://{proxy_addr}/test/special!%40%23%24"))
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
    let proxy1 = ReverseProxy::new("/api1", &format!("http://{addr1}"));
    let proxy2 = ReverseProxy::new("/api2", &format!("http://{addr2}"));

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
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["app"], "test app");

    // Test first proxy
    let response = client
        .get(format!("http://{proxy_addr}/api1/test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "server1");

    // Test second proxy
    let response = client
        .get(format!("http://{proxy_addr}/api2/test"))
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
        .merge(ReverseProxy::new("/api", &format!("http://{test_addr}")))
        .merge(ReverseProxy::new(
            "/_test",
            &format!("http://{test_addr}/_test"),
        ))
        .merge(ReverseProxy::new(
            "/foo",
            &format!("http://{test_addr}/bar"),
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
        .get(format!("http://{proxy_addr}/api/_test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test".to_string());

    // Test with trailing slash to ensure it's preserved
    let response = client
        .get(format!("http://{proxy_addr}/api/_test/"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test/");

    // Test without trailing slash at base path segment
    let response = client
        .get(format!("http://{proxy_addr}/_test"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/_test".to_string());

    // Test without trailing slash at base path segment
    let response = client
        .get(format!("http://{proxy_addr}/foo"))
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
    let proxy = ReverseProxy::new("/", &format!("http://{test_addr}"));
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
        .get(format!("http://{proxy_addr}/echo?foo=bar"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "foo=bar");

    // Test multiple query parameters
    let response = client
        .get(format!(
            "http://{proxy_addr}/echo?foo=bar&baz=qux&special=hello%20world"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "foo=bar&baz=qux&special=hello%20world");

    // Test empty query parameter
    let response = client
        .get(format!("http://{proxy_addr}/echo?empty="))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["query"], "empty=");

    // Test special characters in query parameters
    let response = client
        .get(format!(
            "http://{proxy_addr}/echo?special=%21%40%23%24%25%5E%26"
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
async fn test_no_extra_slash_for_empty_path_with_query() {
    // Upstream echoes the exact path it sees
    let echo_handler = get(|req: Request<Body>| async move {
        let path = req.uri().path().to_string();
        Json(json!({ "received_path": path }))
    });
    let upstream = Router::new()
        .route("/{*path}", echo_handler.clone())
        .route("/", echo_handler);

    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Reverse proxy configured on a non-root path, targeting a sub-path on the upstream
    let proxy = ReverseProxy::new("/proxy", &format!("http://{upstream_addr}/api"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // When requesting /proxy?foo=bar, upstream must see /api (not /api/)
    let response = client
        .get(format!("http://{proxy_addr}/proxy?foo=bar"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/api");

    // Cleanup
    proxy_server.abort();
    upstream_server.abort();
}

#[tokio::test]
async fn test_only_prefixed_paths_are_proxied() {
    // Create a simple upstream
    let upstream = Router::new().route("/ok", get(|| async { "OK" }));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Reverse proxy mounted at /api
    let proxy = ReverseProxy::new("/api", &format!("http://{upstream_addr}"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // Non-prefixed path should NOT be proxied and should 404
    let resp = client
        .get(format!("http://{proxy_addr}/ok"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Prefixed path should be proxied
    let resp = client
        .get(format!("http://{proxy_addr}/api/ok"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "OK");

    // Cleanup
    proxy_server.abort();
    upstream_server.abort();
}

#[tokio::test]
async fn test_similar_prefix_is_not_stripped() {
    // Upstream echoes received path
    let echo_handler = get(|req: Request<Body>| async move {
        let path = req.uri().path();
        Json(json!({ "received_path": path }))
    });
    let upstream = Router::new()
        .route("/{*path}", echo_handler.clone())
        .route("/", echo_handler);

    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Use the proxy directly as a fallback service (not nested),
    // so transform_uri may attempt to strip the configured base.
    let proxy = ReverseProxy::new("/api", &format!("http://{upstream_addr}"));
    let app = Router::new().fallback_service(proxy);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // Path begins with '/api' but is not the '/api' prefix; must not be stripped.
    let response = client
        .get(format!("http://{proxy_addr}/apiary"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/apiary");

    // '/apiX/foo' also should not be stripped.
    let response = client
        .get(format!("http://{proxy_addr}/apix/foo"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/apix/foo");

    proxy_server.abort();
    upstream_server.abort();
}

#[tokio::test]
async fn test_root_base_query_only_no_slash() {
    // Upstream echoes the exact path
    let echo_handler =
        get(|req: Request<Body>| async move { Json(json!({ "received_path": req.uri().path() })) });
    let upstream = Router::new()
        .route("/{*path}", echo_handler.clone())
        .route("/", echo_handler);
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Base path is '/', target has a sub-path.
    let proxy = ReverseProxy::new("/", &format!("http://{upstream_addr}/api"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // '/?q=1' at root should resolve to '/api' (no trailing slash) at upstream
    let response = client
        .get(format!("http://{proxy_addr}/?q=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received_path"], "/api");

    proxy_server.abort();
    upstream_server.abort();
}

// NOTE: Dot-segment normalization is left to underlying stacks. We intentionally
// do not assert preservation here because different layers may canonicalize.

#[tokio::test]
async fn test_encoded_boundary_stripping() {
    // Upstream echoes the path
    let echo_handler =
        get(|req: Request<Body>| async move { Json(json!({ "received_path": req.uri().path() })) });
    let upstream = Router::new()
        .route("/{*path}", echo_handler.clone())
        .route("/", echo_handler);
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Use fallback service so boundary-aware stripping logic is exercised
    let proxy = ReverseProxy::new("/foo%20bar", &format!("http://{upstream_addr}"));
    let app = Router::new().fallback_service(proxy);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // Exact match should strip
    let res = client
        .get(format!("http://{proxy_addr}/foo%20bar"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["received_path"], "/");

    // Boundary with slash should strip
    let res = client
        .get(format!("http://{proxy_addr}/foo%20bar/baz"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["received_path"], "/baz");

    // Similar prefix without boundary must NOT strip
    let res = client
        .get(format!("http://{proxy_addr}/foo%20barista"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["received_path"], "/foo%20barista");

    proxy_server.abort();
    upstream_server.abort();
}

#[tokio::test]
async fn test_root_base_matrix() {
    // Upstream echoes the path
    let echo_handler =
        get(|req: Request<Body>| async move { Json(json!({ "received_path": req.uri().path() })) });
    let upstream = Router::new()
        .route("/{*path}", echo_handler.clone())
        .route("/", echo_handler);
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    // Base '/'; target '/api'
    let proxy = ReverseProxy::new("/", &format!("http://{upstream_addr}/api"));
    let app: Router = proxy.into();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    let cases = [("/", "/api"), ("/x", "/api/x"), ("/x/", "/api/x/")];
    for (req_path, expected) in cases {
        let res = client
            .get(format!("http://{proxy_addr}{req_path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["received_path"], expected);
    }

    proxy_server.abort();
    upstream_server.abort();
}

#[tokio::test]
async fn test_methods_preserve_join() {
    use http::Method;

    let echo_handler = |method: Method, req: Request<Body>| async move {
        Json(json!({ "m": method.as_str(), "p": req.uri().path() }))
    };

    let upstream = Router::new()
        .route(
            "/{*path}",
            get(echo_handler.clone())
                .post(echo_handler.clone())
                .put(echo_handler.clone())
                .delete(echo_handler.clone()),
        )
        .route(
            "/",
            get(echo_handler.clone())
                .post(echo_handler.clone())
                .put(echo_handler.clone())
                .delete(echo_handler),
        );

    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream_server = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    let proxy = ReverseProxy::new("/api", &format!("http://{upstream_addr}/tgt"));
    let app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // GET
    let r = client
        .get(format!("http://{proxy_addr}/api/x"))
        .send()
        .await
        .unwrap();
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["m"], "GET");
    assert_eq!(b["p"], "/tgt/x");

    // POST
    let r = client
        .post(format!("http://{proxy_addr}/api/x"))
        .body("hi")
        .send()
        .await
        .unwrap();
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["m"], "POST");
    assert_eq!(b["p"], "/tgt/x");

    // PUT
    let r = client
        .put(format!("http://{proxy_addr}/api/x/"))
        .body("hi")
        .send()
        .await
        .unwrap();
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["m"], "PUT");
    assert_eq!(b["p"], "/tgt/x/");

    // DELETE
    let r = client
        .delete(format!("http://{proxy_addr}/api?flag=1"))
        .send()
        .await
        .unwrap();
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["m"], "DELETE");
    assert_eq!(b["p"], "/tgt");

    proxy_server.abort();
    upstream_server.abort();
}
