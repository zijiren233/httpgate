use std::sync::Arc;

use async_trait::async_trait;
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use regex::Regex;
use tracing::{debug, info, warn};

use crate::registry::DevboxRegistry;

/// Regex to parse host header: <uniqueID>-<port>.devbox.xxx
///
/// Pattern: ^(<uniqueID>)-(<port>)\.
/// - uniqueID: lowercase alphanumeric with hyphens, cannot start/end with hyphen
/// - port: numeric
///
/// Examples:
///   - "outdoor-before-78648-8080.devbox.xxx" -> ("outdoor-before-78648", 8080)
///   - "my-app-8080.devbox.xxx" -> ("my-app", 8080)
static HOST_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    // [a-z\d] - starts with alphanumeric
    // (?:[-a-z\d]*[a-z\d])? - optionally more chars, must end with alphanumeric
    // -(\d+)\. - port suffix
    Regex::new(r"^([a-z\d](?:[-a-z\d]*[a-z\d])?)-(\d+)\.").unwrap()
});

/// Context passed between proxy request phases
pub struct ProxyCtx {
    /// Backend hostname (e.g., "outdoor-before-78648.ns-admin.svc.cluster.local")
    pub backend_host: String,
    /// Backend port
    pub backend_port: u16,
}

/// Pingora-based HTTP proxy for routing requests to devbox services.
///
/// Routes requests based on the Host header pattern:
/// `<uniqueID>-<port>.devbox.xxx` -> `<uniqueID>.<namespace>.svc.cluster.local:<port>`
pub struct DevboxProxy {
    registry: Arc<DevboxRegistry>,
    #[allow(dead_code)]
    domain_suffix: String,
}

impl DevboxProxy {
    pub const fn new(registry: Arc<DevboxRegistry>, domain_suffix: String) -> Self {
        Self {
            registry,
            domain_suffix,
        }
    }

    /// Parse the Host header to extract uniqueID and port.
    ///
    /// Expected format: `<uniqueID>-<port>.devbox.xxx[:port]`
    /// Example: `outdoor-before-78648-8080.devbox.sealos.io`
    fn parse_host(host: &str) -> Option<(String, u16)> {
        // Remove port suffix if present (e.g., "xxx:443" -> "xxx")
        let host_without_port = host.split(':').next().unwrap_or(host);

        HOST_REGEX.captures(host_without_port).and_then(|caps| {
            let unique_id = caps.get(1)?.as_str().to_string();
            let port: u16 = caps.get(2)?.as_str().parse().ok()?;
            Some((unique_id, port))
        })
    }

    /// Resolve the backend address from uniqueID.
    ///
    /// Returns the backend hostname and port if the uniqueID is registered.
    fn resolve_backend(&self, unique_id: &str, port: u16) -> Option<(String, u16)> {
        let info = self.registry.get(unique_id)?;

        // Format: <uniqueID>.<namespace>.svc.cluster.local
        let backend_host = format!("{}.{}.svc.cluster.local", unique_id, info.namespace);

        debug!(
            unique_id = %unique_id,
            namespace = %info.namespace,
            backend = %backend_host,
            port = port,
            "Resolved backend"
        );

        Some((backend_host, port))
    }

    /// Send a 404 Not Found response
    async fn send_not_found(session: &mut Session) -> Result<bool> {
        let header = ResponseHeader::build(404, None)?;
        session
            .write_response_header(Box::new(header), true)
            .await?;
        Ok(true)
    }

    /// Send a 502 Bad Gateway response
    #[allow(dead_code)]
    async fn send_bad_gateway(session: &mut Session) -> Result<bool> {
        let header = ResponseHeader::build(502, None)?;
        session
            .write_response_header(Box::new(header), true)
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl ProxyHttp for DevboxProxy {
    type CTX = Option<ProxyCtx>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // Extract Host header
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        // Parse uniqueID and port from host
        let Some((unique_id, port)) = Self::parse_host(host) else {
            warn!(host = %host, "Failed to parse host header");
            return Self::send_not_found(session).await;
        };

        // Resolve backend from registry
        let Some((backend_host, backend_port)) = self.resolve_backend(&unique_id, port) else {
            warn!(
                host = %host,
                unique_id = %unique_id,
                "No backend found for uniqueID"
            );
            return Self::send_not_found(session).await;
        };

        info!(
            host = %host,
            backend = %format!("{}:{}", backend_host, backend_port),
            "Routing request"
        );

        *ctx = Some(ProxyCtx {
            backend_host,
            backend_port,
        });

        Ok(false) // Continue to upstream
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let ctx = ctx
            .as_ref()
            .expect("Context should be set in request_filter");

        let peer = HttpPeer::new(
            (ctx.backend_host.as_str(), ctx.backend_port),
            false, // No TLS to backend (internal cluster traffic)
            ctx.backend_host.clone(),
        );

        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        _upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Add standard proxy headers
        // upstream_request
        //     .insert_header("X-Forwarded-Proto", "https")
        //     .unwrap();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host_standard_format() {
        // Standard format: word-word-number
        let result = DevboxProxy::parse_host("outdoor-before-78648-8080.devbox.sealos.io");
        assert_eq!(result, Some(("outdoor-before-78648".to_string(), 8080)));
    }

    #[test]
    fn test_parse_host_simple_id() {
        // Simple uniqueID without numbers
        let result = DevboxProxy::parse_host("my-app-8080.devbox.sealos.io");
        assert_eq!(result, Some(("my-app".to_string(), 8080)));
    }

    #[test]
    fn test_parse_host_single_word() {
        // Single word uniqueID
        let result = DevboxProxy::parse_host("myapp-443.devbox.sealos.io");
        assert_eq!(result, Some(("myapp".to_string(), 443)));
    }

    #[test]
    fn test_parse_host_with_numbers() {
        // uniqueID containing numbers
        let result = DevboxProxy::parse_host("app123-test456-3000.devbox.sealos.io");
        assert_eq!(result, Some(("app123-test456".to_string(), 3000)));
    }

    #[test]
    fn test_parse_host_multiple_hyphens() {
        // Multiple hyphens in uniqueID
        let result = DevboxProxy::parse_host("my-cool-dev-box-1-8080.devbox.sealos.io");
        assert_eq!(result, Some(("my-cool-dev-box-1".to_string(), 8080)));
    }

    #[test]
    fn test_parse_host_single_char() {
        // Single character uniqueID
        let result = DevboxProxy::parse_host("a-8080.devbox.sealos.io");
        assert_eq!(result, Some(("a".to_string(), 8080)));
    }

    #[test]
    fn test_parse_host_with_port_suffix() {
        // Host header with :443 suffix (TLS)
        let result = DevboxProxy::parse_host("outdoor-before-78648-8080.devbox.sealos.io:443");
        assert_eq!(result, Some(("outdoor-before-78648".to_string(), 8080)));
    }

    #[test]
    fn test_parse_host_invalid_no_port() {
        // Missing port
        assert!(DevboxProxy::parse_host("outdoor-before.devbox.sealos.io").is_none());
    }

    #[test]
    fn test_parse_host_invalid_format() {
        // Invalid formats
        assert!(DevboxProxy::parse_host("invalid.example.com").is_none());
        assert!(DevboxProxy::parse_host("").is_none());
        assert!(DevboxProxy::parse_host("-invalid-8080.devbox.io").is_none()); // starts with hyphen
        assert!(DevboxProxy::parse_host("invalid--8080.devbox.io").is_none()); // ends with hyphen
    }

    #[test]
    fn test_resolve_backend() {
        let registry = Arc::new(DevboxRegistry::new());
        registry.register("outdoor-before-78648".to_string(), "ns-admin".to_string());

        let proxy = DevboxProxy::new(registry, "devbox.sealos.io".to_string());

        let result = proxy.resolve_backend("outdoor-before-78648", 8080);
        assert_eq!(
            result,
            Some((
                "outdoor-before-78648.ns-admin.svc.cluster.local".to_string(),
                8080
            ))
        );
    }

    #[test]
    fn test_resolve_backend_not_found() {
        let registry = Arc::new(DevboxRegistry::new());
        let proxy = DevboxProxy::new(registry, "devbox.sealos.io".to_string());

        let result = proxy.resolve_backend("unknown-id-123", 8080);
        assert!(result.is_none());
    }
}
