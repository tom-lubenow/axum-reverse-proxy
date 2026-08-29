# Contributing to axum-reverse-proxy

Thanks for considering a contribution! This document explains how the project
works and what makes a change easy to accept.

## Scope

Read the **Scope** section of the README first. This crate is a library for
embedding reverse-proxy behaviour in axum applications — it is deliberately
*not* an edge proxy server. Features like health checking, circuit breaking,
outlier detection, config-file formats, and connection-level DDoS defenses are
out of scope and will be declined regardless of implementation quality.
Correctness fixes, protocol-conformance improvements, and safe-by-default
behaviour are always in scope.

## Development

```bash
cargo test --features full     # full test suite
cargo test --doc               # doc tests
cargo clippy --features full --all-targets -- -D warnings
cargo fmt --all
```

There is a nix flake (`nix develop`) matching what CI uses, but any recent
stable toolchain works. The MSRV is declared in `Cargo.toml` (`rust-version`)
and checked in CI.

CI also runs the feature matrix (`--no-default-features`, `native-tls`, `dns`,
`full`) and `cargo-semver-checks` against the last published release — if your
change breaks the public API, CI will tell you, and it will need to wait for a
major release.

## What a good PR looks like

- **Bug fixes come with a regression test.** Every bug fixed in 2.0 has one;
  keep that bar.
- **Behaviour changes update the docs** — rustdoc, README, and a CHANGELOG
  entry under an "Unreleased" heading.
- **New public API is minimal.** Prefer a builder method on `ProxyPolicy` over
  a new type; prefer a new type over a new module.
- Path-joining logic (`transform_uri`, `build_upstream_uri`) is covered by
  property tests — if you touch it, extend them.

## Releases

Releases are cut by the maintainer: version bump, CHANGELOG entry, tag, and a
GitHub release (which triggers the crates.io publish workflow). Breaking
changes are batched for major releases; please don't bump versions in PRs.

## Reporting bugs

Use the bug report issue template. The most useful thing you can include is a
minimal reproduction: the proxy setup (a few lines of Rust) plus the request
that misbehaves (`curl -v` output is ideal).

## Security issues

For anything that looks like a security vulnerability (request smuggling,
header injection, SSRF through target resolution, credential leakage), please
use GitHub's private vulnerability reporting on this repository instead of a
public issue.
