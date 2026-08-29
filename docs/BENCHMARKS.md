# Benchmarks

Honest framing first: these numbers measure **per-request overhead of putting
this proxy between a client and an upstream**, on loopback, on one machine.
They are not a comparison with nginx/HAProxy/Envoy — this crate is a library
embedded in your axum app, not an edge proxy (see the Scope section of the
README) — and loopback microbenchmarks say nothing about behaviour under real
network conditions, TLS, or load.

## Method

`benches/proxy_bench.rs` (criterion) starts an axum test server and an
axum-reverse-proxy instance in front of it, then measures a `reqwest` GET:

- `direct_get` — client → upstream directly (the baseline)
- `http1_get` — client → proxy → upstream
- `http2` — same through the proxy's HTTP/2 path
- `large_payload` — 1 MiB POST echo through the proxy

Reproduce with:

```bash
cargo bench --bench proxy_bench
```

## Results (2026-08-28, v2.1.0)

Machine: AMD Ryzen AI Max+ 395, Linux 6.17, loopback. Criterion medians.

| benchmark        | median   |
|------------------|----------|
| `direct_get`     | 15.9 µs  |
| `http1_get`      | 27.0 µs  |
| `http2`          | 36.3 µs  |
| `large_payload` (1 MiB POST) | 298 µs |

## Reading the numbers

Interposing the proxy adds **~11 µs per request** end to end on this machine
(27.0 vs 15.9 µs). Note that this delta includes a whole extra loopback HTTP
hop (connection reuse, syscalls, scheduling), not just this crate's own logic
— the pure header-processing/forwarding cost is a fraction of it. For any
request that touches a real network or does real work upstream, the proxy's
contribution is noise.

The 1 MiB POST at ~298 µs corresponds to roughly 3.4 GiB/s through the proxy
(client → proxy → upstream → back), i.e. body streaming is not a bottleneck at
typical service payload sizes.

Numbers move with hardware, kernel, and dependency versions. Re-run the bench
on your own machine before drawing conclusions; treat this file as a sanity
reference and a regression baseline, not marketing.
