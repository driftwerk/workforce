//! Trusted proxy support for extracting real client IPs.
//!
//! When the hub is behind a reverse proxy (nginx, Caddy, Traefik), the client IP
//! seen by axum is the proxy's IP, not the real client. This module extracts the
//! real client IP from the X-Forwarded-For header, but only when the request
//! comes from a trusted proxy IP to prevent IP spoofing.
//!
//! Trusted proxies can be specified as single IPs or CIDR blocks (e.g. `10.0.0.0/8`).

use std::net::{IpAddr, SocketAddr};

use axum::extract::ConnectInfo;
use ipnetwork::IpNetwork;

/// Configuration for trusted proxy handling.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxyConfig {
    /// List of trusted proxy networks. Supports single IPs and CIDR blocks.
    pub trusted_networks: Vec<IpNetwork>,
}

impl TrustedProxyConfig {
    pub fn new(networks: Vec<IpNetwork>) -> Self {
        Self {
            trusted_networks: networks,
        }
    }

    /// Create a config from string addresses (single IPs or CIDR notation).
    ///
    /// Single IPs like "10.0.0.1" are treated as /32 (IPv4) or /128 (IPv6).
    /// CIDR blocks like "10.0.0.0/8" match any IP in that range.
    pub fn from_strings(addrs: &[String]) -> eyre::Result<Self> {
        let networks = addrs
            .iter()
            .map(|s| {
                s.parse::<IpNetwork>()
                    .map_err(|e| eyre::eyre!("Invalid trusted proxy address '{}': {}", s, e))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        Ok(Self {
            trusted_networks: networks,
        })
    }

    /// Check if a given IP is a trusted proxy (exact match or within a CIDR block).
    pub fn is_trusted(&self, ip: &IpAddr) -> bool {
        self.trusted_networks.iter().any(|net| net.contains(*ip))
    }
}

/// Extract the real client IP from a request, respecting trusted proxy configuration.
///
/// If the direct connection comes from a trusted proxy, extracts the client IP from
/// the X-Forwarded-For header (taking the leftmost/first entry). Otherwise, returns
/// the direct connection IP.
///
/// The X-Forwarded-For header format is: `client, proxy1, proxy2`
/// We take the first (leftmost) IP which is the original client.
pub fn extract_client_ip(
    connect_info: &Option<ConnectInfo<SocketAddr>>,
    xff_header: Option<&str>,
    config: &TrustedProxyConfig,
) -> IpAddr {
    let direct_ip = connect_info
        .as_ref()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));

    // If no trusted proxies configured, always use direct IP
    if config.trusted_networks.is_empty() {
        return direct_ip;
    }

    // Only trust X-Forwarded-For if the direct connection is from a trusted proxy
    if !config.is_trusted(&direct_ip) {
        return direct_ip;
    }

    // Parse X-Forwarded-For header
    if let Some(xff) = xff_header {
        // Take the first (leftmost) IP — this is the original client
        if let Some(client_ip_str) = xff.split(',').next() {
            if let Ok(ip) = client_ip_str.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }

    direct_ip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_config() -> TrustedProxyConfig {
        TrustedProxyConfig::new(vec!["10.0.0.1/32".parse().unwrap()])
    }

    fn cidr_config() -> TrustedProxyConfig {
        TrustedProxyConfig::new(vec!["10.0.0.0/8".parse().unwrap()])
    }

    fn connect_info(ip: &str) -> Option<ConnectInfo<SocketAddr>> {
        Some(ConnectInfo(format!("{ip}:1234").parse().unwrap()))
    }

    #[test]
    fn no_proxy_config_uses_direct_ip() {
        let config = TrustedProxyConfig::default();
        let info = connect_info("192.168.1.100");
        let ip = extract_client_ip(&info, Some("1.2.3.4"), &config);
        assert_eq!(ip, "192.168.1.100".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_proxy_extracts_xff() {
        let config = proxy_config();
        let info = connect_info("10.0.0.1"); // from trusted proxy
        let ip = extract_client_ip(&info, Some("203.0.113.50, 10.0.0.1"), &config);
        assert_eq!(ip, "203.0.113.50".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn untrusted_proxy_ignores_xff() {
        let config = proxy_config();
        let info = connect_info("192.168.1.100"); // NOT trusted
        let ip = extract_client_ip(&info, Some("1.2.3.4"), &config);
        assert_eq!(ip, "192.168.1.100".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn single_ip_in_xff() {
        let config = proxy_config();
        let info = connect_info("10.0.0.1");
        let ip = extract_client_ip(&info, Some("8.8.8.8"), &config);
        assert_eq!(ip, "8.8.8.8".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn no_xff_header_uses_direct() {
        let config = proxy_config();
        let info = connect_info("10.0.0.1");
        let ip = extract_client_ip(&info, None, &config);
        assert_eq!(ip, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn invalid_xff_uses_direct() {
        let config = proxy_config();
        let info = connect_info("10.0.0.1");
        let ip = extract_client_ip(&info, Some("not-an-ip"), &config);
        assert_eq!(ip, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn is_trusted_works() {
        let config = proxy_config();
        assert!(config.is_trusted(&"10.0.0.1".parse().unwrap()));
        assert!(!config.is_trusted(&"10.0.0.2".parse().unwrap()));
    }

    // CIDR-specific tests

    #[test]
    fn cidr_matches_range() {
        let config = cidr_config();
        assert!(config.is_trusted(&"10.0.0.1".parse().unwrap()));
        assert!(config.is_trusted(&"10.1.2.3".parse().unwrap()));
        assert!(config.is_trusted(&"10.255.255.255".parse().unwrap()));
        assert!(!config.is_trusted(&"11.0.0.1".parse().unwrap()));
        assert!(!config.is_trusted(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn cidr_proxy_extracts_xff() {
        let config = cidr_config();
        let info = connect_info("10.42.0.5"); // within 10.0.0.0/8
        let ip = extract_client_ip(&info, Some("203.0.113.50, 10.42.0.5"), &config);
        assert_eq!(ip, "203.0.113.50".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn from_strings_single_ip() {
        let config = TrustedProxyConfig::from_strings(&["10.0.0.1".to_string()]).unwrap();
        assert!(config.is_trusted(&"10.0.0.1".parse().unwrap()));
        assert!(!config.is_trusted(&"10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn from_strings_cidr() {
        let config = TrustedProxyConfig::from_strings(&["172.16.0.0/12".to_string()]).unwrap();
        assert!(config.is_trusted(&"172.16.0.1".parse().unwrap()));
        assert!(config.is_trusted(&"172.31.255.255".parse().unwrap()));
        assert!(!config.is_trusted(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn from_strings_mixed() {
        let config = TrustedProxyConfig::from_strings(&[
            "10.0.0.0/8".to_string(),
            "192.168.1.1".to_string(),
        ])
        .unwrap();
        assert!(config.is_trusted(&"10.1.2.3".parse().unwrap()));
        assert!(config.is_trusted(&"192.168.1.1".parse().unwrap()));
        assert!(!config.is_trusted(&"192.168.1.2".parse().unwrap()));
    }

    #[test]
    fn from_strings_invalid() {
        let result = TrustedProxyConfig::from_strings(&["not-an-ip".to_string()]);
        assert!(result.is_err());
    }
}
