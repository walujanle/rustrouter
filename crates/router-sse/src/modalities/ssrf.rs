//! The SSRF guard.
//!
//! Three layers, each closing a distinct bypass class. This is a security
//! boundary; nothing here is optional and none of it may be simplified away:
//!
//! 1. [`assert_public_url`] — synchronous literal host/IP checks.
//! 2. [`assert_public_url_resolved`] — adds DNS resolution, so a hostname that
//!    merely *resolves* to a private address (a `nip.io` wildcard domain, or an
//!    attacker domain pointed at 127.0.0.1) is rejected too.
//! 3. [`fetch_public`] — manual redirect handling, so a validated public URL
//!    cannot 30x its way to an internal target without each hop being
//!    re-validated through layer 2.
//!
//! Two matching bugs are preserved as fixes, not reintroduced: a trailing dot
//! is normalized off the host (`localhost.` is `localhost`), and IPv6 is parsed
//! into 8 numeric groups once so `::ffff:127.0.0.1` and `::ffff:7f00:1` compare
//! equal instead of only one textual form matching.

use std::net::{IpAddr, SocketAddr};

use crate::executors::http::{ProxyOptions, prepare_send_with_redirect_policy};

/// `BLOCKED_HOSTNAMES`.
const BLOCKED_HOSTNAMES: [&str; 3] = ["localhost", "ip6-localhost", "ip6-loopback"];
/// `BLOCKED_SUFFIXES`.
const BLOCKED_SUFFIXES: [&str; 3] = [".internal", ".local", ".localhost"];

/// An SSRF rejection. The caller maps it to a 400.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Blocked URL: {0}")]
pub struct BlockedUrl(pub String);

impl BlockedUrl {
    fn internal_host() -> Self {
        Self("internal host".to_string())
    }
    fn resolved_internal_host() -> Self {
        Self("hostname resolves to an internal host".to_string())
    }
    fn too_many_redirects() -> Self {
        Self("too many redirects".to_string())
    }
}

/// `ipv4ToInt(host)`: dotted IPv4 to a 32-bit integer, `None` when it is not a
/// valid IPv4 literal.
pub fn ipv4_to_int(host: &str) -> Option<u32> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut value: u32 = 0;
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let octet: u32 = part.parse().ok()?;
        if octet > 255 {
            return None;
        }
        value = value.wrapping_mul(256).wrapping_add(octet);
    }
    Some(value)
}

/// `BLOCKED_V4_RANGES` as `(base, maskBits)`.
const BLOCKED_V4_RANGES: [(u32, u32); 7] = [
    (0, 8),            // 0.0.0.0/8
    (0x0a00_0000, 8),  // 10.0.0.0/8
    (0x6440_0000, 10), // 100.64.0.0/10 CGNAT
    (0x7f00_0000, 8),  // 127.0.0.0/8
    (0xa9fe_0000, 16), // 169.254.0.0/16 cloud metadata
    (0xac10_0000, 12), // 172.16.0.0/12
    (0xc0a8_0000, 16), // 192.168.0.0/16
];

/// `isBlockedIpv4Int(ip)`.
pub fn is_blocked_ipv4_int(ip: u32) -> bool {
    BLOCKED_V4_RANGES.iter().any(|&(base, bits)| {
        let mask: u32 = if bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        (ip & mask) == (base & mask)
    })
}

/// `isBlockedIpv4(host)`.
pub fn is_blocked_ipv4(host: &str) -> bool {
    ipv4_to_int(host).is_some_and(is_blocked_ipv4_int)
}

/// `parseHextets(s)`.
fn parse_hextets(s: &str) -> Option<Vec<u16>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for seg in s.split(':') {
        if seg.is_empty() || seg.len() > 4 || !seg.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        out.push(u16::from_str_radix(seg, 16).ok()?);
    }
    Some(out)
}

/// `parseIPv6ToGroups(rawHost)`: any textual IPv6 form into 8 groups, `None`
/// when it is not a valid literal.
pub fn parse_ipv6_to_groups(raw_host: &str) -> Option<[u16; 8]> {
    let mut host = raw_host.to_ascii_lowercase();

    // An embedded dotted-IPv4 tail, e.g. "::ffff:127.0.0.1", matched with
    // `/(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})$/`.
    static V4_TAIL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})$").expect("static regex")
    });
    let mut v4_groups: Option<[u16; 2]> = None;
    if let Some(caps) = V4_TAIL.captures(&host) {
        let candidate = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let v4_int = ipv4_to_int(&candidate)?;
        v4_groups = Some([(v4_int >> 16) as u16, (v4_int & 0xffff) as u16]);
        let cut = host.len() - candidate.len();
        host.truncate(cut);
        if host.ends_with("::") {
            // The "::" marker itself; the removed IPv4 fills the gap.
        } else if host.ends_with(':') {
            host.pop();
        }
    }

    let double_colon_parts: Vec<&str> = host.split("::").collect();
    if double_colon_parts.len() > 2 {
        return None;
    }

    let groups: Vec<u16> = if double_colon_parts.len() == 2 {
        let head = parse_hextets(double_colon_parts[0])?;
        let tail = parse_hextets(double_colon_parts[1])?;
        let v4_len = v4_groups.map_or(0, |g| g.len());
        let missing = 8i32 - head.len() as i32 - tail.len() as i32 - v4_len as i32;
        if missing < 0 {
            return None;
        }
        let mut groups = head;
        groups.extend(std::iter::repeat_n(0u16, missing as usize));
        groups.extend(tail);
        if let Some(v4) = v4_groups {
            groups.extend(v4);
        }
        groups
    } else {
        let mut groups = parse_hextets(&host)?;
        if let Some(v4) = v4_groups {
            groups.extend(v4);
        }
        groups
    };

    if groups.len() != 8 {
        return None;
    }
    let mut out = [0u16; 8];
    out.copy_from_slice(&groups);
    Some(out)
}

/// `isBlockedIpv6Groups(g)`.
pub fn is_blocked_ipv6_groups(g: [u16; 8]) -> bool {
    let is_zero = |n: usize| g[n] == 0;
    // loopback ::1
    if (0..7).all(is_zero) && g[7] == 1 {
        return true;
    }
    // unspecified ::
    if g.iter().all(|x| *x == 0) {
        return true;
    }
    // link-local fe80::/10
    if (g[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // unique local fc00::/7
    if (g[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    let low32 = ((g[6] as u32) << 16) | g[7] as u32;
    // IPv4-mapped ::ffff:0:0/96
    if (0..5).all(is_zero) && g[5] == 0xffff {
        return is_blocked_ipv4_int(low32);
    }
    // NAT64 well-known prefix 64:ff9b::/96
    if g[0] == 0x0064 && g[1] == 0xff9b && (2..6).all(is_zero) {
        return is_blocked_ipv4_int(low32);
    }
    // IPv4-compatible ::a.b.c.d/96 (deprecated)
    if (0..6).all(is_zero) && low32 != 0 && low32 != 1 {
        return is_blocked_ipv4_int(low32);
    }
    false
}

/// `normalizeHost(hostname)`: lower-case and strip the trailing FQDN dot(s).
pub fn normalize_host(hostname: &str) -> String {
    hostname
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_string()
}

/// `isBlockedHost(host)`.
pub fn is_blocked_host(host: &str) -> bool {
    if BLOCKED_HOSTNAMES.contains(&host) {
        return true;
    }
    if BLOCKED_SUFFIXES.iter().any(|s| host.ends_with(s)) {
        return true;
    }
    if is_blocked_ipv4(host) {
        return true;
    }
    if host.contains(':') {
        let bracketless = host.trim_start_matches('[').trim_end_matches(']');
        if let Some(groups) = parse_ipv6_to_groups(bracketless)
            && is_blocked_ipv6_groups(groups)
        {
            return true;
        }
    }
    false
}

/// `assertPublicUrl(rawUrl)`: layer 1 only.
pub fn assert_public_url(raw_url: &str) -> Result<(), BlockedUrl> {
    let parsed = url::Url::parse(raw_url).map_err(|e| BlockedUrl(e.to_string()))?;
    let Some(host) = parsed.host_str() else {
        return Err(BlockedUrl::internal_host());
    };
    if is_blocked_host(&normalize_host(host)) {
        return Err(BlockedUrl::internal_host());
    }
    Ok(())
}

/// The literal-host check for a parsed `IpAddr`, so layer 2's per-address
/// decision matches layer 1's textual one.
fn is_blocked_addr(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_blocked_ipv4_int(u32::from(*v4)),
        IpAddr::V6(v6) => {
            // An IPv4-mapped/embedded address is caught by the same group check
            // layer 1 uses.
            let octets = v6.octets();
            let mut groups = [0u16; 8];
            for i in 0..8 {
                groups[i] = ((octets[i * 2] as u16) << 8) | octets[i * 2 + 1] as u16;
            }
            is_blocked_ipv6_groups(groups)
        }
    }
}

/// `assertPublicUrlResolved(rawUrl)`: layers 1 and 2.
///
/// A resolution failure is refused rather than allowed through. Allowing it
/// would leave the guard blind to a hostname whose DNS answer is withheld at
/// validation time and supplied to a later, unvalidated connect (DNS
/// rebinding); the caller's fetch error is a worse diagnostic than a 400.
pub async fn assert_public_url_resolved(raw_url: &str) -> Result<(), BlockedUrl> {
    let parsed = url::Url::parse(raw_url).map_err(|e| BlockedUrl(e.to_string()))?;
    let Some(raw_host) = parsed.host_str() else {
        return Err(BlockedUrl::internal_host());
    };
    let host = normalize_host(raw_host);
    if is_blocked_host(&host) {
        return Err(BlockedUrl::internal_host());
    }

    // A literal IPv4/IPv6 address was already covered by layer 1; no lookup
    // applies.
    let bracketless = host.trim_start_matches('[').trim_end_matches(']');
    if ipv4_to_int(bracketless).is_some() || bracketless.contains(':') {
        return Ok(());
    }

    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((bracketless, 0u16))
        .await
        .map_err(|e| BlockedUrl(format!("could not resolve host: {e}")))?
        .collect();
    for addr in addresses {
        if is_blocked_addr(&addr.ip()) {
            return Err(BlockedUrl::resolved_internal_host());
        }
    }
    Ok(())
}

/// A `fetchPublic` response: status, content type and the buffered body.
pub struct PublicResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl PublicResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.body)
    }
}

/// `fetchPublic(url, init, { maxRedirects })`: layer 3.
///
/// `redirect(Policy::none())` is load-bearing — reqwest's default policy follows
/// redirects internally and would silently drop this layer. Each hop's target is
/// re-validated through layer 2 before being followed.
pub async fn fetch_public(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    json_body: Option<&serde_json::Value>,
    proxy_options: &ProxyOptions,
    timeout_ms: u64,
) -> Result<PublicResponse, BlockedUrl> {
    fetch_public_with_max_redirects(
        method,
        url,
        headers,
        json_body,
        proxy_options,
        timeout_ms,
        5,
    )
    .await
}

/// [`fetch_public`] with an explicit hop bound.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_public_with_max_redirects(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    json_body: Option<&serde_json::Value>,
    proxy_options: &ProxyOptions,
    timeout_ms: u64,
    max_redirects: usize,
) -> Result<PublicResponse, BlockedUrl> {
    assert_public_url_resolved(url).await?;
    let mut current_url = url.to_string();

    for hop in 0..=max_redirects {
        // `false` so the client does not follow redirects internally: each hop
        // is re-validated through layer 2 before being followed.
        let target = prepare_send_with_redirect_policy(&current_url, proxy_options, false)
            .await
            .map_err(|e| BlockedUrl(e.to_string()))?;
        let client = target.client;

        let mut request = match method {
            "GET" => client.get(&target.url),
            _ => client.post(&target.url),
        };
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        for (name, value) in &target.extra_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = json_body {
            request = request.json(body);
        }

        let sent =
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), request.send())
                .await
                .map_err(|_| BlockedUrl("request timed out".to_string()))?
                .map_err(|e| BlockedUrl(e.to_string()))?;

        let status = sent.status().as_u16();
        let location = sent
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = sent
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();

        let is_redirect = (300..400).contains(&status);
        let Some(location) = location.filter(|_| is_redirect) else {
            let body = sent.text().await.map_err(|e| BlockedUrl(e.to_string()))?;
            return Ok(PublicResponse {
                status,
                content_type,
                body,
            });
        };

        if hop >= max_redirects {
            return Err(BlockedUrl::too_many_redirects());
        }
        let base = url::Url::parse(&current_url).map_err(|e| BlockedUrl(e.to_string()))?;
        let next = base
            .join(&location)
            .map_err(|e| BlockedUrl(e.to_string()))?
            .to_string();
        assert_public_url_resolved(&next).await?;
        current_url = next;
    }

    Err(BlockedUrl::too_many_redirects())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_literals_in_private_ranges_are_blocked() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
        ] {
            assert!(is_blocked_ipv4(host), "{host} must be blocked");
        }
        for host in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "11.0.0.1"] {
            assert!(!is_blocked_ipv4(host), "{host} must be public");
        }
    }

    #[test]
    fn hostnames_and_suffixes_are_blocked() {
        assert!(is_blocked_host("localhost"));
        // The trailing dot is stripped by `normalize_host` before the check,
        // so `is_blocked_host` itself sees the normalized form.
        assert!(is_blocked_host(&normalize_host("localhost.")));
        assert!(is_blocked_host("metadata.internal"));
        assert!(is_blocked_host("thing.local"));
        assert!(!is_blocked_host("example.com"));
    }

    #[test]
    fn ipv6_mapped_loopback_matches_both_textual_forms() {
        // The whole point of the group parser: both spellings compare equal.
        let dotted = parse_ipv6_to_groups("::ffff:127.0.0.1").unwrap();
        let hex = parse_ipv6_to_groups("::ffff:7f00:1").unwrap();
        assert_eq!(dotted, hex);
        assert!(is_blocked_ipv6_groups(dotted));
        assert!(is_blocked_ipv6_groups(hex));
    }

    #[test]
    fn ipv6_reserved_ranges_are_blocked() {
        assert!(is_blocked_ipv6_groups(parse_ipv6_to_groups("::1").unwrap()));
        assert!(is_blocked_ipv6_groups(parse_ipv6_to_groups("::").unwrap()));
        assert!(is_blocked_ipv6_groups(
            parse_ipv6_to_groups("fe80::1").unwrap()
        ));
        assert!(is_blocked_ipv6_groups(
            parse_ipv6_to_groups("fc00::1").unwrap()
        ));
        assert!(is_blocked_ipv6_groups(
            parse_ipv6_to_groups("64:ff9b::7f00:1").unwrap()
        ));
        assert!(!is_blocked_ipv6_groups(
            parse_ipv6_to_groups("2606:4700:4700::1111").unwrap()
        ));
    }

    #[test]
    fn literal_url_checks_cover_ipv6_in_brackets() {
        assert!(assert_public_url("http://[::1]/").is_err());
        assert!(assert_public_url("http://[::ffff:127.0.0.1]/").is_err());
        assert!(assert_public_url("http://127.0.0.1/").is_err());
        assert!(assert_public_url("http://localhost./").is_err());
        assert!(assert_public_url("https://example.com/").is_ok());
        assert!(assert_public_url("ftp://example.com/").is_ok());
    }

    #[test]
    fn ipv4_to_int_rejects_malformed_input() {
        assert_eq!(ipv4_to_int("127.0.0.1"), Some(0x7f00_0001));
        assert_eq!(ipv4_to_int("1.2.3"), None);
        assert_eq!(ipv4_to_int("1.2.3.999"), None);
        assert_eq!(ipv4_to_int("1.2.3.x"), None);
    }
}
