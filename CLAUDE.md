# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Essential Commands

### Development
- **Build**: `cargo build` / `cargo build --release`
- **Check**: `cargo check`
- **Format**: `cargo fmt`
- **Lint**: `cargo clippy -- -D warnings` (use `--features full` for all features)
- **Fix linting**: `cargo clippy --fix --allow-dirty --allow-staged -- -D warnings`

### Testing
The project uses **cargo-nextest** for faster test execution:
- **Run tests**: `cargo nextest run`
- **Run tests with all features**: `cargo nextest run --features full`
- **Run doctests**: `cargo test --doc` (nextest doesn't support doctests)
- **Run a single test**: `cargo nextest run <test_name>`

### Before Committing
Always run these commands to ensure code quality:
1. `cargo fmt`
2. `cargo clippy --features full -- -D warnings`
3. `cargo nextest run --features full`

## Architecture Overview

This is a reverse proxy library for Axum that provides both simple and advanced proxying capabilities.

### Core Components

1. **`proxy.rs`**: Basic `ReverseProxy` for single upstream proxying
   - Simple path-based routing
   - Request/response modification hooks
   - Header forwarding (X-Forwarded-For, X-Real-IP, etc.)

2. **`balanced_proxy.rs`**: Load balancing implementations
   - `BalancedProxy`: Static upstream list with round-robin
   - `DiscoverableBalancedProxy`: Dynamic service discovery with pluggable strategies:
     - Round-robin
     - Power of Two Choices (P2C) with pending requests
     - P2C with Exponentially Weighted Moving Average (EWMA)

3. **`websocket.rs`**: WebSocket upgrade handling
   - Bidirectional message forwarding
   - Proper connection lifecycle management
   - Integrated with both proxy types

4. **`dns_discovery.rs`** (optional feature): DNS-based service discovery
   - A/AAAA record resolution
   - SRV record support
   - Configurable refresh intervals

5. **Middleware Integration**:
   - `retry.rs`: Tower layer for automatic retries
   - `rfc9110.rs`: RFC9110 compliance layer
   - Full Tower middleware compatibility

### Feature Flags
- `default = ["tls"]`: TLS support via rustls
- `tls`: HTTPS support using hyper-rustls
- `native-tls`: Alternative TLS implementation
- `dns`: DNS-based service discovery
- `full`: Enables both `tls` and `dns`

### Key Design Patterns
- All proxy types implement Tower `Service` trait
- Extensive use of async/await for non-blocking I/O
- Builder pattern for configuration
- Type-safe error handling with custom error types
- Tracing instrumentation throughout

## Testing Strategy
- Unit tests colocated with modules in `src/`
- Integration tests in `tests/` covering real proxy scenarios
- Test both with default features and `--features full`

## Common Development Tasks
When adding new features:
1. Consider if it should be behind a feature flag
2. Add appropriate tracing instrumentation
3. Ensure Tower middleware compatibility
4. Write both unit and integration tests
5. Update examples if the feature affects public API
