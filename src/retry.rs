use axum::body::Body;
use http::Request;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tower::{Layer, Service};
use tracing::error;

/// Configuration for the [`RetryLayer`].
#[derive(Clone)]
pub struct RetryConfig {
    /// Number of times to retry a failed request.
    pub retries: u32,
    /// Backoff strategy used between retries.
    pub backoff: Arc<dyn Fn(u32) -> Duration + Send + Sync>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            retries: 3,
            backoff: Arc::new(|attempt| Duration::from_millis(500 * (1u64 << attempt))),
        }
    }
}

/// A [`Layer`] that retries failed requests with a configurable backoff.
#[derive(Clone, Default)]
pub struct RetryLayer {
    config: RetryConfig,
}

impl RetryLayer {
    /// Create a new layer with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new layer using the provided configuration.
    pub fn with_config(config: RetryConfig) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for RetryLayer {
    type Service = Retry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry {
            inner,
            config: self.config.clone(),
        }
    }
}

/// Service returned by [`RetryLayer`].
#[derive(Clone)]
pub struct Retry<S> {
    inner: S,
    config: RetryConfig,
}

impl<S> Service<Request<Body>> for Retry<S>
where
    S: Service<Request<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Display + Send + 'static,
    S::Response: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let config = self.config.clone();

        Box::pin(async move {
            let (parts, mut body) = req.into_parts();
            let mut retries = config.retries;
            let mut attempt = 0u32;

            loop {
                let req_body = if attempt == 0 {
                    std::mem::replace(&mut body, Body::empty())
                } else {
                    Body::empty()
                };
                let req = Request::from_parts(parts.clone(), req_body);

                match inner.call(req).await {
                    Ok(res) => return Ok(res),
                    Err(err) => {
                        if retries == 0 {
                            error!("Proxy error occurred after all retries err={}", err);
                            return Err(err);
                        }
                        error!(
                            "Proxy error occurred, retrying ({} left) err={}",
                            retries, err
                        );
                        let backoff = (config.backoff)(attempt);
                        attempt += 1;
                        retries -= 1;
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        })
    }
}
