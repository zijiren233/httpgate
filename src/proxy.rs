use std::sync::Arc;

use async_trait::async_trait;
use pingora_core::{upstreams::peer::HttpPeer, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use regex::Regex;
use tracing::{debug, info, warn};

use crate::registry::DevboxRegistry;

/// Result of backend resolution
enum BackendResult {
    /// Backend resolved successfully with Pod IP
    Ok(String, u16),
    /// Devbox not registered (uniqueID not found)
    NotFound,
    /// Devbox registered but Pod is not running (no Pod IP)
    NotRunning,
}

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
    /// Backend Pod IP address
    pub backend_ip: String,
    /// Backend port
    pub backend_port: u16,
}

/// Pingora-based HTTP proxy for routing requests to devbox pods.
///
/// Routes requests based on the Host header pattern:
/// `<uniqueID>-<port>.devbox.xxx` -> `<pod_ip>:<port>`
pub struct DevboxProxy {
    registry: Arc<DevboxRegistry>,
}

impl DevboxProxy {
    pub const fn new(registry: Arc<DevboxRegistry>) -> Self {
        Self { registry }
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
    /// Performs a two-step lookup:
    /// 1. uniqueID -> DevboxInfo (namespace, devbox_name)
    /// 2. namespace/devbox_name -> pod_ip
    ///
    /// Returns:
    /// - `BackendResult::Ok` if uniqueID is registered and Pod IP is available
    /// - `BackendResult::NotFound` if uniqueID is not registered
    /// - `BackendResult::NotRunning` if uniqueID is registered but Pod IP is not available
    fn resolve_backend(&self, unique_id: &str, port: u16) -> BackendResult {
        // Step 1: Look up devbox info
        let Some(info) = self.registry.get_devbox(unique_id) else {
            return BackendResult::NotFound;
        };

        // Step 2: Look up pod IP
        let Some(pod_ip) = self.registry.get_pod_ip(&info.namespace, &info.devbox_name) else {
            return BackendResult::NotRunning;
        };

        debug!(
            unique_id = %unique_id,
            namespace = %info.namespace,
            devbox_name = %info.devbox_name,
            pod_ip = %pod_ip,
            port = port,
            "Resolved backend"
        );

        BackendResult::Ok(pod_ip, port)
    }

    /// Send a 404 Not Found response
    async fn send_not_found(session: &mut Session) -> Result<bool> {
        let header = ResponseHeader::build(404, None)?;
        session
            .write_response_header(Box::new(header), true)
            .await?;
        Ok(true)
    }

    /// Send a 503 Service Unavailable response (devbox not running)
    async fn send_service_unavailable(session: &mut Session) -> Result<bool> {
        let header = ResponseHeader::build(503, None)?;
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
        let (backend_ip, backend_port) = match self.resolve_backend(&unique_id, port) {
            BackendResult::Ok(ip, port) => (ip, port),
            BackendResult::NotFound => {
                warn!(
                    host = %host,
                    unique_id = %unique_id,
                    "Devbox not found"
                );
                return Self::send_not_found(session).await;
            }
            BackendResult::NotRunning => {
                warn!(
                    host = %host,
                    unique_id = %unique_id,
                    "Devbox not running (no Pod IP)"
                );
                return Self::send_service_unavailable(session).await;
            }
        };

        info!(
            host = %host,
            backend = %format!("{}:{}", backend_ip, backend_port),
            "Routing request"
        );

        *ctx = Some(ProxyCtx {
            backend_ip,
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
            (ctx.backend_ip.as_str(), ctx.backend_port),
            false, // No TLS to backend (internal cluster traffic)
            ctx.backend_ip.clone(),
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
    fn test_resolve_backend_with_pod_ip() {
        let registry = Arc::new(DevboxRegistry::new());
        registry.register_devbox(
            "outdoor-before-78648".to_string(),
            "ns-admin".to_string(),
            "devbox1".to_string(),
        );
        registry.update_pod_ip("ns-admin", "devbox1", "10.107.173.213".to_string());

        let proxy = DevboxProxy::new(registry);

        let result = proxy.resolve_backend("outdoor-before-78648", 8080);
        assert!(matches!(
            result,
            BackendResult::Ok(ip, 8080) if ip == "10.107.173.213"
        ));
    }

    #[test]
    fn test_resolve_backend_no_pod_ip() {
        let registry = Arc::new(DevboxRegistry::new());
        registry.register_devbox(
            "outdoor-before-78648".to_string(),
            "ns-admin".to_string(),
            "devbox1".to_string(),
        );
        // Pod IP not set

        let proxy = DevboxProxy::new(registry);

        let result = proxy.resolve_backend("outdoor-before-78648", 8080);
        assert!(matches!(result, BackendResult::NotRunning));
    }

    #[test]
    fn test_resolve_backend_not_found() {
        let registry = Arc::new(DevboxRegistry::new());
        let proxy = DevboxProxy::new(registry);

        let result = proxy.resolve_backend("unknown-id-123", 8080);
        assert!(matches!(result, BackendResult::NotFound));
    }
}
