# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.1.0] - 2026-08-28

This release repositions the project (see the rewritten README: a library for
embedding proxy behaviour in axum apps, explicitly not an edge-proxy server),
hardens the trust infrastructure, and finishes more of the proxy-correctness
long tail. No breaking changes.

### Added

- `ProxyPolicy::with_public_scheme` — declares the scheme the proxy is
  publicly served over, enabling `X-Forwarded-Proto` and `proto=` in the
  RFC 7239 `Forwarded` header.
- RFC 7239 `Forwarded` support: under `XForwardedFor::Append` the proxy
  appends its `for=` element (with `host=`/`proto=` on the first element;
  IPv6 addresses bracketed and quoted per spec).
- `ProxyPolicy::with_upstream_timeout` — deadline for upstream response
  headers; expiry returns `504 Gateway Timeout` (with a `ProxyError`
  extension). Response bodies stream without a deadline.
- `ProxyPolicy::with_max_request_body_bytes` — request body cap: `413 Payload
  Too Large` for oversized declared lengths, mid-stream cutoff for chunked
  bodies that exceed it.
- `CONNECT` requests are now rejected explicitly with `501 Not Implemented`
  instead of being nonsensically forwarded.

### Fixed

- HTTP/2 clients under `HostBehaviour::Preserve`: the `:authority`
  pseudo-header (surfaced in the request URI) is now materialized as the
  upstream `Host` header and used for `X-Forwarded-Host`/`Forwarded`
  fallbacks. Previously h2 requests silently fell back to the upstream
  authority because no `Host` header exists.

### Infrastructure

- Declared MSRV (`rust-version = "1.88"`), verified by a dedicated CI job.
- CI now runs `cargo-semver-checks` against the last published release and a
  five-way feature-combination matrix.
- Property tests (proptest) for the URI-transform and path-joining logic —
  the code where both 2.0 path bugs lived.
- Unit tests for the P2C selection math and EWMA decay, which previously had
  none.
- A no-proxy baseline benchmark and `docs/BENCHMARKS.md` with measured
  per-request overhead and honest caveats.
- `CONTRIBUTING.md` and issue templates; the README now states the project's
  scope explicitly (and no longer aspires to compete with nginx).

## [2.0.0] - 2026-08-28

This release fixes several correctness and security bugs and makes the proxy's
defaults safe. Some fixes change observable behaviour, hence the major bump.

### Fixed

- **`RetryLayer` no longer replays truncated or empty request bodies.**
  Previously the retry buffer only held the bytes the failed attempt happened
  to read — a connect failure replayed an *empty* body while keeping the
  original `Content-Length`. Bodies are now buffered in full (up to a
  configurable cap, default 2 MiB) before the first attempt; larger bodies are
  forwarded once without retries.
- **`RetryLayer` no longer retries genuine upstream 502s or non-idempotent
  requests unsafely.** Retries now happen only when the proxy failed to
  *connect* to the upstream (detected via the new `ProxyError` response
  extension) — a case where the request was never sent and replay is safe for
  any method.
- **`proxy_route` with a static string target no longer drops the request
  path.** `proxy_route("/api/{*rest}", "https://api.example.com")` now forwards
  `/api/foo` to `https://api.example.com/foo` (as documented) instead of
  sending every request to the target's root path. Static string targets are
  base URLs; `proxy_template` and custom resolvers are used verbatim (see
  `TargetResolver::is_base_url`).
- **`DnsDiscoveryConfig::with_resolver_opts` is now applied.** Previously the
  options were silently ignored, and a custom `resolver_config` was only used
  when options were *also* set. Each is now honoured independently.
- **`DnsDiscovery`'s stream no longer risks a lost wakeup** when a clone holds
  the internal receiver lock.
- **P2C load metrics can no longer be attributed to the wrong endpoint.**
  Metrics now travel with each discovered endpoint instead of being tracked by
  vector index, so removals/replacements mid-list no longer shift another
  endpoint's pending-request count or EWMA onto the wrong upstream. A
  `Change::Insert` for an existing key now replaces the endpoint (per tower's
  `Discover` contract) instead of leaking the old one.
- **`start_discovery` is now idempotent** — calling it twice no longer spawns
  competing discovery tasks that duplicate endpoints. It also takes `&self`.
- **Invalid `ReverseProxy` targets now panic at construction, not per
  request.** The target URI is parsed once in `new`/`new_with_client` (with a
  clear message) instead of being re-parsed — and potentially panicking —
  inside request handling.
- **The RFC9110 layer's TRACE echo no longer reflects credentials.**
  `Max-Forwards: 0` TRACE responses are now `message/http` formatted and
  exclude `Authorization`, `Proxy-Authorization`, `Cookie`, and `Set-Cookie`
  (RFC 9110 §9.3.8). Previously the full request `Debug` output — including
  credentials — was echoed to the client.
- **WebSocket close frames are forwarded verbatim** — the peer now sees the
  original close code and reason instead of an empty close.

### Changed (breaking)

- **Hop-by-hop headers are stripped by default** in both directions
  (RFC 9110 §7.6.1): `Connection` and everything it nominates, `Keep-Alive`,
  `Proxy-Connection`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`.
  `te: trailers` is preserved so gRPC keeps working. The `Rfc9110Layer` is no
  longer required for this.
- **`X-Forwarded-For` is appended by default** (and `X-Forwarded-Host` set)
  when the client address is available via
  `into_make_service_with_connect_info::<SocketAddr>()`. Opt out with
  `ProxyPolicy::with_x_forwarded_for(XForwardedFor::Preserve)`.
- **`ProxyPolicy` is now `#[non_exhaustive]`** — construct it with
  `ProxyPolicy::new()` and the `with_*` builder methods instead of a struct
  literal. New fields: `x_forwarded_for`, `websocket_connect_timeout`.
- **Error responses no longer leak upstream error details.** Synthesized 502
  bodies are generic; the underlying error is logged and available
  programmatically via the `ProxyError` response extension. Failed WebSocket
  upgrades now return 502 (was 500) with a generic body.
- **The RFC9110 layer no longer copies `Max-Forwards` onto responses** (not
  part of the spec), and the undocumented "firewall mode" (which replaced the
  configured pseudonym with the literal `1.1 firewall` when
  `combine_via = false`) is removed.
- `ReverseProxy::new` / `new_with_client` accept independently-typed `path` and
  `target` arguments (previously both had to be the same `Into<String>` type).
- `RetryLayer` buffers request bodies before the first attempt (see Fixed);
  streaming-through behaviour is retained only for bodies above the cap.

### Added

- `ProxyError` response extension carrying the upstream failure message and
  whether it was a connect error (`is_connect()`).
- `XForwardedFor` policy enum (`Append` / `Preserve`).
- `ProxyPolicy::with_websocket_connect_timeout` (previously hardcoded to 5s).
- `ProxyRouterExt::proxy_route_with_client` to share a client/connection pool
  across proxy routes.
- `TargetResolver::is_base_url` (default `false`) controlling path appending.
- `RetryLayer::with_max_buffer_bytes` to tune the replay buffer cap.
- `BalancedProxy::with_policy` and `DiscoverableBalancedProxy::with_policy`.
- HTTP/2 (ALPN) support for TLS upstream connections (rustls feature).
- Response trailers are preserved, making gRPC proxying possible.

## [1.4.0] and earlier

See the git history.
