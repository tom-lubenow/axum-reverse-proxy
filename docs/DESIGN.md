# Design notes and decisions

Short records of the non-obvious decisions in this crate, so future
maintainers (and contributors) don't relitigate them without new information.

## Principles

1. **Library, not server.** See the README's Scope section. Anything that
   makes this crate feel like an ops-configured data plane (health checks,
   circuit breaking, config formats) is out of scope.
2. **Safe by default, knobs opt-in.** Hop-by-hop stripping, `X-Forwarded-For`
   appending, and generic error bodies are defaults; timeouts, body caps, and
   `Via` emission are opt-in `ProxyPolicy` builders. New behaviour that could
   surprise an upgrader ships behind a policy field.
3. **Every bug fix carries a regression test.** The path-joining logic is
   additionally covered by property tests; extend them when touching it.

## Decision: keep the custom P2C balancer (2026-08, v2.2 era)

We considered replacing `DiscoverableBalancedProxy`'s hand-rolled
Power-of-Two-Choices balancer with `tower::balance::p2c::Balance`, which is
maintained upstream and implements the same algorithm.

Decision: **keep the custom implementation.** Rationale:

- `tower::balance` is driven by `poll_ready` backpressure and consumes the
  `Discover` stream itself; our services are `Clone + Infallible` and always
  ready (hyper's pooled client handles connection readiness internally).
  Bridging the models requires wrapping every endpoint in `tower::load`
  wrappers and putting a `Buffer` in front (since `Balance` is not `Clone`),
  which adds a queue, a spawned worker, latency, and a new failure mode —
  for no behavioural gain.
- The custom implementation is small (~100 lines of selection + metrics),
  and since 2.1 the selection math and EWMA decay are unit-tested, which was
  the actual deficiency.
- The 2.0 rework made metrics travel with endpoints, eliminating the
  index-misattribution bug class that motivated the reconsideration.

Revisit if: tower ships a balance API compatible with cloneable
always-ready services, or the metrics/selection logic grows beyond what we
can confidently test ourselves.

## Decision: deprecate `Rfc9110Layer` (v2.2, removal in 3.0)

The layer's useful parts have migrated into the core proxy: hop-by-hop
stripping became default behaviour in 2.0; Via emission and pseudonym-based
loop detection became `ProxyPolicy::with_via` in 2.2. What remains in the
layer is Max-Forwards/TRACE handling (rarely used, and TRACE echo is a
security liability more often than a feature) and server-name loop detection
that only works for clients sending absolute URIs. Maintaining a parallel
half-spec layer is worse than owning the useful subset in core. Users who
rely on Max-Forwards handling can vendor the layer's code.

## Decision: proxy targets are validated at construction

`ReverseProxy::new` panics on an invalid target rather than returning
`Result`. The target is almost always a literal or config value known at
startup; failing fast there beats threading `Result` through every
constructor and re-checking per request. (Changed in 2.0 — previously the
parse happened, and could panic, on every request.)

## Decision: retries only replay requests that never reached the upstream

`RetryLayer` retries only proxy-synthesized connect failures (marked via the
`ProxyError` response extension), never upstream-produced statuses. Retrying
anything the upstream may have processed is unsafe for non-idempotent
methods, and heuristics about idempotency (method allowlists) still break on
misused GETs. "The connection was never established" is the only case where
replay is provably safe, so that's the line. Bodies are buffered up-front
(capped) because a partially-consumed streaming body can never be replayed
faithfully.
