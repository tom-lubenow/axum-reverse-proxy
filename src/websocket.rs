//! HTTP/1.1 WebSocket upgrades share the ordinary request transport.
use http::{HeaderMap, HeaderValue};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::time::{Duration, timeout};

fn token(headers: &HeaderMap, name: &str, wanted: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(wanted))
}

pub(crate) fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    token(headers, "connection", "upgrade")
        && token(headers, "upgrade", "websocket")
        && headers.contains_key("sec-websocket-key")
        && headers.contains_key("sec-websocket-version")
}

pub(crate) fn upgrade_headers(headers: &mut HeaderMap) {
    headers.insert("connection", HeaderValue::from_static("upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
}

pub(crate) fn valid_upgrade_response(headers: &HeaderMap, request: &HeaderMap) -> bool {
    let Some(key) = request.get("sec-websocket-key") else {
        return false;
    };
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    token(headers, "connection", "upgrade")
        && token(headers, "upgrade", "websocket")
        && headers
            .get("sec-websocket-accept")
            .is_some_and(|v| v == accept.as_str())
        && headers
            .get("sec-websocket-protocol")
            .is_none_or(|selected| {
                selected.to_str().is_ok_and(|selected| {
                    request
                        .get_all("sec-websocket-protocol")
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .flat_map(|v| v.split(','))
                        .any(|offered| offered.trim() == selected)
                })
            })
}

pub(crate) fn bridge(
    downstream: OnUpgrade,
    upstream: OnUpgrade,
    guard: Option<crate::lifecycle::Resource>,
    cancellation: Option<crate::TunnelCancellation>,
) {
    tokio::spawn(async move {
        // Declared before the tunnel future so transports are dropped before
        // the caller observes resource release, including cancellation/unwind.
        let _guard = guard;
        let tunnel = async move {
            // Bound upgrade completion; on failure both sides are dropped together.
            match timeout(Duration::from_secs(5), async {
                tokio::try_join!(downstream, upstream)
            })
            .await
            {
                Ok(Ok((downstream, upstream))) => {
                    let mut downstream = TokioIo::new(downstream);
                    let mut upstream = TokioIo::new(upstream);
                    // Includes hyper's buffered bytes. EOF shuts down the opposite
                    // writer while the remaining direction drains; errors drop both.
                    if let Err(error) =
                        tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await
                    {
                        tracing::debug!(%error, "WebSocket tunnel closed");
                    }
                }
                result => tracing::debug!(?result, "WebSocket upgrade failed"),
            }
        };
        if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                _ = cancellation.0.cancelled() => {},
                _ = tunnel => {},
            }
        } else {
            tunnel.await;
        }
    });
}
