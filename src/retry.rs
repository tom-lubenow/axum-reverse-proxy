//! A retry layer for proxied requests.
//!
//! [`RetryLayer`] retries a request when the proxy failed to *connect* to the
//! upstream — detected via the [`ProxyError`](crate::ProxyError) extension the
//! proxy inserts into synthesized `502` responses with
//! [`is_connect()`](crate::ProxyError::is_connect) set. Because the request
//! was never sent in that case, replaying it is safe for any method. A `502`
//! returned by the upstream itself (or any other status) is never retried.
//!
//! To make replays possible, request bodies are buffered in memory up to a
//! configurable cap (default 2 MiB) before the first attempt. Bodies that
//! exceed the cap are streamed through unbuffered and are forwarded exactly
//! once, without retries.

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http_body_util::BodyExt;
use std::convert::Infallible;
use std::time::Duration;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};
use tracing::debug;

use crate::forward::ProxyError;

const DEFAULT_MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// Layer that retries requests the proxy failed to deliver to the upstream.
///
/// See the [module documentation](self) for retry semantics and body-buffering
/// behaviour.
#[derive(Clone)]
pub struct RetryLayer {
    attempts: usize,
    delay: Duration,
    max_buffer_bytes: usize,
}

impl RetryLayer {
    /// Create a layer that makes up to `attempts` total attempts, waiting
    /// 500ms between them. `attempts` is clamped to at least 1.
    pub fn new(attempts: usize) -> Self {
        Self::with_delay(attempts, Duration::from_millis(500))
    }

    /// Create a layer with an explicit delay between attempts.
    pub fn with_delay(attempts: usize, delay: Duration) -> Self {
        Self {
            attempts: attempts.max(1),
            delay,
            max_buffer_bytes: DEFAULT_MAX_BUFFER_BYTES,
        }
    }

    /// Set the maximum request body size that will be buffered for replay.
    /// Requests with larger bodies are forwarded once, without retries.
    #[must_use]
    pub fn with_max_buffer_bytes(mut self, max_buffer_bytes: usize) -> Self {
        self.max_buffer_bytes = max_buffer_bytes;
        self
    }
}

#[derive(Clone)]
pub struct Retry<S> {
    inner: S,
    attempts: usize,
    delay: Duration,
    max_buffer_bytes: usize,
}

impl<S> Layer<S> for RetryLayer {
    type Service = Retry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry {
            inner,
            attempts: self.attempts,
            delay: self.delay,
            max_buffer_bytes: self.max_buffer_bytes,
        }
    }
}

/// Whether a response is a proxy-synthesized "could not reach upstream"
/// failure that is safe to retry.
fn is_retryable(res: &axum::http::Response<Body>) -> bool {
    res.extensions()
        .get::<ProxyError>()
        .map(|e| e.is_connect())
        .unwrap_or(false)
}

/// Outcome of trying to buffer a request body for replay.
enum BufferedRequestBody {
    /// The complete body, replayable.
    Complete(Bytes),
    /// The body exceeded the cap: the buffered prefix plus the untouched
    /// remainder, forwardable exactly once.
    TooLarge(Body),
}

/// Buffer up to `max` bytes of `body`. Trailer frames are not replayable and
/// are dropped (with a debug log) if present.
async fn buffer_body(mut body: Body, max: usize) -> Result<BufferedRequestBody, axum::Error> {
    let mut buf = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        match frame.into_data() {
            Ok(data) => {
                if buf.len() + data.len() > max {
                    // Too large to replay: chain what we've read back onto the
                    // rest of the stream and forward it once.
                    let prefix = buf.freeze();
                    let rest = body.into_data_stream();
                    let chained =
                        stream::iter([Ok::<_, axum::Error>(prefix), Ok(data)]).chain(rest);
                    return Ok(BufferedRequestBody::TooLarge(Body::from_stream(chained)));
                }
                buf.extend_from_slice(&data);
            }
            Err(_trailers) => {
                debug!("RetryLayer: dropping request trailers (not replayable)");
            }
        }
    }
    Ok(BufferedRequestBody::Complete(buf.freeze()))
}

impl<S> Service<axum::http::Request<Body>> for Retry<S>
where
    S: Service<
            axum::http::Request<Body>,
            Response = axum::http::Response<Body>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let attempts = self.attempts;
        let delay = self.delay;
        let max_buffer_bytes = self.max_buffer_bytes;
        Box::pin(async move {
            if attempts == 1 {
                return inner.call(req).await;
            }

            let (parts, body) = req.into_parts();
            let bytes = match buffer_body(body, max_buffer_bytes).await {
                Ok(BufferedRequestBody::Complete(bytes)) => bytes,
                Ok(BufferedRequestBody::TooLarge(body)) => {
                    debug!(
                        "RetryLayer: request body exceeds {max_buffer_bytes} bytes, forwarding without retry"
                    );
                    let req = axum::http::Request::from_parts(parts, body);
                    return inner.call(req).await;
                }
                Err(e) => {
                    // The client's body stream itself failed; nothing to send.
                    debug!("RetryLayer: failed to read request body: {e}");
                    return Ok(axum::http::Response::builder()
                        .status(axum::http::StatusCode::BAD_REQUEST)
                        .body(Body::from("Failed to read request body"))
                        .unwrap());
                }
            };

            let mut attempt = 0;
            loop {
                attempt += 1;
                let req = axum::http::Request::from_parts(parts.clone(), Body::from(bytes.clone()));
                let res = inner.call(req).await?;
                if !is_retryable(&res) || attempt >= attempts {
                    return Ok(res);
                }
                debug!(
                    "RetryLayer: upstream connect failed (attempt {attempt}/{attempts}), retrying in {delay:?}"
                );
                tokio::time::sleep(delay).await;
            }
        })
    }
}
