//! Router extension for adding proxy routes with dynamic target resolution.
//!
//! This module provides the [`ProxyRouterExt`] trait which extends [`axum::Router`]
//! with a convenient [`proxy_route`](ProxyRouterExt::proxy_route) method for adding
//! proxy routes with static or dynamic target URLs.
//!
//! # Example
//!
//! ```rust
//! use axum::Router;
//! use axum_reverse_proxy::{ProxyRouterExt, proxy_template};
//!
//! let app: Router = Router::new()
//!     // Static target: /api/foo/bar proxies to https://api.example.com/foo/bar
//!     .proxy_route("/api/{*rest}", "https://api.example.com")
//!     // Dynamic target with path parameter substitution
//!     .proxy_route("/users/{id}/profile", proxy_template("https://profiles.example.com/user/{id}"));
//! ```

use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{Request, Response, StatusCode, Uri},
    routing::any,
};
use http::uri::Builder as UriBuilder;
use hyper_util::client::legacy::{Client, connect::Connect};
use std::convert::Infallible;
use tracing::{error, trace};

use crate::{
    forward::{create_proxy_client, forward_request},
    proxy::ProxyPolicy,
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

/// A trait for resolving the target URL for a proxy request.
///
/// Implement this trait to provide custom target URL resolution logic.
/// The resolver receives the full request and path parameters, allowing
/// routing decisions based on headers, method, query parameters, etc.
///
/// # Built-in Implementations
///
/// - `String` and `&'static str`: Static *base* URLs. The part of the request
///   path not covered by the route's literal prefix is appended to the target's
///   path (see [`is_base_url`](TargetResolver::is_base_url)).
/// - [`TemplateTarget`]: Template-based URL with `{param}` substitution; the
///   resolved URL is used verbatim.
///
/// # Example
///
/// ```rust
/// use axum::body::Body;
/// use axum::http::Request;
/// use axum_reverse_proxy::TargetResolver;
///
/// #[derive(Clone)]
/// struct HeaderBasedResolver {
///     default_url: String,
///     premium_url: String,
/// }
///
/// impl TargetResolver for HeaderBasedResolver {
///     fn resolve(&self, req: &Request<Body>, _params: &[(String, String)]) -> String {
///         // Route premium users to a different backend
///         if req.headers().get("x-premium-user").is_some() {
///             self.premium_url.clone()
///         } else {
///             self.default_url.clone()
///         }
///     }
/// }
/// ```
pub trait TargetResolver: Clone + Send + Sync + 'static {
    /// Resolve the target URL based on the request and path parameters.
    ///
    /// # Arguments
    ///
    /// * `req` - The incoming HTTP request (headers, method, URI, etc.)
    /// * `params` - Path parameters extracted from the request URL as key-value pairs
    ///
    /// # Returns
    ///
    /// The target URL as a string. This should be a valid URL including scheme and host.
    fn resolve(&self, req: &Request<Body>, params: &[(String, String)]) -> String;

    /// Whether the resolved URL is a *base* URL that the request's remaining
    /// path should be appended to.
    ///
    /// When `true`, the route's literal prefix (everything before the first
    /// path parameter, e.g. `/api` in `/api/{*rest}`) is stripped from the
    /// request path and the remainder is appended to the resolved URL's path —
    /// the same joining behaviour as
    /// [`ReverseProxy`](crate::ReverseProxy). When `false` (the default), the
    /// resolved URL's path is used verbatim.
    ///
    /// The built-in `String` and `&'static str` resolvers return `true`;
    /// [`TemplateTarget`] and custom resolvers default to `false` since they
    /// construct the full upstream path themselves.
    fn is_base_url(&self) -> bool {
        false
    }
}

impl TargetResolver for String {
    fn resolve(&self, _req: &Request<Body>, _params: &[(String, String)]) -> String {
        self.clone()
    }

    fn is_base_url(&self) -> bool {
        true
    }
}

impl TargetResolver for &'static str {
    fn resolve(&self, _req: &Request<Body>, _params: &[(String, String)]) -> String {
        (*self).to_string()
    }

    fn is_base_url(&self) -> bool {
        true
    }
}

/// Characters that are percent-encoded in template parameter values to prevent
/// URL structure manipulation (SSRF). This encodes characters that could alter
/// the authority, query, or fragment components of the resolved URL.
///
/// Notably, `/` is NOT encoded to preserve catch-all route (`{*rest}`) behavior.
const TEMPLATE_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'@') // Prevents userinfo injection (SSRF via authority rewrite)
    .add(b'#') // Prevents fragment injection (URL truncation)
    .add(b'?') // Prevents query injection
    .add(b'\\') // Backslash treated as separator by some parsers
    .add(b'[') // Prevents IPv6 address injection
    .add(b']'); // Prevents IPv6 address injection

/// A template-based target resolver that substitutes path parameters into a URL template.
///
/// Template placeholders use the format `{param_name}` and are replaced with the
/// corresponding path parameter values from the request.
///
/// # Security
///
/// Parameter values are percent-encoded before substitution to prevent SSRF attacks.
/// Characters like `@`, `?`, `#`, and `\` are encoded so that injected values cannot
/// alter the URL's authority, query, or fragment components.
///
/// # Example
///
/// ```rust
/// use axum::Router;
/// use axum_reverse_proxy::{ProxyRouterExt, proxy_template};
///
/// let app: Router = Router::new()
///     // Request to /videos/abc123/720p proxies to https://cdn.example.com/v/abc123/res_720p
///     .proxy_route("/videos/{id}/{quality}", proxy_template("https://cdn.example.com/v/{id}/res_{quality}"));
/// ```
#[derive(Clone)]
pub struct TemplateTarget {
    template: String,
}

impl TemplateTarget {
    /// Create a new template target with the given URL template.
    ///
    /// # Arguments
    ///
    /// * `template` - A URL template with `{param}` placeholders
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
        }
    }
}

impl TargetResolver for TemplateTarget {
    fn resolve(&self, _req: &Request<Body>, params: &[(String, String)]) -> String {
        let mut result = self.template.clone();
        for (key, value) in params {
            let placeholder = format!("{{{}}}", key);
            let encoded = utf8_percent_encode(value, TEMPLATE_ENCODE_SET).to_string();
            result = result.replace(&placeholder, &encoded);
        }
        result
    }
}

/// Create a new [`TemplateTarget`] with the given URL template.
///
/// This is a convenience function for creating template-based target resolvers.
///
/// # Arguments
///
/// * `template` - A URL template with `{param}` placeholders
///
/// # Example
///
/// ```rust
/// use axum::Router;
/// use axum_reverse_proxy::{ProxyRouterExt, proxy_template};
///
/// let app: Router = Router::new()
///     .proxy_route("/api/{version}/{*path}", proxy_template("https://api.example.com/{version}/{path}"));
/// ```
pub fn proxy_template(template: impl Into<String>) -> TemplateTarget {
    TemplateTarget::new(template)
}

/// Extension trait for [`axum::Router`] that adds proxy routing capabilities.
///
/// This trait provides a convenient way to add proxy routes to an Axum router
/// with support for both static and dynamic target URLs.
pub trait ProxyRouterExt<S> {
    /// Add a proxy route that forwards requests to a target URL.
    ///
    /// The target can be:
    /// - A static string (`&str` or `String`), treated as a base URL: the part
    ///   of the request path beyond the route's literal prefix is appended
    /// - A [`TargetResolver`] such as [`TemplateTarget`] for dynamic URL
    ///   generation, used verbatim
    ///
    /// # Arguments
    ///
    /// * `path` - The route path pattern (e.g., `/api/{id}` or `/proxy/{*rest}`)
    /// * `target` - The target URL or resolver
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum::Router;
    /// use axum_reverse_proxy::{ProxyRouterExt, proxy_template};
    ///
    /// let app: Router = Router::new()
    ///     // Static proxy: /api/foo -> https://api.example.com/foo
    ///     .proxy_route("/api/{*rest}", "https://api.example.com")
    ///     // Dynamic proxy with path substitution
    ///     .proxy_route("/users/{id}", proxy_template("https://users.example.com/{id}"));
    /// ```
    fn proxy_route<T: TargetResolver>(self, path: &str, target: T) -> Self
    where
        Self: Sized,
    {
        self.proxy_route_with_policy(path, target, ProxyPolicy::default())
    }

    /// Add a proxy route with an explicit [`ProxyPolicy`].
    ///
    /// Identical to [`proxy_route`](Self::proxy_route) but lets the caller
    /// override forwarding behaviour (e.g. preserving the client's `Host`
    /// header). [`proxy_route`](Self::proxy_route) is the [`ProxyPolicy::default`]
    /// case of this method.
    fn proxy_route_with_policy<T: TargetResolver>(
        self,
        path: &str,
        target: T,
        policy: ProxyPolicy,
    ) -> Self
    where
        Self: Sized,
    {
        self.proxy_route_with_client(path, target, policy, create_proxy_client())
    }

    /// Add a proxy route using a caller-supplied HTTP client.
    ///
    /// Use this to share one connection pool across routes or to customize
    /// connector/client settings.
    fn proxy_route_with_client<T: TargetResolver, C>(
        self,
        path: &str,
        target: T,
        policy: ProxyPolicy,
        client: Client<C, Body>,
    ) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static;
}

impl<S> ProxyRouterExt<S> for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn proxy_route_with_client<T: TargetResolver, C>(
        self,
        path: &str,
        target: T,
        policy: ProxyPolicy,
        client: Client<C, Body>,
    ) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        // Literal prefix of the route pattern (up to the first path
        // parameter), used to compute the remaining path for base-URL targets.
        let route_prefix: String = route_literal_prefix(path).to_string();

        self.route(
            path,
            any(
                move |Path(params): Path<Vec<(String, String)>>, req: Request<Body>| {
                    let target = target.clone();
                    let client = client.clone();
                    let policy = policy.clone();
                    let route_prefix = route_prefix.clone();
                    async move {
                        proxy_request(target, params, req, client, &policy, &route_prefix).await
                    }
                },
            ),
        )
    }
}

/// The literal prefix of a route pattern: everything before the first path
/// parameter, without a trailing slash. E.g. `/api/{*rest}` -> `/api`.
fn route_literal_prefix(pattern: &str) -> &str {
    let literal = match pattern.find('{') {
        Some(idx) => &pattern[..idx],
        None => pattern,
    };
    literal.trim_end_matches('/')
}

/// Strip the route's literal prefix from a request path at a segment boundary.
/// Returns the remainder (empty or `/`-prefixed), or the full path when the
/// prefix doesn't apply.
fn strip_route_prefix<'a>(path: &'a str, prefix: &str) -> &'a str {
    if prefix.is_empty() {
        return path;
    }
    if let Some(rem) = path.strip_prefix(prefix)
        && (rem.is_empty() || rem.starts_with('/'))
    {
        return rem;
    }
    path
}

async fn proxy_request<T: TargetResolver, C>(
    target: T,
    params: Vec<(String, String)>,
    req: Request<Body>,
    client: Client<C, Body>,
    policy: &ProxyPolicy,
    route_prefix: &str,
) -> Result<Response<Body>, Infallible>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let target_url = target.resolve(&req, &params);
    trace!("Proxying request to resolved target: {}", target_url);

    // Parse target URL
    let target_uri: Uri = match target_url.parse() {
        Ok(uri) => uri,
        Err(e) => {
            error!("Invalid target URL '{}': {}", target_url, e);
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Invalid proxy target"))
                .unwrap());
        }
    };

    // For base-URL targets, append the request path beyond the route's
    // literal prefix to the target's path.
    let append_path = if target.is_base_url() {
        Some(strip_route_prefix(req.uri().path(), route_prefix))
    } else {
        None
    };

    // Build the upstream URI, preserving query string from original request
    let upstream_uri = match build_upstream_uri(&target_uri, req.uri(), append_path) {
        Ok(uri) => uri,
        Err(e) => {
            error!("Failed to build upstream URI from '{}': {}", target_url, e);
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Invalid proxy target"))
                .unwrap());
        }
    };

    // Use shared forwarding logic
    forward_request(upstream_uri, req, &client, policy).await
}

/// Build the upstream URI from the target and original request.
///
/// When `append_path` is given (base-URL targets), it is joined onto the
/// target's path. The query string is the target's own, falling back to the
/// original request's.
fn build_upstream_uri(
    target: &Uri,
    original: &Uri,
    append_path: Option<&str>,
) -> Result<Uri, String> {
    let scheme = target.scheme_str().unwrap_or("http");
    let authority = target
        .authority()
        .map(|a| a.as_str())
        .ok_or_else(|| "target URL has no authority (host)".to_string())?;

    let path = match append_path {
        None | Some("") => target.path().to_string(),
        Some(rest) => {
            // `rest` starts with '/'; trim the target's trailing slash (and
            // treat a bare "/" as empty) so joining never doubles a slash.
            let base = target.path().trim_end_matches('/');
            format!("{base}{rest}")
        }
    };

    // Combine query strings: prefer target's query, fall back to original's
    let query = target.query().or_else(|| original.query());

    let path_and_query = match query {
        Some(q) => format!("{}?{}", path, q),
        None => path,
    };

    UriBuilder::new()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path_and_query)
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_request() -> Request<Body> {
        Request::builder().uri("/test").body(Body::empty()).unwrap()
    }

    #[test]
    fn test_static_string_resolver() {
        let resolver = "https://example.com".to_string();
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        assert_eq!(resolver.resolve(&req, &params), "https://example.com");
        assert!(resolver.is_base_url());
    }

    #[test]
    fn test_static_str_resolver() {
        let resolver: &'static str = "https://example.com";
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        assert_eq!(resolver.resolve(&req, &params), "https://example.com");
        assert!(resolver.is_base_url());
    }

    #[test]
    fn test_template_resolver_single_param() {
        let resolver = proxy_template("https://example.com/users/{id}");
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://example.com/users/123"
        );
        assert!(!resolver.is_base_url());
    }

    #[test]
    fn test_template_resolver_multiple_params() {
        let resolver = proxy_template("https://cdn.example.com/{id}/quality_{quality}");
        let req = dummy_request();
        let params = vec![
            ("id".to_string(), "video123".to_string()),
            ("quality".to_string(), "720p".to_string()),
        ];
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://cdn.example.com/video123/quality_720p"
        );
    }

    #[test]
    fn test_template_resolver_missing_param() {
        let resolver = proxy_template("https://example.com/{id}/{missing}");
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        // Missing params are left as-is (placeholder remains)
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://example.com/123/{missing}"
        );
    }

    #[test]
    fn test_template_resolver_no_params() {
        let resolver = proxy_template("https://example.com/static/path");
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://example.com/static/path"
        );
    }

    #[test]
    fn test_route_literal_prefix() {
        assert_eq!(route_literal_prefix("/api/{*rest}"), "/api");
        assert_eq!(route_literal_prefix("/users/{id}/profile"), "/users");
        assert_eq!(route_literal_prefix("/proxy"), "/proxy");
        assert_eq!(route_literal_prefix("/{*rest}"), "");
        assert_eq!(route_literal_prefix("/"), "");
    }

    #[test]
    fn test_build_upstream_uri_with_target_query() {
        let target: Uri = "https://example.com/path?foo=bar".parse().unwrap();
        let original: Uri = "/request?baz=qux".parse().unwrap();
        let result = build_upstream_uri(&target, &original, None).unwrap();
        // Target query takes precedence
        assert_eq!(result.to_string(), "https://example.com/path?foo=bar");
    }

    #[test]
    fn test_build_upstream_uri_with_original_query() {
        let target: Uri = "https://example.com/path".parse().unwrap();
        let original: Uri = "/request?baz=qux".parse().unwrap();
        let result = build_upstream_uri(&target, &original, None).unwrap();
        // Falls back to original query
        assert_eq!(result.to_string(), "https://example.com/path?baz=qux");
    }

    #[test]
    fn test_build_upstream_uri_no_query() {
        let target: Uri = "https://example.com/path".parse().unwrap();
        let original: Uri = "/request".parse().unwrap();
        let result = build_upstream_uri(&target, &original, None).unwrap();
        assert_eq!(result.to_string(), "https://example.com/path");
    }

    #[test]
    fn test_build_upstream_uri_appends_remaining_path() {
        let target: Uri = "https://example.com".parse().unwrap();
        let original: Uri = "/api/foo/bar?x=1".parse().unwrap();
        let result = build_upstream_uri(&target, &original, Some("/foo/bar")).unwrap();
        assert_eq!(result.to_string(), "https://example.com/foo/bar?x=1");

        let target: Uri = "https://example.com/v2/".parse().unwrap();
        let result = build_upstream_uri(&target, &original, Some("/foo/bar")).unwrap();
        assert_eq!(result.to_string(), "https://example.com/v2/foo/bar?x=1");
    }

    #[test]
    fn test_build_upstream_uri_rejects_missing_authority() {
        let target: Uri = "/just-a-path".parse().unwrap();
        let original: Uri = "/request".parse().unwrap();
        assert!(build_upstream_uri(&target, &original, None).is_err());
    }

    #[test]
    fn test_strip_route_prefix() {
        assert_eq!(strip_route_prefix("/api/foo", "/api"), "/foo");
        assert_eq!(strip_route_prefix("/api", "/api"), "");
        assert_eq!(strip_route_prefix("/apifoo", "/api"), "/apifoo");
        assert_eq!(strip_route_prefix("/other", "/api"), "/other");
        assert_eq!(strip_route_prefix("/anything", ""), "/anything");
    }

    #[test]
    fn test_template_encodes_at_sign_prevents_ssrf() {
        // A template like "http://my-app-{env}/api" with env="@attacker.com"
        // must NOT resolve to "http://my-app-@attacker.com/api" (host=attacker.com).
        // The @ must be percent-encoded to prevent authority rewriting.
        let resolver = proxy_template("http://my-app-{env}/api");
        let req = dummy_request();
        let params = vec![("env".to_string(), "@attacker.com".to_string())];
        let resolved = resolver.resolve(&req, &params);
        assert_eq!(resolved, "http://my-app-%40attacker.com/api");

        // The encoded URL will fail URI parsing (InvalidAuthority), which means
        // the proxy returns a 500 instead of making the SSRF request. This is
        // the desired security outcome — the attack is fully prevented.
        assert!(resolved.parse::<Uri>().is_err());

        // Verify the unencoded version WOULD have parsed with the attacker as host
        let malicious = "http://my-app-@attacker.com/api";
        let malicious_uri: Uri = malicious.parse().unwrap();
        assert_eq!(malicious_uri.host(), Some("attacker.com"));
    }

    #[test]
    fn test_template_encodes_fragment_injection() {
        let resolver = proxy_template("https://backend.com/path/{id}/data");
        let req = dummy_request();
        let params = vec![("id".to_string(), "x#evil".to_string())];
        let resolved = resolver.resolve(&req, &params);
        assert_eq!(resolved, "https://backend.com/path/x%23evil/data");
    }

    #[test]
    fn test_template_encodes_query_injection() {
        let resolver = proxy_template("https://backend.com/path/{id}");
        let req = dummy_request();
        let params = vec![("id".to_string(), "x?admin=true".to_string())];
        let resolved = resolver.resolve(&req, &params);
        assert_eq!(resolved, "https://backend.com/path/x%3Fadmin=true");
    }

    #[test]
    fn test_template_preserves_slashes_for_catch_all() {
        // Slashes are intentionally NOT encoded to support {*rest} catch-all routes
        let resolver = proxy_template("https://backend.com/api/{rest}");
        let req = dummy_request();
        let params = vec![("rest".to_string(), "foo/bar/baz".to_string())];
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://backend.com/api/foo/bar/baz"
        );
    }

    #[test]
    fn test_template_safe_values_unchanged() {
        // Normal alphanumeric values, hyphens, dots, underscores pass through unmodified
        let resolver = proxy_template("https://{service}.example.com/{id}");
        let req = dummy_request();
        let params = vec![
            ("service".to_string(), "my-app_v2.1".to_string()),
            ("id".to_string(), "abc-123".to_string()),
        ];
        assert_eq!(
            resolver.resolve(&req, &params),
            "https://my-app_v2.1.example.com/abc-123"
        );
    }

    mod properties {
        use super::super::{build_upstream_uri, strip_route_prefix};
        use axum::http::Uri;
        use proptest::prelude::*;

        proptest! {
            /// Joining a base target with any remaining path must preserve
            /// scheme/authority and never panic or double slashes.
            #[test]
            fn build_upstream_uri_joins_cleanly(
                target_path in prop_oneof![
                    Just("".to_string()),
                    Just("/".to_string()),
                    Just("/v2".to_string()),
                    Just("/v2/".to_string()),
                ],
                segs in proptest::collection::vec("[a-zA-Z0-9._~-]{1,8}", 0..4),
            ) {
                let target: Uri = format!("https://backend{target_path}").parse().unwrap();
                let original: Uri = "/req".parse().unwrap();
                let rest: String = segs.iter().map(|s| format!("/{s}")).collect();
                let append = if rest.is_empty() { None } else { Some(rest.as_str()) };

                let uri = build_upstream_uri(&target, &original, append).unwrap();
                prop_assert_eq!(uri.scheme_str(), Some("https"));
                prop_assert_eq!(uri.authority().map(|a| a.as_str()), Some("backend"));
                prop_assert!(!uri.path().contains("//"), "path was {:?}", uri.path());
                if let Some(rest) = append {
                    prop_assert!(uri.path().ends_with(rest));
                }
            }

            /// Prefix stripping only happens at segment boundaries.
            #[test]
            fn strip_route_prefix_respects_boundaries(suffix in "[a-zA-Z0-9]{1,8}") {
                let joined = format!("/api{suffix}");
                prop_assert_eq!(strip_route_prefix(&joined, "/api"), joined.as_str());
                let nested = format!("/api/{suffix}");
                let expected = format!("/{suffix}");
                prop_assert_eq!(strip_route_prefix(&nested, "/api"), expected.as_str());
            }
        }
    }
}
