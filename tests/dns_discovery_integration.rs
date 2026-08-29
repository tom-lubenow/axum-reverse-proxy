#![cfg(feature = "dns")]

use axum_reverse_proxy::{
    DiscoverableBalancedProxy, DnsDiscovery, DnsDiscoveryConfig, StaticDnsDiscovery,
};
use futures_util::StreamExt;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use std::time::Duration;
use tower::discover::Change;

#[tokio::test]
async fn test_dns_discovery_insert_and_proxy_update() {
    let config =
        DnsDiscoveryConfig::new("localhost", 80).with_refresh_interval(Duration::from_millis(100));

    // Discovery for validating events
    let mut event_discovery = DnsDiscovery::new(config.clone()).expect("create discovery");
    // Discovery used by the proxy
    let proxy_discovery = DnsDiscovery::new(config).expect("create discovery for proxy");

    // Poll the stream for an insert event
    let change = tokio::time::timeout(Duration::from_secs(2), event_discovery.next())
        .await
        .expect("timeout waiting for dns event")
        .expect("stream ended")
        .expect("dns resolution error");

    match change {
        Change::Insert(ip, service) => {
            assert!(service.contains(&ip.to_string()));
        }
        _ => panic!("expected Change::Insert"),
    }

    // Create proxy using the discovery
    let connector = HttpConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector);
    let proxy = DiscoverableBalancedProxy::new_with_client("/", client, proxy_discovery);

    proxy.start_discovery().await;
    // wait briefly for discovery task to populate
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(proxy.service_count().await > 0);
}

#[tokio::test]
async fn test_static_dns_discovery_insert_event() {
    let config = DnsDiscoveryConfig::new("localhost", 8080);
    let mut discovery = StaticDnsDiscovery::new(config).expect("create static discovery");

    let change = tokio::time::timeout(Duration::from_secs(2), discovery.next())
        .await
        .expect("timeout waiting for static dns event")
        .expect("stream ended")
        .expect("dns resolution error");

    match change {
        Change::Insert(_ip, service) => {
            assert!(service.ends_with(":8080"));
        }
        _ => panic!("expected Change::Insert"),
    }
}
