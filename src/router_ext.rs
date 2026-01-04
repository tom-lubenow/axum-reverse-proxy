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
//!     // Static target
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
use std::convert::Infallible;
use tracing::{error, trace};

use crate::forward::{ProxyClient, create_proxy_client, forward_request};

/// A trait for resolving the target URL for a proxy request.
///
/// Implement this trait to provide custom target URL resolution logic.
/// The resolver receives the full request and path parameters, allowing
/// routing decisions based on headers, method, query parameters, etc.
///
/// # Built-in Implementations
///
/// - `String` and `&'static str`: Static target URLs (request/parameters are ignored)
/// - [`TemplateTarget`]: Template-based URL with `{param}` substitution
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
}

impl TargetResolver for String {
    fn resolve(&self, _req: &Request<Body>, _params: &[(String, String)]) -> String {
        self.clone()
    }
}

impl TargetResolver for &'static str {
    fn resolve(&self, _req: &Request<Body>, _params: &[(String, String)]) -> String {
        (*self).to_string()
    }
}

/// A template-based target resolver that substitutes path parameters into a URL template.
///
/// Template placeholders use the format `{param_name}` and are replaced with the
/// corresponding path parameter values from the request.
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
            result = result.replace(&placeholder, value);
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
    /// - A static string (`&str` or `String`)
    /// - A [`TemplateTarget`] for dynamic URL generation based on path parameters
    /// - Any custom type implementing [`TargetResolver`]
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
    ///     // Static proxy
    ///     .proxy_route("/api/{*rest}", "https://api.example.com")
    ///     // Dynamic proxy with path substitution
    ///     .proxy_route("/users/{id}", proxy_template("https://users.example.com/{id}"));
    /// ```
    fn proxy_route<T: TargetResolver>(self, path: &str, target: T) -> Self;
}

impl<S> ProxyRouterExt<S> for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn proxy_route<T: TargetResolver>(self, path: &str, target: T) -> Self {
        let client = create_proxy_client();

        self.route(
            path,
            any(
                move |Path(params): Path<Vec<(String, String)>>, req: Request<Body>| {
                    let target = target.clone();
                    let client = client.clone();
                    async move { proxy_request(target, params, req, client).await }
                },
            ),
        )
    }
}

async fn proxy_request<T: TargetResolver>(
    target: T,
    params: Vec<(String, String)>,
    req: Request<Body>,
    client: ProxyClient,
) -> Result<Response<Body>, Infallible> {
    let target_url = target.resolve(&req, &params);
    trace!("Proxying request to resolved target: {}", target_url);

    // Parse target URL
    let target_uri: Uri = match target_url.parse() {
        Ok(uri) => uri,
        Err(e) => {
            error!("Invalid target URL '{}': {}", target_url, e);
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Invalid target URL: {e}")))
                .unwrap());
        }
    };

    // Build the upstream URI, preserving query string from original request
    let upstream_uri = build_upstream_uri(&target_uri, req.uri());

    // Use shared forwarding logic
    forward_request(upstream_uri, req, &client).await
}

/// Build the upstream URI from the target and original request.
///
/// If the original request has a query string and the target doesn't,
/// the query string is appended to the target.
fn build_upstream_uri(target: &Uri, original: &Uri) -> Uri {
    let scheme = target.scheme_str().unwrap_or("http");
    let authority = target
        .authority()
        .map(|a| a.as_str())
        .unwrap_or("localhost");
    let path = target.path();

    // Combine query strings: prefer target's query, fall back to original's
    let query = target.query().or_else(|| original.query());

    let path_and_query = match query {
        Some(q) => format!("{}?{}", path, q),
        None => path.to_string(),
    };

    UriBuilder::new()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path_and_query)
        .build()
        .expect("Failed to build upstream URI")
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
    }

    #[test]
    fn test_static_str_resolver() {
        let resolver: &'static str = "https://example.com";
        let req = dummy_request();
        let params = vec![("id".to_string(), "123".to_string())];
        assert_eq!(resolver.resolve(&req, &params), "https://example.com");
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
    fn test_build_upstream_uri_with_target_query() {
        let target: Uri = "https://example.com/path?foo=bar".parse().unwrap();
        let original: Uri = "/request?baz=qux".parse().unwrap();
        let result = build_upstream_uri(&target, &original);
        // Target query takes precedence
        assert_eq!(result.to_string(), "https://example.com/path?foo=bar");
    }

    #[test]
    fn test_build_upstream_uri_with_original_query() {
        let target: Uri = "https://example.com/path".parse().unwrap();
        let original: Uri = "/request?baz=qux".parse().unwrap();
        let result = build_upstream_uri(&target, &original);
        // Falls back to original query
        assert_eq!(result.to_string(), "https://example.com/path?baz=qux");
    }

    #[test]
    fn test_build_upstream_uri_no_query() {
        let target: Uri = "https://example.com/path".parse().unwrap();
        let original: Uri = "/request".parse().unwrap();
        let result = build_upstream_uri(&target, &original);
        assert_eq!(result.to_string(), "https://example.com/path");
    }
}
