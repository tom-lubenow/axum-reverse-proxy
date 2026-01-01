//! Example of creating HTTPS clients that accept invalid certificates.
//!
//! WARNING: This should only be used for development/testing purposes!

use axum::{Router, routing::get};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("⚠️  Dangerous HTTPS Client Example");
    println!("==================================");
    println!();
    println!("This example shows how to create HTTPS clients that accept invalid certificates.");
    println!("This should ONLY be used for development/testing with self-signed certificates!");
    println!();

    // Example for rustls (default TLS backend)
    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    {
        use axum_reverse_proxy::create_dangerous_rustls_config;
        use bytes::Bytes;
        use http_body_util::Empty;
        use hyper_rustls::HttpsConnectorBuilder;
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::TokioExecutor;

        println!("Creating dangerous client with rustls...");

        // Create the dangerous TLS config
        let tls_config = create_dangerous_rustls_config();

        // Build HTTPS connector
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_http1()
            .build();

        // Create the client
        let _client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        println!("✓ Created hyper client that accepts invalid certificates");

        // Example usage:
        // let response = client
        //     .request(Request::get("https://self-signed.example.com")
        //         .body(Body::empty())
        //         .unwrap())
        //     .await
        //     .unwrap();
    }

    // Example for native-tls
    #[cfg(feature = "native-tls")]
    {
        use axum_reverse_proxy::create_dangerous_native_tls_connector;
        use bytes::Bytes;
        use http_body_util::Empty;
        use hyper_tls::HttpsConnector;
        use hyper_util::client::legacy::{Client, connect::HttpConnector};
        use hyper_util::rt::TokioExecutor;

        println!("Creating dangerous client with native-tls...");

        // Create the dangerous TLS connector
        let tls = create_dangerous_native_tls_connector().expect("Failed to create TLS connector");

        // Create HTTP connector
        let mut http = HttpConnector::new();
        http.enforce_http(false);

        // Convert to tokio connector
        let tls = tokio_native_tls::TlsConnector::from(tls);

        // Create HTTPS connector
        let https = HttpsConnector::from((http, tls));

        // Create the client
        let _client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        println!("✓ Created hyper client that accepts invalid certificates");
    }

    println!();
    println!("You can now use these clients to connect to servers with:");
    println!("- Self-signed certificates");
    println!("- Expired certificates");
    println!("- Wrong hostname certificates");
    println!("- Untrusted CA certificates");
    println!();

    // Simple server to keep the example running
    async fn root() -> &'static str {
        "Dangerous client example server"
    }

    let app = Router::new().route("/", get(root));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Example server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
