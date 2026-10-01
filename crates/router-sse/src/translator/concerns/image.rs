//! Remote-image fetch and data-URI helpers.
//!
//! The fetch is the security-sensitive part. Three guards, all of which must
//! hold before a byte of the body is read:
//!
//! 1. Every DNS record for the host must resolve to a public IP. One private
//!    record rejects the whole host, which defeats a multi-A-record trick.
//! 2. The connection is pinned to the validated IP, so a second resolution
//!    cannot rebind between the check and the connect (TOCTOU).
//! 3. Redirects are not followed, so a public URL cannot bounce to a private
//!    one.
//!
//! Then the body is streamed with a hard byte cap and the magic bytes must
//! match a known image signature — a disguised payload is rejected, not stored.

use std::net::{IpAddr, Ipv4Addr};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;

use crate::executors::http::tls_builder;
use crate::runtime_config::{
    BLOCKED_HOSTS, IMAGE_SIGNATURES, MAX_IMAGE_BYTES, MEDIA_FETCH_TIMEOUT_MS,
};

/// `encodeDataUri(mimeType, base64)`.
pub fn encode_data_uri(mime_type: &str, base64: &str) -> String {
    format!("data:{mime_type};base64,{base64}")
}

/// A parsed `data:` URI.
pub struct DataUri<'a> {
    pub mime_type: &'a str,
    pub base64: &'a str,
}

/// `parseDataUri(url)`: `^data:([^;]+);base64,([\s\S]+)$`.
///
/// Hand-rolled rather than a regex because the payload can be megabytes and a
/// regex over it allocates; this scans for the two separators once.
pub fn parse_data_uri(url: &str) -> Option<DataUri<'_>> {
    let rest = url.strip_prefix("data:")?;
    let semi = rest.find(';')?;
    let mime = &rest[..semi];
    if mime.is_empty() {
        return None;
    }
    let after = &rest[semi + 1..];
    let payload = after.strip_prefix("base64,")?;
    if payload.is_empty() {
        return None;
    }
    Some(DataUri {
        mime_type: mime,
        base64: payload,
    })
}

/// `isPrivateIp(ip)`: loopback, RFC1918, link-local, CGNAT, IPv6 ULA/link-local.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let seg = v6.segments()[0];
            // fc00::/7 unique-local, fe80::/10 link-local.
            if seg & 0xfe00 == 0xfc00 || seg & 0xffc0 == 0xfe80 {
                return true;
            }
            // ::ffff:a.b.c.d — validate the embedded IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            false
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
}

/// `detectImageMime(buf)`: the first matching signature's mime, else `None`.
pub fn detect_image_mime(buf: &[u8]) -> Option<&'static str> {
    for entry in &IMAGE_SIGNATURES {
        if buf.len() < entry.offset + entry.sig.len() {
            continue;
        }
        if buf[entry.offset..entry.offset + entry.sig.len()] != *entry.sig {
            continue;
        }
        if entry.verify_webp && !(buf.len() >= 12 && &buf[8..12] == b"WEBP") {
            continue;
        }
        return Some(entry.mime);
    }
    None
}

/// A fetched image: the data URI and its detected mime.
pub struct FetchedImage {
    pub url: String,
    pub mime_type: &'static str,
}

/// `resolvePinnedIps(hostname)`: the public IPs for a host, or `None` when the
/// host is blocked, unresolvable, or resolves to anything private.
async fn resolve_pinned_ips(hostname: &str) -> Option<Vec<IpAddr>> {
    if hostname.is_empty() || BLOCKED_HOSTS.contains(&hostname.to_lowercase().as_str()) {
        return None;
    }
    let records: Vec<IpAddr> = tokio::net::lookup_host((hostname, 0))
        .await
        .ok()?
        .map(|sa| sa.ip())
        .collect();
    if records.is_empty() || records.iter().copied().any(is_private_ip) {
        return None;
    }
    Some(records)
}

/// `fetchImageAsBase64(url, options)`: `None` on any failure or rejection.
///
/// The client is built per request so the validated IP can be pinned with
/// `ClientBuilder::resolve`, which keeps SNI intact (unlike rewriting the host
/// to the bare IP). `redirect(Policy::none())` closes the redirect bypass.
pub async fn fetch_image_as_base64(image_url: &str) -> Option<FetchedImage> {
    if !(image_url.starts_with("http://") || image_url.starts_with("https://")) {
        return None;
    }
    let url = url::Url::parse(image_url).ok()?;
    let hostname = url.host_str()?.to_string();
    let pinned = resolve_pinned_ips(&hostname).await?;
    let port = url.port_or_known_default()?;

    let client = tls_builder()
        .resolve(&hostname, std::net::SocketAddr::new(pinned[0], port))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_millis(MEDIA_FETCH_TIMEOUT_MS))
        .build()
        .ok()?;

    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    // Stream-read with a hard cap so a huge payload never lands in memory.
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }

    let mime_type = detect_image_mime(&buf)?;
    Some(FetchedImage {
        url: encode_data_uri(mime_type, &BASE64.encode(&buf)),
        mime_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn data_uris_round_trip_and_reject_non_data() {
        let uri = encode_data_uri("image/png", "AAAA");
        assert_eq!(uri, "data:image/png;base64,AAAA");
        let parsed = parse_data_uri(&uri).unwrap();
        assert_eq!(parsed.mime_type, "image/png");
        assert_eq!(parsed.base64, "AAAA");
        assert!(parse_data_uri("https://x/y.png").is_none());
        assert!(
            parse_data_uri("data:image/png;utf8,AAAA").is_none(),
            "only base64"
        );
        assert!(parse_data_uri("data:;base64,AAAA").is_none(), "empty mime");
        assert!(
            parse_data_uri("data:image/png;base64,").is_none(),
            "empty payload"
        );
    }

    #[test]
    fn private_and_public_ips_are_classified() {
        for private in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.5",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
        ] {
            assert!(
                is_private_ip(private.parse().unwrap()),
                "{private} must be private"
            );
        }
        for public in ["8.8.8.8", "1.1.1.1", "203.0.113.5"] {
            assert!(
                !is_private_ip(public.parse().unwrap()),
                "{public} must be public"
            );
        }
        assert!(is_private_ip("::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_private_ip("fc00::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_private_ip("fe80::1".parse::<Ipv6Addr>().unwrap().into()));
        // IPv4-mapped IPv6 defers to the embedded IPv4.
        assert!(is_private_ip(
            "::ffff:127.0.0.1".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!is_private_ip(
            "::ffff:8.8.8.8".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!is_private_ip(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap().into()
        ));
    }

    #[test]
    fn magic_bytes_identify_each_supported_format() {
        assert_eq!(
            detect_image_mime(&[0x89, 0x50, 0x4e, 0x47, 0x0d]),
            Some("image/png")
        );
        assert_eq!(
            detect_image_mime(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        assert_eq!(detect_image_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(detect_image_mime(b"BMxxxx"), Some("image/bmp"));
        // RIFF but not WEBP at 8..12 is rejected.
        assert_eq!(detect_image_mime(b"RIFFxxxxAVI "), None);
        assert_eq!(
            detect_image_mime(b"RIFF\x00\x00\x00\x00WEBP"),
            Some("image/webp")
        );
        assert_eq!(detect_image_mime(b"not an image"), None);
    }

    #[test]
    fn a_truncated_signature_does_not_match() {
        assert_eq!(detect_image_mime(&[0x89, 0x50]), None);
        assert_eq!(detect_image_mime(&[]), None);
    }
}
