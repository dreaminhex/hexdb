// HexDB Core Egress: which addresses outgoing webhooks may reach
//
// Webhooks a user configures (A2A push notifications, stream destinations)
// are requests the server makes for them, so they're checked against
// server-side request forgery: every address the host resolves to must be on
// the public internet (not loopback, private, link-local, CGNAT, multicast or
// reserved), checked when the webhook is configured and again before each
// request, with the connection pinned to the checked address so DNS can't
// change in between (see `pinned_client`). Redirects aren't followed. An
// administrator can allow private networks per feature in the lattice config
// (`a2a.push_private_networks`, `streams.private_networks`), for receivers on
// the same network.

use reqwest::Url;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Whether an address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            // NAT64 (64:ff9b::/96) reaches the IPv4 address inside it.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            // IPv4-compatible (::a.b.c.d, deprecated) addresses reach IPv4 too.
            if s[..6] == [0, 0, 0, 0, 0, 0] && !v6.is_loopback() && !v6.is_unspecified() {
                return is_public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240)
}

/// A URL's host and port, and the addresses it resolves to now.
pub async fn resolve(url: &Url) -> Result<(String, Vec<SocketAddr>), String> {
    let host = url.host_str().ok_or("The URL has no host.")?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host.as_str(), port)).await.map_err(|_| format!("Couldn't resolve {}.", host))?.collect(),
    };
    if addrs.is_empty() {
        return Err(format!("{} has no addresses.", host));
    }
    Ok((host, addrs))
}

/// Check that a URL may be called: no credentials in it, and (unless
/// `private_networks`) every address it resolves to public. Returns the
/// host and the checked addresses, for [`pinned_client`].
pub async fn check(url: &Url, private_networks: bool, setting: &str) -> Result<(String, Vec<SocketAddr>), String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Put credentials in headers, not in the URL.".into());
    }
    let (host, addrs) = resolve(url).await?;
    if !private_networks && !addrs.iter().all(|a| is_public(a.ip())) {
        return Err(format!("{} isn't a public address; webhooks on private networks need {} in the lattice config.", host, setting));
    }
    Ok((host, addrs))
}

/// An HTTP client that connects `host` to the address just checked, whatever
/// DNS says next, and doesn't follow redirects.
pub fn pinned_client(host: &str, addr: SocketAddr, timeout: Duration) -> reqwest::Result<reqwest::Client> {
    let mut builder = crate::crypt::http_client().timeout(timeout).redirect(reqwest::redirect::Policy::none());
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve(host, addr);
    }
    builder.build()
}

/// A URL as it may be shown or logged: the password and query values replaced.
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else { return url.to_string() };
    if parsed.password().is_some() {
        let _ = parsed.set_password(Some(REDACTED));
    }
    if parsed.query().is_some() {
        let pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, _)| (k.into_owned(), REDACTED.to_string())).collect();
        parsed.query_pairs_mut().clear().extend_pairs(pairs);
    }
    parsed.to_string()
}

/// What a secret reads as when it's shown.
pub const REDACTED: &str = "********";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_public() {
        for private in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "::", "fd00::1", "fe80::1", "fec0::1",
            "::ffff:10.0.0.1", "::ffff:127.0.0.1", "::127.0.0.1", "64:ff9b::a00:1", "224.0.0.1", "255.255.255.255",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{}", private);
        }
        for public in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public(public.parse().unwrap()), "{}", public);
        }
    }

    #[tokio::test]
    async fn private_addresses_and_credentials_are_refused() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(check(&url("https://127.0.0.1/hook"), false, "x").await.is_err(), "loopback");
        assert!(check(&url("http://[::1]/hook"), false, "x").await.is_err(), "loopback v6");
        assert!(check(&url("http://[::ffff:7f00:1]/hook"), false, "x").await.is_err(), "IPv4-mapped loopback");
        assert!(check(&url("http://169.254.169.254/latest/meta-data"), false, "x").await.is_err(), "cloud metadata");
        assert!(check(&url("http://localhost:9/hook"), false, "x").await.is_err(), "a name resolving to loopback");
        assert!(check(&url("https://user:pw@8.8.8.8/hook"), true, "x").await.is_err(), "credentials in the URL");
        assert!(check(&url("https://8.8.8.8/hook"), false, "x").await.is_ok());
        assert!(check(&url("http://127.0.0.1:9000/hook"), true, "x").await.is_ok(), "allowed on private networks");
    }

    #[test]
    fn urls_are_redacted() {
        assert_eq!(redact_url("https://u:secret@h.example/p?token=abc&x=1"), "https://u:********@h.example/p?token=********&x=********");
        assert_eq!(redact_url("https://h.example/p"), "https://h.example/p");
    }
}
