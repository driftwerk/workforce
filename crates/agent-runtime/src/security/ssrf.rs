//! SSRF (Server-Side Request Forgery) protection for web tools.
//!
//! Blocks requests to private/internal network addresses to prevent agents
//! from accessing internal services (NATS, Weaviate, metadata endpoints, etc.).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

/// User-agent every agent-initiated outbound request identifies itself with.
pub const BOT_USER_AGENT: &str = "Workforce/1.0 (bot)";

/// Maximum redirect hops followed when redirects are enabled.
const MAX_REDIRECTS: usize = 10;

/// Build an HTTP client whose every redirect hop is re-checked against
/// [`check_url_for_ssrf`], so an open redirect can't be used to reach an
/// internal service. The one such builder in the tree: `web.*` and `http.*`
/// share it so the SSRF policy cannot drift between them.
///
/// `follow_redirects` is `false` for a request carrying credentials — `reqwest`
/// does not strip a manually-set `Authorization` header across a cross-host
/// redirect, so following one would hand the credential to whatever host the
/// target names.
pub fn ssrf_safe_client(timeout: Duration, follow_redirects: bool) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BOT_USER_AGENT));

    let redirect_policy = if follow_redirects {
        reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if let Err(reason) = check_url_for_ssrf(attempt.url()) {
                attempt.error(format!("Redirect blocked: {reason}"))
            } else {
                attempt.follow()
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };

    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(timeout)
        .redirect(redirect_policy)
        .build()
        .expect("failed to build HTTP client")
}

/// Check if a URL's resolved IP address is in a private/internal range.
/// Returns an error message if the IP is blocked, Ok(()) if it's safe.
pub fn check_url_for_ssrf(url: &url::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Try to resolve the hostname to IP addresses
    let addrs = resolve_host(host, url.port_or_known_default().unwrap_or(80))?;

    for addr in &addrs {
        if is_private_ip(addr) {
            return Err(format!(
                "Blocked: URL resolves to private/internal address {addr}. \
                 Requests to internal networks are not allowed."
            ));
        }
    }

    Ok(())
}

/// Resolve a hostname to IP addresses.
fn resolve_host(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    // First check if it's already an IP address
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    // DNS resolution
    let socket_addrs: Vec<_> = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve hostname '{host}': {e}"))?
        .collect();

    if socket_addrs.is_empty() {
        return Err(format!(
            "Hostname '{host}' did not resolve to any addresses"
        ));
    }

    Ok(socket_addrs.into_iter().map(|sa| sa.ip()).collect())
}

/// Check if an IP address is in a private, reserved, or internal range.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_private_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_private_ipv6(ipv6),
    }
}

fn is_private_ipv4(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();

    // Loopback: 127.0.0.0/8
    if octets[0] == 127 {
        return true;
    }

    // Private RFC1918: 10.0.0.0/8
    if octets[0] == 10 {
        return true;
    }

    // Private RFC1918: 172.16.0.0/12
    if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        return true;
    }

    // Private RFC1918: 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        return true;
    }

    // Link-local: 169.254.0.0/16 (includes cloud metadata at 169.254.169.254)
    if octets[0] == 169 && octets[1] == 254 {
        return true;
    }

    // CGNAT: 100.64.0.0/10
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return true;
    }

    // Unspecified: 0.0.0.0
    if ip.is_unspecified() {
        return true;
    }

    // Broadcast: 255.255.255.255
    if ip.is_broadcast() {
        return true;
    }

    // Multicast: 224.0.0.0/4
    if octets[0] >= 224 && octets[0] <= 239 {
        return true;
    }

    // Reserved for future use: 240.0.0.0/4
    if octets[0] >= 240 {
        return true;
    }

    // Benchmarking: 198.18.0.0/15 (RFC 2544)
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return true;
    }

    // Protocol assignments: 192.0.0.0/24
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        return true;
    }

    false
}

fn is_private_ipv6(ip: &Ipv6Addr) -> bool {
    // Loopback: ::1
    if ip.is_loopback() {
        return true;
    }

    // Unspecified: ::
    if ip.is_unspecified() {
        return true;
    }

    // IPv4-mapped IPv6: ::ffff:x.x.x.x
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_private_ipv4(&ipv4);
    }

    let segments = ip.segments();

    // Unique Local Address (ULA): fc00::/7
    if segments[0] & 0xfe00 == 0xfc00 {
        return true;
    }

    // Link-local: fe80::/10
    if segments[0] & 0xffc0 == 0xfe80 {
        return true;
    }

    // 6to4: 2002::/16 — check if embedded IPv4 is private
    if segments[0] == 0x2002 {
        let embedded_ipv4 = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        if is_private_ipv4(&embedded_ipv4) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_v4() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 1, 2, 3))));
    }

    #[test]
    fn blocks_rfc1918() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 1, 20))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 31, 255, 255))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn blocks_link_local() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));
    }

    #[test]
    fn blocks_cgnat() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
    }

    #[test]
    fn allows_public_ips() {
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }

    #[test]
    fn blocks_loopback_v6() {
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn blocks_unspecified() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn blocks_ula_v6() {
        let ip: Ipv6Addr = "fd00::1".parse().unwrap();
        assert!(is_private_ip(&IpAddr::V6(ip)));
    }

    #[test]
    fn blocks_link_local_v6() {
        let ip: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(is_private_ip(&IpAddr::V6(ip)));
    }

    #[test]
    fn check_url_blocks_private() {
        let url = url::Url::parse("http://127.0.0.1:8222/healthz").unwrap();
        assert!(check_url_for_ssrf(&url).is_err());

        let url = url::Url::parse("http://10.20.0.5:4222").unwrap();
        assert!(check_url_for_ssrf(&url).is_err());

        let url = url::Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(check_url_for_ssrf(&url).is_err());
    }

    #[test]
    fn check_url_allows_public() {
        let url = url::Url::parse("https://example.com").unwrap();
        assert!(check_url_for_ssrf(&url).is_ok());
    }
}
