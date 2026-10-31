use axum::body::Body;
use axum_reverse_proxy::{ProxyPolicy, ReverseProxy, proxy_request_on_stream};
use http::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::timeout;

fn request() -> Request<Body> {
    Request::builder()
        .uri("/incoming")
        .body(Body::empty())
        .unwrap()
}

fn websocket_request() -> Request<Body> {
    Request::builder()
        .uri("/ws")
        .header("connection", "keep-alive, upgrade, x-secret")
        .header("x-secret", "must-not-forward")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-protocol", "chat, other")
        .body(Body::empty())
        .unwrap()
}

async fn headers(stream: &mut DuplexStream) -> String {
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(2), async {
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn authorization_before_poll_and_rejection_write_nothing() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let policy = ProxyPolicy::default();
    let pending = proxy_request_on_stream(
        "http://unresolvable.invalid/final".parse().unwrap(),
        request(),
        stream,
        &policy,
    );
    // Caller has connected but has not authorized. Even constructing the future
    // cannot write bytes. Rejecting drops the stream without polling the proxy.
    assert!(
        timeout(Duration::from_millis(20), peer.read_u8())
            .await
            .is_err()
    );
    drop(pending);
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
}

#[tokio::test]
async fn supplied_stream_only_and_response_streaming() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let seen = headers(&mut peer).await;
        assert!(seen.starts_with("GET /final?q=1 HTTP/1.1"));
        assert!(seen.contains("host: unresolvable.invalid"));
        peer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        peer.write_all(b"3\r\ntwo\r\n0\r\n\r\n").await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let response = proxy_request_on_stream(
        "http://unresolvable.invalid/final?q=1".parse().unwrap(),
        request(),
        stream,
        &ProxyPolicy::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap(),
        "onetwo"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_closes_supplied_connection() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        headers(&mut peer).await;
        assert_eq!(
            timeout(Duration::from_secs(2), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let response = proxy_request_on_stream(
        "http://unused.invalid/".parse().unwrap(),
        request(),
        stream,
        &ProxyPolicy::new().with_upstream_timeout(Duration::from_millis(30)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_custom_connector_can_reject() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let connector = tower::service_fn(move |_uri: http::Uri| {
        captured.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err::<TokioIo<tokio::net::TcpStream>, _>(std::io::Error::other("authorization denied"))
        })
    });
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(connector);
    let proxy = ReverseProxy::new_with_client("/", "http://unused.invalid", client);
    let response = proxy.proxy_request(websocket_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn websocket_rejection_preserves_status_headers_and_body() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let seen = headers(&mut peer).await;
        assert!(!seen.contains("x-secret"));
        peer.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\nX-Reason: denied\r\nConnection: close\r\n\r\ndenied").await.unwrap();
    });
    let response = proxy_request_on_stream(
        "http://unused.invalid/".parse().unwrap(),
        websocket_request(),
        stream,
        &ProxyPolicy::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(response.headers()["x-reason"], "denied");
    assert!(!response.headers().contains_key("connection"));
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap(),
        "denied"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn upgrade_preserves_headers_buffered_bytes_and_half_close() {
    let (upstream, mut peer) = tokio::io::duplex(4096);
    let (mut client, downstream) = tokio::io::duplex(4096);
    let upstream = Arc::new(tokio::sync::Mutex::new(Some(upstream)));
    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let upstream = upstream.clone();
        async move {
            proxy_request_on_stream(
                "http://unused.invalid/".parse().unwrap(),
                req.map(Body::new),
                upstream.lock().await.take().unwrap(),
                &ProxyPolicy::default(),
            )
            .await
        }
    });
    let driver = tokio::spawn(async move {
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(downstream), service)
            .with_upgrades()
            .await
            .unwrap();
    });
    let server = tokio::spawn(async move {
        headers(&mut peer).await;
        peer.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade, x-private\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Protocol: chat\r\nSec-WebSocket-Extensions: permessage-deflate\r\nX-Private: secret\r\n\r\n\x00\xffraw").await.unwrap();
        let mut data = Vec::new();
        peer.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"\x81\x80\x01\x02\x03\x04");
        // Respond after the client's write half closes.
        peer.write_all(b"tail").await.unwrap();
        peer.shutdown().await.unwrap();
    });
    client.write_all(b"GET / HTTP/1.1\r\nHost: client\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat, other\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n").await.unwrap();
    let seen = headers(&mut client).await.to_ascii_lowercase();
    assert!(seen.starts_with("http/1.1 101"));
    assert!(seen.contains("sec-websocket-protocol: chat"));
    assert!(seen.contains("sec-websocket-extensions: permessage-deflate"));
    assert!(!seen.contains("x-private"));
    client.write_all(b"\x81\x80\x01\x02\x03\x04").await.unwrap();
    client.shutdown().await.unwrap();
    let mut received = Vec::new();
    timeout(Duration::from_secs(2), client.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, b"\x00\xffrawtail");
    server.await.unwrap();
    driver.await.unwrap();
}

#[tokio::test]
async fn broken_supplied_stream_returns_error_without_fallback() {
    let (stream, peer) = tokio::io::duplex(4096);
    drop(peer);
    let response = proxy_request_on_stream(
        "http://unused.invalid/".parse().unwrap(),
        request(),
        stream,
        &ProxyPolicy::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        !response
            .extensions()
            .get::<axum_reverse_proxy::ProxyError>()
            .unwrap()
            .is_connect()
    );
}

#[tokio::test]
async fn malformed_upgrade_is_rejected_and_connection_closed() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        headers(&mut peer).await;
        peer.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: wrong\r\n\r\n").await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let response = proxy_request_on_stream(
        "http://unused.invalid/".parse().unwrap(),
        websocket_request(),
        stream,
        &ProxyPolicy::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    server.await.unwrap();
}

#[tokio::test]
async fn dropping_response_body_closes_stream_without_draining() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        headers(&mut peer).await;
        peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let response = proxy_request_on_stream(
        "http://unused.invalid/".parse().unwrap(),
        request(),
        stream,
        &ProxyPolicy::default(),
    )
    .await
    .unwrap();
    drop(response);
    server.await.unwrap();
}

#[tokio::test]
async fn dropping_in_flight_future_closes_stream() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        proxy_request_on_stream(
            "http://unused.invalid/".parse().unwrap(),
            request(),
            stream,
            &ProxyPolicy::default(),
        )
        .await
    });
    headers(&mut peer).await;
    task.abort();
    let _ = task.await;
    assert_eq!(
        timeout(Duration::from_secs(2), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn connection_nominated_host_is_not_resurrected() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let seen = headers(&mut peer).await;
        assert!(seen.contains("host: authorized.internal"));
        assert!(!seen.lines().any(|line| line == "host: untrusted.example"));
        peer.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
    });
    let req = Request::builder()
        .uri("/")
        .header("host", "untrusted.example")
        .header("connection", "host")
        .body(Body::empty())
        .unwrap();
    let response = proxy_request_on_stream(
        "http://authorized.internal/".parse().unwrap(),
        req,
        stream,
        &ProxyPolicy::new().with_host_behaviour(axum_reverse_proxy::HostBehaviour::Preserve),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    server.await.unwrap();
}
