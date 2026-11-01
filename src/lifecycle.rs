use axum::body::Body;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    any::Any,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

pub(crate) type Resource = Box<dyn Any + Send>;

/// Owned request resources retained until the response body or upgraded tunnel ends.
///
/// Insert a fresh guard into each request's extensions. The resource need only
/// be `Send + 'static`: for example an owned semaphore permit, a drop-based
/// metrics guard, or a tuple of both. It need not implement `Clone` or `Sync`.
///
/// Clones share a single-consumption slot, to satisfy [`http::Extensions`]. The
/// first forwarding attempt atomically takes ownership; other clones become
/// empty and cannot delay release. Never reuse a guard for independent requests.
/// Retries need a fresh guard per attempt (place admission middleware inside the
/// retry layer). If never consumed, the last clone drops the resource.
///
/// Forwarding errors release immediately. Ordinary responses, including rejected
/// upgrades, retain it until body EOF, body error, or drop; known-empty bodies
/// release immediately. Completion means the proxy yielded the final body frame,
/// not that the downstream client consumed it. A successful 101 transfers it to
/// the tunnel task through upgrade waiting and transport teardown. Dropping the
/// 101 response alone does not cancel an established tunnel.
///
/// This tracks forwarding ownership, not all HTTP connections. If upgrade waiting
/// fails, times out, or is cancelled, the guard releases after dropping the upgrade
/// futures and any handed-over transports. Hyper may still own a transport whose
/// handover is pending; guard release does not prove that transport has closed.
/// See [`TunnelCancellation`] for shutdown scope.
///
/// Release uses Rust drop semantics, including task cancellation and unwinding;
/// process aborts and deliberately forgotten values cannot be observed this way.
///
/// An axum middleware function can be installed as a Tower layer with
/// `axum::middleware::from_fn_with_state(permits, admit)`:
///
/// ```
/// use axum::{extract::{Request, State}, middleware::Next, response::Response};
/// use axum_reverse_proxy::TunnelGuard;
/// use std::sync::Arc;
/// use tokio::sync::Semaphore;
///
/// async fn admit(
///     State(permits): State<Arc<Semaphore>>, mut req: Request, next: Next,
/// ) -> Response {
///     let permit = permits.acquire_owned().await.unwrap();
///     req.extensions_mut().insert(TunnelGuard::new(permit));
///     next.run(req).await
/// }
/// ```
///
/// See the repository's `examples/tunnel_lifecycle.rs` for overload rejection,
/// a drop-based active counter, and caller-controlled shutdown.
#[derive(Clone)]
pub struct TunnelGuard(Arc<Mutex<Option<Resource>>>);

impl TunnelGuard {
    /// Hold an owned resource for one forwarding attempt.
    pub fn new(resource: impl Any + Send) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(resource)))))
    }

    pub(crate) fn take(self) -> Option<Resource> {
        // No user code runs under this lock, and no lock survives an await.
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Optional request extension that cancels a successful upgraded tunnel.
///
/// Insert `TunnelCancellation(token.clone())` alongside a [`TunnelGuard`].
/// Cancellation is observed while awaiting both hyper upgrades and while copying
/// bytes. It drops handed-over transports, discarding buffered bytes; it does not
/// send a WebSocket close frame or gracefully drain. While handover is pending,
/// hyper still owns the transport: dropping the upgrade receiver only closes it
/// when hyper hands it over (or its connection driver is stopped). The guard can
/// release before that happens. To bound all connection teardown, callers must
/// also control their HTTP connection drivers/server shutdown.
/// Without cancellation, half-close
/// drains the other direction and hyper's buffered bytes are preserved.
///
/// This only controls tunnels after a valid 101. It does not cancel sending the
/// HTTP request, ordinary responses, or upstream non-101 rejections. An already
/// cancelled token still permits the HTTP handshake but closes a resulting
/// tunnel immediately. Cancellation wins when observed ready alongside progress;
/// bytes already copied cannot be recalled, and a 101 may already reach the client.
/// Callers must stop admission and track outstanding guards themselves for shutdown.
#[derive(Clone, Debug)]
pub struct TunnelCancellation(pub tokio_util::sync::CancellationToken);

pub(crate) fn retain(body: Body, guard: Option<Resource>) -> Body {
    if guard.is_none() || body.is_end_stream() {
        return body;
    }
    Body::new(GuardedBody { body, guard })
}

struct GuardedBody {
    body: Body,
    guard: Option<Resource>,
}

impl HttpBody for GuardedBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.body).poll_frame(cx);
        if matches!(frame, Poll::Ready(None | Some(Err(_)))) || this.body.is_end_stream() {
            this.guard.take();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Count(Arc<AtomicUsize>, std::cell::Cell<u8>);
    impl Drop for Count {
        fn drop(&mut self) {
            self.1.set(1);
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn clones_consume_once_even_concurrently_and_accept_non_sync_resources() {
        let count = Arc::new(AtomicUsize::new(0));
        let guard = TunnelGuard::new(Count(count.clone(), std::cell::Cell::new(0)));
        let mut extensions = http::Extensions::new();
        extensions.insert(guard.clone());
        let winners = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..16)
                .map(|_| {
                    let mut extensions = extensions.clone();
                    scope
                        .spawn(move || extensions.remove::<TunnelGuard>().unwrap().take().is_some())
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| usize::from(task.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(winners, 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop((guard, extensions));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unused_clones_and_unwinding_release() {
        let count = Arc::new(AtomicUsize::new(0));
        let guard = TunnelGuard::new(Count(count.clone(), std::cell::Cell::new(0)));
        let clone = guard.clone();
        drop(guard);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(clone);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let count = Arc::new(AtomicUsize::new(0));
        let guard = TunnelGuard::new(Count(count.clone(), std::cell::Cell::new(0)));
        let clone = guard.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = guard.take();
            panic!("unwind after consumption");
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(clone);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
