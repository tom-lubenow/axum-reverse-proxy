#[cfg(any(feature = "tls", feature = "native-tls"))]
#[cfg(test)]
mod tests {
    use axum::{Router, routing::get};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty};
    use hyper::{Request, StatusCode};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    #[cfg(feature = "native-tls")]
    use axum_reverse_proxy::create_dangerous_native_tls_connector;
    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    use axum_reverse_proxy::create_dangerous_rustls_config;

    // Create a test server with self-signed certificate
    async fn start_https_test_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        // The local TLS fixture uses tokio-rustls even when only native-tls
        // is enabled for the proxy (the optional direct rustls dep is absent).
        use tokio_rustls::{
            TlsAcceptor,
            rustls::{
                self,
                pki_types::{CertificateDer, PrivateKeyDer},
            },
        };

        // Generate self-signed certificate for testing
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();

        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();

        // Set ALPN protocols for HTTP/1.1
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let acceptor = TlsAcceptor::from(Arc::new(config));

        let app = Router::new().route("/", get(|| async { "Hello from self-signed HTTPS server" }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                hyper::service::service_fn(move |req| {
                                    let app = app.clone();
                                    async move { app.oneshot(req).await }
                                }),
                            )
                            .await;
                    }
                });
            }
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        (addr, handle)
    }

    #[tokio::test]
    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    async fn test_dangerous_rustls_accepts_self_signed_cert() {
        // Start HTTPS server with self-signed certificate
        let (addr, server_handle) = start_https_test_server().await;

        // Create dangerous rustls config
        let tls_config = create_dangerous_rustls_config();

        // Build HTTPS connector with dangerous config
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_http1()
            .build();

        // Create client
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        // Make request to self-signed server - should succeed with dangerous config
        let uri = format!("https://localhost:{}/", addr.port());
        let response = client
            .request(Request::get(uri).body(Empty::<Bytes>::new()).unwrap())
            .await
            .expect("Request should succeed with dangerous config");

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "Hello from self-signed HTTPS server");

        server_handle.abort();
    }

    #[tokio::test]
    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    async fn test_normal_rustls_rejects_self_signed_cert() {
        // Start HTTPS server with self-signed certificate
        let (addr, server_handle) = start_https_test_server().await;

        // Create normal rustls config (with proper verification)
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();

        // Build HTTPS connector with normal config
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_http1()
            .build();

        // Create client
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        // Make request to self-signed server - should fail with normal config
        let uri = format!("https://localhost:{}/", addr.port());
        let result = client
            .request(Request::get(uri).body(Empty::<Bytes>::new()).unwrap())
            .await;

        // Should fail due to certificate verification
        assert!(result.is_err(), "Request should fail with normal config");

        server_handle.abort();
    }

    #[tokio::test]
    #[cfg(feature = "native-tls")]
    async fn test_dangerous_native_tls_accepts_self_signed_cert() {
        // Start HTTPS server with self-signed certificate
        let (addr, server_handle) = start_https_test_server().await;

        // Create dangerous native-tls connector
        let tls = create_dangerous_native_tls_connector()
            .expect("Failed to create dangerous TLS connector");

        // Create HTTP connector
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);

        // Convert to tokio connector
        let tls = tokio_native_tls::TlsConnector::from(tls);

        // Create HTTPS connector
        let https = hyper_tls::HttpsConnector::from((http, tls));

        // Create client
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        // Make request to self-signed server - should succeed with dangerous config
        let uri = format!("https://localhost:{}/", addr.port());
        let response = client
            .request(Request::get(uri).body(Empty::<Bytes>::new()).unwrap())
            .await
            .expect("Request should succeed with dangerous config");

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "Hello from self-signed HTTPS server");

        server_handle.abort();
    }

    #[tokio::test]
    #[cfg(feature = "native-tls")]
    async fn test_normal_native_tls_rejects_self_signed_cert() {
        // Start HTTPS server with self-signed certificate
        let (addr, server_handle) = start_https_test_server().await;

        // Create normal native-tls connector (with proper verification)
        let tls = native_tls::TlsConnector::new().expect("Failed to create TLS connector");

        // Create HTTP connector
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);

        // Convert to tokio connector
        let tls = tokio_native_tls::TlsConnector::from(tls);

        // Create HTTPS connector
        let https = hyper_tls::HttpsConnector::from((http, tls));

        // Create client
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

        // Make request to self-signed server - should fail with normal config
        let uri = format!("https://localhost:{}/", addr.port());
        let result = client
            .request(Request::get(uri).body(Empty::<Bytes>::new()).unwrap())
            .await;

        // Should fail due to certificate verification
        assert!(result.is_err(), "Request should fail with normal config");

        server_handle.abort();
    }

    #[test]
    #[cfg(all(feature = "tls", not(feature = "native-tls")))]
    fn test_dangerous_rustls_config_properties() {
        // Test that the dangerous config has expected properties
        let config = create_dangerous_rustls_config();

        // Config was created successfully - the dangerous verifier is installed
        drop(config);
    }

    #[test]
    #[cfg(feature = "native-tls")]
    fn test_dangerous_native_tls_connector_creation() {
        // Test that we can create a dangerous native-tls connector
        let connector = create_dangerous_native_tls_connector()
            .expect("Failed to create dangerous TLS connector");

        // We can't easily test the internal state of native-tls,
        // but we verified it accepts self-signed certs in the integration test above
        drop(connector);
    }
}
