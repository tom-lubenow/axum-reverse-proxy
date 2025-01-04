use axum::{body::Body, extract::Request, routing::post, Router};
use axum_reverse_proxy::{ProxyOptions, ReverseProxy};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};
use tower::ServiceExt;

#[tokio::test]
async fn test_streaming_behavior() {
    // Create a counter to track chunks received
    let chunks_received = Arc::new(AtomicUsize::new(0));
    let chunks_received_clone = chunks_received.clone();

    // Create an echo server that will help us test streaming
    let echo = Router::new().route(
        "/",
        post(move |body: Body| {
            let chunks_received = chunks_received_clone.clone();
            async move {
                // Echo back the body, counting chunks as they arrive
                let stream = body.into_data_stream().inspect(move |_chunk| {
                    chunks_received.fetch_add(1, Ordering::SeqCst);
                });
                Body::from_stream(stream)
            }
        }),
    );

    // Bind echo server to a random port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn echo server
    tokio::spawn(async move {
        axum::serve(listener, echo).await.unwrap();
    });

    // Test streaming mode
    {
        let proxy = ReverseProxy::new("/", format!("http://{}", addr).as_str());
        let app: Router = proxy.into();

        // Create a body that sends chunks with delays
        let body = Body::from_stream(async_stream::stream! {
            // Send first chunk immediately
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 1024]));

            // Wait a bit before sending second chunk
            sleep(Duration::from_millis(100)).await;
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'b'; 1024]));

            // Wait again before sending final chunk
            sleep(Duration::from_millis(100)).await;
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'c'; 1024]));
        });

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .body(body)
            .unwrap();

        // Reset counter before starting
        chunks_received.store(0, Ordering::SeqCst);

        let start_time = std::time::Instant::now();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);

        // Verify that chunks are received over time
        let mut total_bytes = 0;
        let mut chunks_count = 0;
        let mut body = res.into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.unwrap();
            total_bytes += chunk.len();
            chunks_count += 1;

            // If we're streaming properly, we should receive chunks over time
            // not all at once at the end
            if chunks_count < 3 {
                assert!(
                    start_time.elapsed() >= Duration::from_millis(100 * (chunks_count - 1)),
                    "Chunks received too quickly, suggesting buffering"
                );
            }
        }

        // Verify we received all the data
        assert_eq!(total_bytes, 3 * 1024);
        assert_eq!(chunks_count, 3, "Should receive exactly 3 chunks");

        // Verify the echo server received chunks over time too
        assert_eq!(
            chunks_received.load(Ordering::SeqCst),
            3,
            "Echo server should receive exactly 3 chunks"
        );
    }

    // Test buffered mode
    {
        let proxy = ReverseProxy::new_with_options(
            "/",
            format!("http://{}", addr).as_str(),
            ProxyOptions {
                buffer_bodies: true,
            },
        );
        let app: Router = proxy.into();

        // Create the same delayed streaming body
        let body = Body::from_stream(async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 1024]));
            sleep(Duration::from_millis(100)).await;
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'b'; 1024]));
            sleep(Duration::from_millis(100)).await;
            yield Ok::<_, std::io::Error>(Bytes::from(vec![b'c'; 1024]));
        });

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .body(body)
            .unwrap();

        // Reset counter
        chunks_received.store(0, Ordering::SeqCst);

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);

        // In buffered mode, we should receive all data quickly after the initial delay
        let mut total_bytes = 0;
        let mut body = res.into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.unwrap();
            total_bytes += chunk.len();
        }

        // Verify we received all the data
        assert_eq!(total_bytes, 3 * 1024);

        // In buffered mode, the echo server should receive all data at once
        assert_eq!(
            chunks_received.load(Ordering::SeqCst),
            1,
            "Echo server should receive exactly 1 chunk in buffered mode"
        );
    }
}
