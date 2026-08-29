//! Tests for core Via header emission and loop detection
//! (`ProxyPolicy::with_via`), the replacement for the deprecated
//! `Rfc9110Layer`.

use axum::{Router, body::Body, http::Request, response::Json, routing::get};
use axum_reverse_proxy::{ProxyPolicy, ReverseProxy};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::net::TcpListener;

async fn echo_headers(req: Request<Body>) -> Json<Value> {
    let headers = req.headers().clone();
    Json(json!({ "headers": headers.iter().map(|(k, v)| {
        (k.as_str().to_string(), v.to_str().unwrap().to_string())
    }).collect::<HashMap<String, String>>() }))
}

async fn serve_proxy(upstream: Router, policy: ProxyPolicy) -> SocketAddr {
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
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });

    proxy_addr
}

#[tokio::test]
async fn via_appended_to_forwarded_request() {
    let upstream = Router::new().route("/headers", get(echo_headers));
    let policy = ProxyPolicy::new().with_via("my-proxy");
    let proxy_addr = serve_proxy(upstream, policy).await;

    let client = reqwest::Client::new();

    // No existing Via: ours is the only element
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["headers"]["via"], "1.1 my-proxy");

    // Existing Via chain is preserved and ours appended
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .header("Via", "1.0 earlier-proxy")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["headers"]["via"], "1.0 earlier-proxy, 1.1 my-proxy");
}

#[tokio::test]
async fn via_appended_to_response() {
    let upstream = Router::new().route("/ok", get(|| async { "ok" }));
    let policy = ProxyPolicy::new().with_via("my-proxy");
    let proxy_addr = serve_proxy(upstream, policy).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/ok"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("via").unwrap(), "1.1 my-proxy");
}

#[tokio::test]
async fn own_pseudonym_in_via_chain_returns_508() {
    let upstream = Router::new().route("/ok", get(|| async { "ok" }));
    let policy = ProxyPolicy::new().with_via("my-proxy");
    let proxy_addr = serve_proxy(upstream, policy).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/ok"))
        .header("Via", "1.1 other-proxy, 1.1 my-proxy")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::LOOP_DETECTED);

    // A different pseudonym in the chain is not a loop
    let resp = client
        .get(format!("http://{proxy_addr}/ok"))
        .header("Via", "1.1 other-proxy")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn no_via_handling_without_pseudonym() {
    let upstream = Router::new().route("/headers", get(echo_headers));
    let proxy_addr = serve_proxy(upstream, ProxyPolicy::default()).await;

    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!("http://{proxy_addr}/headers"))
        .header("Via", "1.1 some-proxy")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Passed through untouched, no element appended
    assert_eq!(body["headers"]["via"], "1.1 some-proxy");
}
