//! Integration tests for the ProxyPolicy safety knobs added in 2.1:
//! upstream response timeout, request body cap, X-Forwarded-Proto, and
//! CONNECT rejection.

use axum::{Router, body::Body, http::Request, response::Json, routing::get};
use axum_reverse_proxy::{ProxyPolicy, ReverseProxy};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceExt;

async fn echo_headers(req: Request<Body>) -> Json<Value> {
    let headers = req.headers().clone();
    Json(json!({ "headers": headers.iter().map(|(k, v)| {
        (k.as_str().to_string(), v.to_str().unwrap().to_string())
    }).collect::<HashMap<String, String>>() }))
}

/// Spawn an upstream router and a proxy in front of it with the given policy.
/// Returns the proxy's address.
async fn serve_proxy(upstream: Router, policy: ProxyPolicy, connect_info: bool) -> SocketAddr {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{upstream_addr}")).with_policy(policy);
    let proxy_app: Router = proxy.into();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    tokio::spawn(async move {
        if connect_info {
            axum::serve(
                proxy_listener,
                proxy_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        } else {
            axum::serve(proxy_listener, proxy_app).await.unwrap();
        }
    });

    proxy_addr
}

#[tokio::test]
async fn upstream_timeout_returns_504() {
    let upstream = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "too late"
        }),
    );
    let policy = ProxyPolicy::new().with_upstream_timeout(Duration::from_millis(200));
    let proxy_addr = serve_proxy(upstream, policy, false).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn upstream_timeout_does_not_fire_for_fast_upstream() {
    let upstream = Router::new().route("/fast", get(|| async { "ok" }));
    let policy = ProxyPolicy::new().with_upstream_timeout(Duration::from_secs(5));
    let proxy_addr = serve_proxy(upstream, policy, false).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/fast"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn declared_oversize_body_rejected_with_413() {
    let upstream = Router::new().route(
        "/upload",
        axum::routing::post(|body: String| async move { format!("got {} bytes", body.len()) }),
    );
    let policy = ProxyPolicy::new().with_max_request_body_bytes(1024);
    let proxy_addr = serve_proxy(upstream, policy, false).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{proxy_addr}/upload"))
        .body("x".repeat(4096))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    // Under the cap passes through
    let resp = client
        .post(format!("http://{proxy_addr}/upload"))
        .body("x".repeat(512))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "got 512 bytes");
}

#[tokio::test]
async fn streaming_oversize_body_does_not_reach_upstream_complete() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Track whether the upstream ever saw a complete oversized body.
    let saw_complete = Arc::new(AtomicBool::new(false));
    let saw_complete_clone = saw_complete.clone();
    let upstream = Router::new().route(
        "/upload",
        axum::routing::post(move |body: Body| {
            let saw_complete = saw_complete_clone.clone();
            async move {
                if axum::body::to_bytes(body, usize::MAX).await.is_ok() {
                    saw_complete.store(true, Ordering::SeqCst);
                }
                "ok"
            }
        }),
    );
    let policy = ProxyPolicy::new().with_max_request_body_bytes(1024);
    let proxy_addr = serve_proxy(upstream, policy, false).await;

    // Send a chunked body (no Content-Length) that exceeds the cap.
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> =
        vec![Ok(bytes::Bytes::from(vec![b'x'; 4096]))];
    let stream = futures_util::stream::iter(chunks);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{proxy_addr}/upload"))
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap();

    // The upstream request fails mid-body: the client must not see success.
    assert_ne!(resp.status(), reqwest::StatusCode::OK);
    assert!(
        !saw_complete.load(Ordering::SeqCst),
        "oversized streaming body reached the upstream in full"
    );
}

#[tokio::test]
async fn x_forwarded_proto_and_forwarded_set_with_public_scheme() {
    let upstream = Router::new().route("/headers", get(echo_headers));
    let policy = ProxyPolicy::new().with_public_scheme("https");
    let proxy_addr = serve_proxy(upstream, policy, true).await;

    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let headers = body["headers"].as_object().unwrap();
    assert_eq!(headers["x-forwarded-proto"], "https");
    assert_eq!(headers["x-forwarded-for"], "127.0.0.1");
    let forwarded = headers["forwarded"].as_str().unwrap();
    assert!(forwarded.contains("for=127.0.0.1"), "{forwarded}");
    assert!(forwarded.contains("proto=https"), "{forwarded}");
    assert!(
        forwarded.contains(&format!("host=\"{proxy_addr}\"")),
        "{forwarded}"
    );
}

#[tokio::test]
async fn connect_method_is_rejected_with_501() {
    let upstream = Router::new().route("/", get(|| async { "ok" }));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    });

    let proxy = ReverseProxy::new("/", format!("http://{upstream_addr}"));
    let app: Router = proxy.into();

    let req = Request::builder()
        .method(axum::http::Method::CONNECT)
        .uri("/anywhere")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
}
