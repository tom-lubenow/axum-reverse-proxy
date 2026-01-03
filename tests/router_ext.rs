//! Integration tests for ProxyRouterExt

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use axum_reverse_proxy::{ProxyRouterExt, TargetResolver, proxy_template};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::ServiceExt;

/// Create a simple backend server that echoes request info
async fn create_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/", get(|| async { "root" }))
        .route("/users/{id}", get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
            format!("user:{}", id)
        }))
        .route("/videos/{id}/{quality}", get(|axum::extract::Path((id, quality)): axum::extract::Path<(String, String)>| async move {
            format!("video:{}:{}", id, quality)
        }))
        .route("/api/{*rest}", get(|axum::extract::Path(rest): axum::extract::Path<String>| async move {
            format!("api:{}", rest)
        }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server time to start
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    (addr, handle)
}

#[tokio::test]
async fn test_static_target_proxy() {
    let (backend_addr, _handle) = create_backend().await;
    let target = format!("http://{}", backend_addr);

    let app: Router = Router::new().proxy_route("/proxy", target);

    let req = Request::builder()
        .uri("/proxy")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"root");
}

#[tokio::test]
async fn test_template_target_single_param() {
    let (backend_addr, _handle) = create_backend().await;
    let target = format!("http://{}/users/{{id}}", backend_addr);

    let app: Router = Router::new().proxy_route("/u/{id}", proxy_template(&target));

    let req = Request::builder()
        .uri("/u/123")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"user:123");
}

#[tokio::test]
async fn test_template_target_multiple_params() {
    let (backend_addr, _handle) = create_backend().await;
    let target = format!("http://{}/videos/{{id}}/{{quality}}", backend_addr);

    let app: Router = Router::new().proxy_route("/v/{id}/{quality}", proxy_template(&target));

    let req = Request::builder()
        .uri("/v/abc123/720p")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"video:abc123:720p");
}

#[tokio::test]
async fn test_wildcard_path() {
    let (backend_addr, _handle) = create_backend().await;
    let target = format!("http://{}/api/{{rest}}", backend_addr);

    let app: Router = Router::new().proxy_route("/gateway/{*rest}", proxy_template(&target));

    let req = Request::builder()
        .uri("/gateway/foo/bar/baz")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"api:foo/bar/baz");
}

#[tokio::test]
async fn test_custom_resolver() {
    #[derive(Clone)]
    struct PrefixResolver {
        base: String,
        prefix: String,
    }

    impl TargetResolver for PrefixResolver {
        fn resolve(&self, _req: &axum::http::Request<Body>, params: &[(String, String)]) -> String {
            let id = params
                .iter()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.as_str())
                .unwrap_or("unknown");
            format!("{}/users/{}{}", self.base, self.prefix, id)
        }
    }

    let (backend_addr, _handle) = create_backend().await;

    let resolver = PrefixResolver {
        base: format!("http://{}", backend_addr),
        prefix: "user_".to_string(),
    };

    let app: Router = Router::new().proxy_route("/custom/{id}", resolver);

    let req = Request::builder()
        .uri("/custom/42")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    // The backend receives /users/user_42 which doesn't match its route pattern exactly
    // but that's OK - this tests that the custom resolver is being called
    assert_eq!(&body[..], b"user:user_42");
}

#[tokio::test]
async fn test_header_based_resolver() {
    #[derive(Clone)]
    struct HeaderResolver {
        default_backend: String,
        premium_backend: String,
    }

    impl TargetResolver for HeaderResolver {
        fn resolve(&self, req: &axum::http::Request<Body>, _params: &[(String, String)]) -> String {
            if req.headers().get("x-premium").is_some() {
                self.premium_backend.clone()
            } else {
                self.default_backend.clone()
            }
        }
    }

    let (backend_addr, _handle) = create_backend().await;

    let resolver = HeaderResolver {
        default_backend: format!("http://{}/users/default", backend_addr),
        premium_backend: format!("http://{}/users/premium", backend_addr),
    };

    let app: Router = Router::new().proxy_route("/account", resolver);

    // Request without premium header
    let req1 = Request::builder()
        .uri("/account")
        .body(Body::empty())
        .unwrap();
    let res1 = app.clone().oneshot(req1).await.unwrap();
    let body1 = axum::body::to_bytes(res1.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body1[..], b"user:default");

    // Request with premium header
    let req2 = Request::builder()
        .uri("/account")
        .header("x-premium", "true")
        .body(Body::empty())
        .unwrap();
    let res2 = app.oneshot(req2).await.unwrap();
    let body2 = axum::body::to_bytes(res2.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body2[..], b"user:premium");
}

#[tokio::test]
async fn test_multiple_proxy_routes() {
    let (backend_addr, _handle) = create_backend().await;
    let base = format!("http://{}", backend_addr);

    let app: Router = Router::new()
        .proxy_route(
            "/users/{id}",
            proxy_template(&format!("{}/users/{{id}}", base)),
        )
        .proxy_route(
            "/videos/{id}/{q}",
            proxy_template(&format!("{}/videos/{{id}}/{{q}}", base)),
        );

    // Test first route
    let req1 = Request::builder()
        .uri("/users/alice")
        .body(Body::empty())
        .unwrap();
    let res1 = app.clone().oneshot(req1).await.unwrap();
    let body1 = axum::body::to_bytes(res1.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body1[..], b"user:alice");

    // Test second route
    let req2 = Request::builder()
        .uri("/videos/vid001/1080p")
        .body(Body::empty())
        .unwrap();
    let res2 = app.oneshot(req2).await.unwrap();
    let body2 = axum::body::to_bytes(res2.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body2[..], b"video:vid001:1080p");
}
