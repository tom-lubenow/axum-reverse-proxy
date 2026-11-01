//! Request admission and explicit shutdown using a Tower-compatible axum layer.
//! Run with `cargo run --example tunnel_lifecycle` (upstream: localhost:3001).
//!
//! This is an ownership/admission example, not a bounded server shutdown recipe.
//! Ordinary requests/bodies are not cancelled by the token and can delay
//! `axum::serve` shutdown indefinitely without application-specific deadlines.
//! A zero counter does not prove all sockets closed: hyper can still own a
//! transport awaiting upgrade handover. Bound HTTP driver shutdown separately.
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use axum_reverse_proxy::{ReverseProxy, TunnelCancellation, TunnelGuard};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Admission {
    permits: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    stop: CancellationToken,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn admit(State(state): State<Admission>, mut req: Request, next: Next) -> Response {
    // This example rejects overload instead of building an unbounded wait queue.
    let Ok(permit) = state.permits.clone().try_acquire_owned() else {
        return Response::builder().status(503).body(Body::empty()).unwrap();
    };
    state.active.fetch_add(1, Ordering::SeqCst);
    req.extensions_mut()
        .insert(TunnelGuard::new((permit, Active(state.active))));
    req.extensions_mut().insert(TunnelCancellation(state.stop));
    next.run(req).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Admission {
        permits: Arc::new(Semaphore::new(100)),
        active: Arc::new(AtomicUsize::new(0)),
        stop: CancellationToken::new(),
    };
    let app = Router::new()
        .fallback_service(ReverseProxy::new("/", "http://127.0.0.1:3001"))
        .layer(middleware::from_fn_with_state(state.clone(), admit));
    let shutdown = state.clone();
    axum::serve(tokio::net::TcpListener::bind("127.0.0.1:3000").await?, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.permits.close(); // stop new admissions first
            shutdown.stop.cancel(); // force-close tunnels; not a graceful WS close
        })
        .await?;
    // Hyper server shutdown alone does not wait for detached tunnels. The
    // caller-owned counter also covers ordinary response bodies and errors.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
