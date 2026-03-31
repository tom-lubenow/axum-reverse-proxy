use axum::body::Body;
use bytes::{Bytes, BytesMut};
use http::StatusCode;
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::BodyExt;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct RetryLayer {
    attempts: usize,
    delay: Duration,
}

impl RetryLayer {
    pub fn new(attempts: usize) -> Self {
        let attempts = attempts.max(1);
        Self {
            attempts,
            delay: Duration::from_millis(500),
        }
    }

    pub fn with_delay(attempts: usize, delay: Duration) -> Self {
        let attempts = attempts.max(1);
        Self { attempts, delay }
    }
}

#[derive(Clone)]
pub struct Retry<S> {
    inner: S,
    attempts: usize,
    delay: Duration,
}

struct BufferedBody<B> {
    body: B,
    buf: Arc<Mutex<BytesMut>>,
}

impl<B> BufferedBody<B> {
    fn new(body: B, buf: Arc<Mutex<BytesMut>>) -> Self {
        Self { body, buf }
    }
}

impl<B> HttpBody for BufferedBody<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.buf.lock().unwrap().extend_from_slice(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl<S> Layer<S> for RetryLayer {
    type Service = Retry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry {
            inner,
            attempts: self.attempts,
            delay: self.delay,
        }
    }
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
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let buf = Arc::new(Mutex::new(BytesMut::new()));
            let wrapped = BufferedBody::new(body, buf.clone());
            let attempt_req = axum::http::Request::from_parts(
                parts.clone(),
                Body::from_stream(wrapped.into_data_stream()),
            );
            let mut res = inner.call(attempt_req).await?;
            if res.status() != StatusCode::BAD_GATEWAY || attempts == 1 {
                return Ok(res);
            }

            for attempt in 1..attempts {
                tokio::time::sleep(delay).await;
                let bytes = buf.lock().unwrap().clone().freeze();
                let req = axum::http::Request::from_parts(parts.clone(), Body::from(bytes.clone()));
                res = inner.call(req).await?;
                if res.status() != StatusCode::BAD_GATEWAY || attempt == attempts - 1 {
                    return Ok(res);
                }
            }
            unreachable!();
        })
    }
}
