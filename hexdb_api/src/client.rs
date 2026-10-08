// HexDB API: the client's address
//
// The client address drives sign-in throttling and appears in request and
// audit logs. By default it is the TCP peer. Behind a reverse proxy every
// request would come from the proxy, so `network.trusted_proxies` lists the
// proxies (addresses or CIDR ranges) whose `X-Forwarded-For` header is
// believed: walking the header from the right (the hop nearest to us), the
// first address that isn't a trusted proxy is the client. Requests from
// anywhere else keep their TCP peer address, so clients can't spoof it.

use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{request::Parts, HeaderMap},
    middleware::Next,
    response::Response,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

/// The resolved client address of a request.
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<ClientIp>().cloned().unwrap_or_else(|| ClientIp(peer(&parts.extensions).map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".into()))))
    }
}

fn peer(extensions: &axum::http::Extensions) -> Option<IpAddr> {
    extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())
}

/// One address range.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(text: &str) -> Option<Cidr> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (text.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Cidr { network: addr, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 address may arrive mapped into IPv6.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// The configured trusted proxies.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies(Vec<Cidr>);

impl TrustedProxies {
    /// Parse `network.trusted_proxies`; names the first invalid entry.
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        entries
            .iter()
            .map(|e| Cidr::parse(e).ok_or_else(|| format!("network.trusted_proxies: '{}' is not an IP address or CIDR range", e)))
            .collect::<Result<Vec<_>, _>>()
            .map(TrustedProxies)
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(ip))
    }

    /// The client address for a request from `peer` with these headers.
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        if !self.trusts(peer) {
            return peer;
        }
        let hops: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let mut client = peer;
        for hop in hops.iter().rev() {
            // A malformed entry ends the chain; the last good hop is used.
            let Ok(ip) = hop.parse::<IpAddr>() else { break };
            client = ip;
            if !self.trusts(ip) {
                break;
            }
        }
        client
    }
}

/// Record the client address on every request (see the module comment). A
/// request forwarded by a replica carries the original client's address,
/// believed only with a valid lattice signature (see `crate::forward`).
pub async fn resolve_client(
    State((proxies, engine)): State<(Arc<TrustedProxies>, Arc<hexdb_core::engine::HexDBEngine>)>,
    mut request: Request,
    next: Next,
) -> Response {
    use crate::forward::{signed_target, Forwarded, FORWARDED_FOR_HEADER, FORWARD_SIGNATURE_HEADER};
    let mut address = match peer(request.extensions()) {
        Some(peer) => proxies.resolve(peer, request.headers()).to_string(),
        None => "unknown".into(),
    };
    let forwarded_for = request.headers().get(FORWARDED_FOR_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let signature = request.headers().get(FORWARD_SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let (Some(client), Some(signature)) = (forwarded_for, signature) {
        let target = request.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
        let method = request.method().as_str().to_string();
        if engine.lattice_keys.verify_request(&signature, &method, &signed_target(&target, &client), b"", &engine.lattice_nonces) {
            address = client;
            request.extensions_mut().insert(Forwarded);
        } else {
            tracing::warn!("🚫 Ignoring a forwarded-request header without a valid lattice signature.");
        }
    }
    request.headers_mut().remove(FORWARDED_FOR_HEADER);
    request.headers_mut().remove(FORWARD_SIGNATURE_HEADER);
    request.extensions_mut().insert(ClientIp(address));
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(xff: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", xff.parse().unwrap());
        h
    }

    #[test]
    fn ranges_match() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.200.1.1".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(c.contains("::ffff:10.1.2.3".parse().unwrap()), "IPv4-mapped IPv6");
        assert!(Cidr::parse("fd00::/8").unwrap().contains("fd12::1".parse().unwrap()));
        assert!(Cidr::parse("127.0.0.1").unwrap().contains("127.0.0.1".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("proxy.local").is_none());
        assert!(TrustedProxies::parse(&["nope".into()]).is_err());
    }

    #[test]
    fn forwarded_for_is_only_believed_from_trusted_proxies() {
        let proxies = TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap();
        let proxy: IpAddr = "10.0.0.5".parse().unwrap();
        let stranger: IpAddr = "203.0.113.9".parse().unwrap();
        // From the proxy: the rightmost untrusted hop is the client.
        assert_eq!(proxies.resolve(proxy, &headers("198.51.100.7")), "198.51.100.7".parse::<IpAddr>().unwrap());
        assert_eq!(proxies.resolve(proxy, &headers("1.1.1.1, 198.51.100.7, 10.0.0.9")), "198.51.100.7".parse::<IpAddr>().unwrap(), "a spoofed leftmost entry is ignored");
        assert_eq!(proxies.resolve(proxy, &HeaderMap::new()), proxy);
        assert_eq!(proxies.resolve(proxy, &headers("garbage")), proxy);
        // From anyone else: the header is ignored.
        assert_eq!(proxies.resolve(stranger, &headers("1.2.3.4")), stranger);
        assert_eq!(TrustedProxies::default().resolve(proxy, &headers("1.2.3.4")), proxy);
    }
}
