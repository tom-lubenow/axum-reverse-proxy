use axum::body::Body;
use axum_reverse_proxy::{
    ProxyPolicy, ReverseProxy, TunnelCancellation, TunnelGuard, proxy_request_on_stream,
};
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::{
        Client,
        connect::{Connected, Connection},
    },
    rt::{TokioExecutor, TokioIo},
};
use std::{
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Path {
    Client,
    Stream,
}
const PATHS: [Path; 2] = [Path::Client, Path::Stream];
const UPGRADE: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: client\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

struct Io(TokioIo<DuplexStream>);
impl Connection for Io {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}
impl hyper::rt::Read for Io {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl hyper::rt::Write for Io {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

async fn forward(
    path: Path,
    req: Request<Body>,
    stream: DuplexStream,
    policy: ProxyPolicy,
) -> Response<Body> {
    match path {
        Path::Stream => proxy_request_on_stream(
            "http://unused.invalid/".parse().unwrap(),
            req,
            stream,
            &policy,
        )
        .await
        .unwrap(),
        Path::Client => {
            let stream = Arc::new(Mutex::new(Some(stream)));
            let connector = tower::service_fn(move |_| {
                let stream = stream.lock().unwrap().take();
                Box::pin(async move {
                    stream
                        .map(|s| Io(TokioIo::new(s)))
                        .ok_or_else(|| std::io::Error::other("no second connection"))
                })
            });
            let client = Client::builder(TokioExecutor::new()).build(connector);
            ReverseProxy::new_with_client("/", "http://unused.invalid", client)
                .with_policy(policy)
                .proxy_request(req)
                .await
                .unwrap()
        }
    }
}

struct Count(Arc<AtomicUsize>);
impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn guarded(upgrade: bool) -> (Request<Body>, Arc<AtomicUsize>, http::Extensions) {
    let count = Arc::new(AtomicUsize::new(0));
    let mut req = Request::builder().uri("/");
    if upgrade {
        req = req
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-version", "13");
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(TunnelGuard::new(Count(count.clone())));
    let clones = req.extensions().clone();
    (req, count, clones)
}
async fn headers(peer: &mut DuplexStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            data.push(peer.read_u8().await.unwrap());
        }
        data
    })
    .await
    .unwrap()
}
async fn released(count: &AtomicUsize) {
    timeout(Duration::from_secs(2), async {
        while count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
async fn closed(peer: &mut DuplexStream) {
    assert_eq!(
        timeout(Duration::from_secs(2), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ordinary_empty_streaming_rejection_and_drop() {
    for path in PATHS {
        for (upgrade, empty, early_drop) in [
            (false, true, false),
            (false, false, false),
            (false, false, true),
            (true, false, false),
        ] {
            let (mut req, count, clones) = guarded(upgrade);
            // Cancellation does not affect ordinary bodies or rejected upgrades.
            let token = CancellationToken::new();
            token.cancel();
            req.extensions_mut().insert(TunnelCancellation(token));
            let (stream, mut peer) = tokio::io::duplex(4096);
            let task = tokio::spawn(forward(path, req, stream, ProxyPolicy::default()));
            headers(&mut peer).await;
            if empty {
                peer.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                    .await
                    .unwrap();
            } else {
                peer.write_all(if upgrade {
                    b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                })
                .await
                .unwrap();
            }
            let response = task.await.unwrap();
            if empty {
                released(&count).await;
            } else {
                assert_eq!(count.load(Ordering::SeqCst), 0);
                if !early_drop {
                    let mut body = response.into_body();
                    peer.write_all(b"3\r\none\r\n").await.unwrap();
                    assert_eq!(
                        body.frame().await.unwrap().unwrap().into_data().unwrap(),
                        "one"
                    );
                    assert_eq!(count.load(Ordering::SeqCst), 0);
                    peer.write_all(b"0\r\n\r\n").await.unwrap();
                    assert!(body.frame().await.is_none());
                    released(&count).await; // even while the completed body remains alive
                    drop(body);
                } else {
                    drop(response);
                }
                released(&count).await;
            }
            drop(clones);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn errors_timeouts_and_dropped_forward_future() {
    for path in PATHS {
        for mode in 0..5 {
            let (req, count, clones) = guarded(true);
            let (stream, mut peer) = tokio::io::duplex(4096);
            let task = tokio::spawn(forward(
                path,
                req,
                stream,
                ProxyPolicy::new().with_upstream_timeout(Duration::from_millis(50)),
            ));
            headers(&mut peer).await;
            match mode {
                0 => {
                    drop(peer);
                    assert_eq!(task.await.unwrap().status(), StatusCode::BAD_GATEWAY);
                }
                1 => {
                    assert_eq!(task.await.unwrap().status(), StatusCode::GATEWAY_TIMEOUT);
                    closed(&mut peer).await;
                }
                2 => {
                    task.abort();
                    let _ = task.await;
                    closed(&mut peer).await;
                }
                3 => {
                    peer.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: wrong\r\n\r\n").await.unwrap();
                    assert_eq!(task.await.unwrap().status(), StatusCode::BAD_GATEWAY);
                    closed(&mut peer).await;
                }
                _ => {
                    // No downstream hyper upgrade: bridge fails after 101.
                    peer.write_all(UPGRADE).await.unwrap();
                    assert_eq!(
                        task.await.unwrap().status(),
                        StatusCode::SWITCHING_PROTOCOLS
                    );
                    closed(&mut peer).await;
                }
            }
            released(&count).await;
            drop(clones);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
}

// Keep the downstream response pending to exercise cancellation/timeout while
// awaiting hyper's downstream OnUpgrade, with the upstream already upgraded.
async fn tunnel(
    path: Path,
    hold_response: bool,
    already_cancelled: bool,
    with_cancellation: bool,
) -> (
    DuplexStream,
    DuplexStream,
    Arc<AtomicUsize>,
    CancellationToken,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let (mut client, downstream) = tokio::io::duplex(4096);
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let token = CancellationToken::new();
    if already_cancelled {
        token.cancel();
    }
    let cancel = token.clone();
    let state = Mutex::new(Some((stream, gate_rx, ready_tx)));
    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let (stream, gate, ready) = state.lock().unwrap().take().unwrap();
        let mut req = req.map(Body::new);
        req.extensions_mut()
            .insert(TunnelGuard::new(Count(captured.clone())));
        if with_cancellation {
            req.extensions_mut()
                .insert(TunnelCancellation(cancel.clone()));
        }
        async move {
            let response = forward(path, req, stream, ProxyPolicy::default()).await;
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            ready.send(()).unwrap();
            if hold_response {
                let _ = gate.await;
            }
            Ok::<_, Infallible>(response)
        }
    });
    let driver = tokio::spawn(async move {
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(downstream), service)
            .with_upgrades()
            .await;
    });
    client.write_all(REQUEST).await.unwrap();
    headers(&mut peer).await;
    peer.write_all(UPGRADE).await.unwrap();
    ready_rx.await.unwrap();
    if !hold_response {
        assert!(headers(&mut client).await.starts_with(b"HTTP/1.1 101"));
    }
    (client, peer, count, token, gate_tx, driver)
}

#[tokio::test]
async fn tunnels_retain_through_half_close_and_cancel_active_or_waiting() {
    for path in PATHS {
        for mode in 0..4 {
            let hold = mode >= 2;
            let (mut client, mut peer, count, token, gate, driver) =
                tunnel(path, hold, mode == 3, mode != 0).await;
            if mode == 0 {
                assert_eq!(count.load(Ordering::SeqCst), 0);
                peer.write_all(b"buffered").await.unwrap();
                let mut bytes = [0; 8];
                client.read_exact(&mut bytes).await.unwrap();
                assert_eq!(&bytes, b"buffered");
                client.shutdown().await.unwrap();
                closed(&mut peer).await;
                assert_eq!(count.load(Ordering::SeqCst), 0); // half-close is not closure
                peer.write_all(b"tail").await.unwrap();
                peer.shutdown().await.unwrap();
                let mut rest = Vec::new();
                client.read_to_end(&mut rest).await.unwrap();
                assert_eq!(rest, b"tail");
            } else {
                if mode != 3 {
                    assert_eq!(count.load(Ordering::SeqCst), 0);
                    token.cancel();
                }
                closed(&mut peer).await;
                if !hold {
                    closed(&mut client).await;
                }
            }
            released(&count).await;
            drop(gate);
            driver.abort();
            let _ = driver.await;
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn upgrade_wait_timeout_releases_resource() {
    for path in PATHS {
        let (_client, mut peer, count, _token, gate, driver) =
            tunnel(path, true, false, false).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(6)).await;
        released(&count).await;
        closed(&mut peer).await;
        drop(gate);
        driver.abort();
        let _ = driver.await;
    }
}

#[tokio::test]
async fn body_final_frame_and_body_error_release_without_dropping_body() {
    for path in PATHS {
        for broken in [false, true] {
            let (req, count, clones) = guarded(false);
            let (stream, mut peer) = tokio::io::duplex(4096);
            let task = tokio::spawn(forward(path, req, stream, ProxyPolicy::default()));
            headers(&mut peer).await;
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
                .await
                .unwrap();
            let mut body = task.await.unwrap().into_body();
            assert_eq!(count.load(Ordering::SeqCst), 0);
            if broken {
                drop(peer);
                assert!(body.frame().await.unwrap().is_err());
            } else {
                peer.write_all(b"done").await.unwrap();
                assert_eq!(
                    body.frame().await.unwrap().unwrap().into_data().unwrap(),
                    "done"
                );
            }
            released(&count).await;
            drop((body, clones));
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn runtime_shutdown_drops_detached_tunnel_guard() {
    for path in PATHS {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (client, peer, count, _token, gate, driver) =
            runtime.block_on(tunnel(path, false, false, false));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(runtime);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop((client, peer, gate, driver));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn trailers_preserve_guard_until_completion() {
    for path in PATHS {
        let (req, count, clones) = guarded(false);
        let (stream, mut peer) = tokio::io::duplex(4096);
        let task = tokio::spawn(forward(path, req, stream, ProxyPolicy::default()));
        headers(&mut peer).await;
        peer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: grpc-status\r\n\r\n3\r\none\r\n").await.unwrap();
        let mut body = task.await.unwrap().into_body();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "one"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        peer.write_all(b"0\r\ngrpc-status: 0\r\n\r\n")
            .await
            .unwrap();
        let trailers = body
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers["grpc-status"], "0");
        assert!(body.frame().await.is_none());
        released(&count).await;
        drop((body, clones));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cancelled_pending_handover_is_not_a_connection_cleanup_barrier() {
    for path in PATHS {
        let (mut client, mut peer, count, token, gate, driver) =
            tunnel(path, true, false, true).await;
        token.cancel();
        released(&count).await;
        closed(&mut peer).await; // upstream was already handed over
        // The service still holds its 101. Hyper owns the downstream transport;
        // a released guard must not be advertised as proof of socket closure.
        assert!(
            timeout(Duration::from_millis(20), client.read(&mut [0]))
                .await
                .is_err()
        );
        gate.send(()).unwrap();
        assert!(headers(&mut client).await.starts_with(b"HTTP/1.1 101"));
        closed(&mut client).await; // rejected handover drops the transport
        driver.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
