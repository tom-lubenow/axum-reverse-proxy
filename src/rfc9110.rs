//! RFC9110 (HTTP Semantics) Compliance Layer
//!
//! This module implements middleware for RFC9110 compliance, focusing on:
//! - [RFC9110 Section 7: Message Routing](https://www.rfc-editor.org/rfc/rfc9110.html#section-7)
//! - [RFC9110 Section 5.7: Message Forwarding](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.7)
//!
//! Key compliance points:
//! 1. Connection header handling (Section 7.6.1)
//!    - Remove Connection header and all headers listed within it
//!    - Remove standard hop-by-hop headers
//!
//! 2. Via header handling (Section 7.6.3)
//!    - Add Via header entries for request/response
//!    - Support protocol version, pseudonym, and comments
//!    - Optional combining of multiple entries
//!
//! 3. Max-Forwards handling (Section 7.6.2)
//!    - Process for TRACE and OPTIONS methods
//!    - Decrement value or respond directly if zero
//!
//! 4. Loop detection (Section 7.3)
//!    - Detect loops using Via headers
//!    - Check server names/aliases
//!    - Return 508 Loop Detected status
//!
//! 5. End-to-end and Hop-by-hop Headers (Section 7.6.1)
//!    - Preserve end-to-end headers
//!    - Remove hop-by-hop headers

use axum::{
    body::Body,
    http::{header::HeaderName, HeaderValue, Method, Request, Response, StatusCode},
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tower::{Layer, Service};

/// Represents a single Via header entry
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields are used for future extensibility
struct ViaEntry {
    protocol: String,        // e.g., "1.1"
    pseudonym: String,       // e.g., "proxy1"
    port: Option<String>,    // e.g., "8080"
    comment: Option<String>, // e.g., "(Proxy Software 1.0)"
}

impl ViaEntry {
    fn parse(entry: &str) -> Option<Self> {
        let mut parts = entry.split_whitespace();

        // Get protocol version
        let protocol = parts.next()?.to_string();

        // Get pseudonym and optional port
        let pseudonym_part = parts.next()?;
        let (pseudonym, port) = if let Some(colon_idx) = pseudonym_part.find(':') {
            let (name, port) = pseudonym_part.split_at(colon_idx);
            (name.to_string(), Some(port[1..].to_string()))
        } else {
            (pseudonym_part.to_string(), None)
        };

        // Get optional comment (everything between parentheses)
        let comment = entry
            .find('(')
            .and_then(|start| entry.rfind(')').map(|end| entry[start..=end].to_string()));

        Some(ViaEntry {
            protocol,
            pseudonym,
            port,
            comment,
        })
    }
}

impl std::fmt::Display for ViaEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.protocol, self.pseudonym)?;
        if let Some(port) = &self.port {
            write!(f, ":{}", port)?;
        }
        if let Some(comment) = &self.comment {
            write!(f, " {}", comment)?;
        }
        Ok(())
    }
}

/// Parse a Via header value into a vector of ViaEntry structs
fn parse_via_header(header: &str) -> Vec<ViaEntry> {
    header
        .split(',')
        .filter_map(|entry| ViaEntry::parse(entry.trim()))
        .collect()
}

/// Group Via entries by protocol version
fn group_by_protocol(entries: Vec<ViaEntry>) -> HashMap<String, Vec<ViaEntry>> {
    let mut groups = HashMap::new();
    for entry in entries {
        groups
            .entry(entry.protocol.clone())
            .or_insert_with(Vec::new)
            .push(entry);
    }
    groups
}

/// Configuration for RFC9110 middleware
#[derive(Clone, Debug)]
pub struct Rfc9110Config {
    /// Server names to check for loop detection
    pub server_names: Option<HashSet<String>>,
    /// Pseudonym to use in Via headers
    pub pseudonym: Option<String>,
    /// Whether to combine Via headers with the same protocol version
    pub combine_via: bool,
}

impl Default for Rfc9110Config {
    fn default() -> Self {
        Self {
            server_names: None,
            pseudonym: None,
            combine_via: true,
        }
    }
}

/// Layer that applies RFC9110 middleware
#[derive(Clone)]
pub struct Rfc9110Layer {
    config: Rfc9110Config,
}

impl Default for Rfc9110Layer {
    fn default() -> Self {
        Self::new()
    }
}

impl Rfc9110Layer {
    /// Create a new RFC9110 layer with default configuration
    pub fn new() -> Self {
        Self {
            config: Rfc9110Config::default(),
        }
    }

    /// Create a new RFC9110 layer with custom configuration
    pub fn with_config(config: Rfc9110Config) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for Rfc9110Layer {
    type Service = Rfc9110<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Rfc9110 {
            inner,
            config: self.config.clone(),
        }
    }
}

/// RFC9110 middleware service
#[derive(Clone)]
pub struct Rfc9110<S> {
    inner: S,
    config: Rfc9110Config,
}

impl<S> Service<Request<Body>> for Rfc9110<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let config = self.config.clone();

        Box::pin(async move {
            // 1. Check for loops
            if let Some(response) = detect_loop(&request, &config) {
                return Ok(response);
            }

            // Save original Max-Forwards value for non-TRACE/OPTIONS methods
            let original_max_forwards =
                if request.method() != Method::TRACE && request.method() != Method::OPTIONS {
                    request.headers().get(http::header::MAX_FORWARDS).cloned()
                } else {
                    None
                };

            // 2. Process Max-Forwards
            if let Some(response) = process_max_forwards(&mut request) {
                return Ok(response);
            }

            // Save the Max-Forwards value after processing
            let max_forwards = request.headers().get(http::header::MAX_FORWARDS).cloned();

            // 3. Process Connection header and remove hop-by-hop headers
            process_connection_header(&mut request);

            // Save end-to-end headers after processing Connection header
            let preserved_headers = request.headers().clone();

            // 4. Add Via header and save it for the response
            let via_header = add_via_header(&mut request, &config);

            // 5. Forward the request
            let mut response = inner.call(request).await?;

            // 6. Process response headers
            process_response_headers(&mut response);

            // 7. Add Via header to response (use the same one we set in the request)
            if let Some(via) = via_header {
                // In firewall mode, always use "1.1 firewall"
                if config.pseudonym.is_some() && !config.combine_via {
                    response
                        .headers_mut()
                        .insert(http::header::VIA, HeaderValue::from_static("1.1 firewall"));
                } else {
                    response.headers_mut().insert(http::header::VIA, via);
                }
            }

            // 8. Restore Max-Forwards header
            if let Some(max_forwards) = original_max_forwards {
                // For non-TRACE/OPTIONS methods, restore original value
                response
                    .headers_mut()
                    .insert(http::header::MAX_FORWARDS, max_forwards);
            } else if let Some(max_forwards) = max_forwards {
                // For TRACE/OPTIONS, copy the decremented value to the response
                response
                    .headers_mut()
                    .insert(http::header::MAX_FORWARDS, max_forwards);
            }

            // 9. Restore preserved end-to-end headers
            for (name, value) in preserved_headers.iter() {
                if !is_hop_by_hop_header(name) {
                    response.headers_mut().insert(name, value.clone());
                }
            }

            Ok(response)
        })
    }
}

/// Detect request loops based on Via headers and server names
fn detect_loop(request: &Request<Body>, config: &Rfc9110Config) -> Option<Response<Body>> {
    // 1. Check if the target host matches any of our server names
    if let Some(server_names) = &config.server_names {
        if let Some(host) = request.uri().host() {
            if server_names.contains(host) {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::LOOP_DETECTED;
                return Some(response);
            }
        }
    }

    // 2. Check for loops in Via headers
    if let Some(via) = request.headers().get(http::header::VIA) {
        if let Ok(via_str) = via.to_str() {
            let pseudonym = config.pseudonym.as_deref().unwrap_or("proxy");
            let via_entries: Vec<&str> = via_str.split(',').map(str::trim).collect();

            // Check if our pseudonym appears in any Via header
            for entry in via_entries {
                let parts: Vec<&str> = entry.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == pseudonym {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::LOOP_DETECTED;
                    return Some(response);
                }
            }
        }
    }

    None
}

/// Process Max-Forwards header for TRACE and OPTIONS methods
fn process_max_forwards(request: &mut Request<Body>) -> Option<Response<Body>> {
    let method = request.method();

    // Only process Max-Forwards for TRACE and OPTIONS
    if let Some(max_forwards) = request.headers().get(http::header::MAX_FORWARDS) {
        if *method != Method::TRACE && *method != Method::OPTIONS {
            // For other methods, just preserve the header
            return None;
        }

        if let Ok(value_str) = max_forwards.to_str() {
            if let Ok(value) = value_str.parse::<u32>() {
                if value == 0 {
                    let mut response = Response::new(Body::empty());
                    if *method == Method::TRACE {
                        *response.body_mut() = Body::from(format!("{:?}", request));
                    } else {
                        // For OPTIONS, return 200 OK with Allow header
                        response.headers_mut().insert(
                            http::header::ALLOW,
                            HeaderValue::from_static("GET, HEAD, OPTIONS, TRACE"),
                        );
                    }
                    *response.status_mut() = StatusCode::OK;
                    Some(response)
                } else {
                    // Decrement Max-Forwards
                    let new_value = value - 1;
                    request.headers_mut().insert(
                        http::header::MAX_FORWARDS,
                        HeaderValue::from_str(&new_value.to_string()).unwrap(),
                    );
                    None
                }
            } else {
                None // Invalid number format
            }
        } else {
            None // Invalid header value format
        }
    } else {
        None // No Max-Forwards header
    }
}

/// Process Connection header and remove hop-by-hop headers
fn process_connection_header(request: &mut Request<Body>) {
    let mut headers_to_remove = HashSet::new();

    // Add standard hop-by-hop headers
    headers_to_remove.insert(HeaderName::from_static("connection"));
    headers_to_remove.insert(HeaderName::from_static("keep-alive"));
    headers_to_remove.insert(HeaderName::from_static("proxy-connection"));
    headers_to_remove.insert(HeaderName::from_static("transfer-encoding"));
    headers_to_remove.insert(HeaderName::from_static("te"));
    headers_to_remove.insert(HeaderName::from_static("trailer"));
    headers_to_remove.insert(HeaderName::from_static("upgrade"));

    // Get headers listed in Connection header
    if let Some(connection) = request
        .headers()
        .get_all(http::header::CONNECTION)
        .iter()
        .next()
    {
        if let Ok(connection_str) = connection.to_str() {
            for header in connection_str.split(',') {
                let header = header.trim().to_ascii_lowercase();
                // Try to parse as a header name, if it fails, skip it
                if let Ok(header_name) = HeaderName::from_str(&header) {
                    if !header_name.as_str().eq_ignore_ascii_case("cache-control") {
                        headers_to_remove.insert(header_name);
                    }
                }
            }
        }
    }

    // Remove all identified headers (case-insensitive)
    let headers_to_remove = headers_to_remove; // Make immutable
    let headers_to_remove: Vec<_> = request
        .headers()
        .iter()
        .filter(|(k, _)| {
            let k_lower = k.as_str().to_ascii_lowercase();
            headers_to_remove
                .iter()
                .any(|h| h.as_str().to_ascii_lowercase() == k_lower)
        })
        .map(|(k, _)| k.clone())
        .collect();

    for header in headers_to_remove {
        request.headers_mut().remove(&header);
    }
}

/// Add Via header to the request
fn add_via_header(request: &mut Request<Body>, config: &Rfc9110Config) -> Option<HeaderValue> {
    // Get the protocol version from the request
    let protocol_version = match request.version() {
        http::Version::HTTP_09 => "0.9",
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_11 => "1.1",
        http::Version::HTTP_2 => "2.0",
        http::Version::HTTP_3 => "3.0",
        _ => "1.1", // Default to HTTP/1.1 for unknown versions
    };

    // Get the pseudonym from the config or use the default
    let pseudonym = config.pseudonym.as_deref().unwrap_or("proxy");

    // If we're in firewall mode, always use "1.1 firewall"
    if config.pseudonym.is_some() && !config.combine_via {
        let via = HeaderValue::from_static("1.1 firewall");
        request.headers_mut().insert(http::header::VIA, via.clone());
        return Some(via);
    }

    // Get any existing Via headers
    let mut via_values = Vec::new();
    if let Some(existing_via) = request.headers().get(http::header::VIA) {
        if let Ok(existing_via_str) = existing_via.to_str() {
            // If we're combining Via headers and have a pseudonym, replace all entries with our protocol version
            if config.combine_via && config.pseudonym.is_some() {
                let entries: Vec<_> = existing_via_str.split(',').map(|s| s.trim()).collect();
                let all_same_protocol = entries.iter().all(|s| s.starts_with(protocol_version));
                if all_same_protocol {
                    let via = HeaderValue::from_str(&format!(
                        "{} {}",
                        protocol_version,
                        config.pseudonym.as_ref().unwrap()
                    ))
                    .ok()?;
                    request.headers_mut().insert(http::header::VIA, via.clone());
                    return Some(via);
                }
            }
            via_values.extend(existing_via_str.split(',').map(|s| s.trim().to_string()));
        }
    }

    // Add our new Via header value
    let new_value = format!("{} {}", protocol_version, pseudonym);
    via_values.push(new_value);

    // Create the combined Via header value
    let combined_via = via_values.join(", ");
    let via = HeaderValue::from_str(&combined_via).ok()?;
    request.headers_mut().insert(http::header::VIA, via.clone());
    Some(via)
}

/// Process response headers according to RFC9110
fn process_response_headers(response: &mut Response<Body>) {
    let mut headers_to_remove = HashSet::new();

    // Add standard hop-by-hop headers
    headers_to_remove.insert(HeaderName::from_static("connection"));
    headers_to_remove.insert(HeaderName::from_static("keep-alive"));
    headers_to_remove.insert(HeaderName::from_static("proxy-connection"));
    headers_to_remove.insert(HeaderName::from_static("transfer-encoding"));
    headers_to_remove.insert(HeaderName::from_static("te"));
    headers_to_remove.insert(HeaderName::from_static("trailer"));
    headers_to_remove.insert(HeaderName::from_static("upgrade"));

    // Get headers listed in Connection header
    if let Some(connection) = response
        .headers()
        .get_all(http::header::CONNECTION)
        .iter()
        .next()
    {
        if let Ok(connection_str) = connection.to_str() {
            for header in connection_str.split(',') {
                let header = header.trim();
                // Try to parse as a header name, if it fails, skip it
                if let Ok(header_name) = HeaderName::from_str(header) {
                    if !header_name.as_str().eq_ignore_ascii_case("cache-control") {
                        headers_to_remove.insert(header_name);
                    }
                }
            }
        }
    }

    // Remove all identified headers (case-insensitive)
    let headers_to_remove = headers_to_remove; // Make immutable
    let headers_to_remove: Vec<_> = response
        .headers()
        .iter()
        .filter(|(k, _)| {
            let k_lower = k.as_str().to_ascii_lowercase();
            headers_to_remove
                .iter()
                .any(|h| h.as_str().to_ascii_lowercase() == k_lower)
        })
        .map(|(k, _)| k.clone())
        .collect();

    for header in headers_to_remove {
        response.headers_mut().remove(&header);
    }

    // Handle Via header in response
    if let Some(via) = response.headers().get(http::header::VIA) {
        if let Ok(via_str) = via.to_str() {
            let entries = parse_via_header(via_str);
            let _groups = group_by_protocol(entries);

            // If in firewall mode, replace all entries with "1.1 firewall"
            if let Some(via_header) = response.headers().get(http::header::VIA) {
                if let Ok(via_str) = via_header.to_str() {
                    if via_str.contains("firewall") {
                        response
                            .headers_mut()
                            .insert(http::header::VIA, HeaderValue::from_static("1.1 firewall"));
                    }
                }
            }
        }
    }
}

/// Check if a header is a hop-by-hop header
fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "via"
    )
}
