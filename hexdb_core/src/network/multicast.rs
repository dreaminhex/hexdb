// Multicast LAN discovery (`network.multicast`).
//
// Seed peers (`network.peers`) and the local port scan find hexes someone
// told a hex about. On a LAN, multicast finds them with no addresses at all:
// every discovery round, each hex announces its discovery address to a
// multicast group (`network.multicast_group`, 239.255.70.2:7702 by default,
// an administratively scoped address that stays on the local network), and a
// starting hex asks the group, so the others answer at once instead of at
// their next round.
//
// An announcement only says where to probe. What a hex learns from one goes
// through the usual authenticated handshake (see `discovery`) before it counts
// for anything, like an address learned from a prober. Announcements are
// signed with a key derived from the lattice key and carry their time, so a
// hex ignores those of other lattices and stale replays:
//
//   HEXDB-MCAST 1 <announce|query> <hex ID> <discovery address> <epoch ms> <signature>
//
// Cloud networks (Fly.io, most VPCs) don't carry multicast: there, use
// `network.peers` (a name that resolves to every machine works well).

use crate::config::HexConfig;
use crate::HexDBEngine;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, warn};

const MAGIC: &str = "HEXDB-MCAST";
const VERSION: &str = "1";
/// Announcements older (or further in the future) than this are ignored.
const MAX_AGE_MS: i64 = 60_000;
/// How long a starting hex waits for answers to its query.
const QUERY_WAIT: Duration = Duration::from_millis(1500);
const MAX_PACKET: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Announce,
    Query,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Announce => "announce",
            Kind::Query => "query",
        }
    }
}

/// The signing key for announcements, from a lattice key.
fn signing_key(lattice_key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("HexDB 2026 multicast discovery v1", lattice_key)
}

/// What a packet says: its kind, the sending hex's ID and its discovery address.
#[derive(Debug, PartialEq)]
struct Heard {
    kind: Kind,
    id: String,
    addr: String,
}

fn signature(key: &[u8; 32], kind: Kind, id: &str, addr: &str, at: i64) -> String {
    blake3::keyed_hash(key, format!("{}\n{}\n{}\n{}", kind.as_str(), id, addr, at).as_bytes()).to_hex().to_string()
}

fn packet(lattice_key: &[u8; 32], kind: Kind, id: &str, addr: &str, at: i64) -> String {
    format!("{} {} {} {} {} {} {}", MAGIC, VERSION, kind.as_str(), id, addr, at, signature(&signing_key(lattice_key), kind, id, addr, at))
}

/// What a packet says, if it's signed with one of `keys` and recent.
fn parse(text: &str, keys: &[[u8; 32]], now: i64) -> Option<Heard> {
    let mut parts = text.trim().split(' ');
    if parts.next()? != MAGIC || parts.next()? != VERSION {
        return None;
    }
    let kind = match parts.next()? {
        "announce" => Kind::Announce,
        "query" => Kind::Query,
        _ => return None,
    };
    let id = parts.next()?;
    let addr = parts.next()?;
    let at: i64 = parts.next()?.parse().ok()?;
    let presented = parts.next()?;
    if parts.next().is_some() || id.is_empty() || id.len() > 64 || addr.is_empty() || addr.len() > 255 || (now - at).abs() > MAX_AGE_MS {
        return None;
    }
    let signed = keys.iter().any(|key| {
        let expected = signature(&signing_key(key), kind, id, addr, at);
        // Constant-time: a signature is a secret derived from the lattice key.
        expected.len() == presented.len() && expected.bytes().zip(presented.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    });
    signed.then(|| Heard { kind, id: id.to_string(), addr: addr.to_string() })
}

/// The multicast group to use, if `network.multicast` is on.
fn group(config: &HexConfig) -> Option<SocketAddrV4> {
    if !config.network.multicast {
        return None;
    }
    match config.network.multicast_group.parse::<SocketAddrV4>() {
        Ok(group) if group.ip().is_multicast() => Some(group),
        _ => {
            warn!("❗ network.multicast_group '{}' isn't an IPv4 multicast address and port; multicast discovery is off.", config.network.multicast_group);
            None
        }
    }
}

/// A UDP socket joined to `group`, sharing its port with other hexes on this machine.
fn join(group: SocketAddrV4) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, group.port())).into())?;
    socket.join_multicast_v4(group.ip(), &Ipv4Addr::UNSPECIFIED)?;
    // Hexes on this machine hear it too; one router hop at most.
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The next valid packet and where it came from (`None`: something else arrived).
async fn receive(socket: &UdpSocket, keys: &[[u8; 32]]) -> Option<(Heard, SocketAddr)> {
    let mut buf = [0u8; MAX_PACKET];
    let (len, from) = socket.recv_from(&mut buf).await.ok()?;
    let text = std::str::from_utf8(&buf[..len]).ok()?;
    Some((parse(text, keys, now_ms())?, from))
}

async fn send(socket: &UdpSocket, text: &str, group: SocketAddrV4) {
    if let Err(e) = socket.send_to(text.as_bytes(), group).await {
        debug!("Multicast discovery can't send to {}: {}", group, e);
    }
}

/// At startup, before this hex listens: ask the group, and return the
/// discovery addresses that answer within `QUERY_WAIT`.
pub async fn query(config: &HexConfig, id: &str) -> Vec<String> {
    let Some(group) = group(config) else { return Vec::new() };
    let Ok(keys) = config.lattice_keys() else { return Vec::new() };
    let socket = match join(group) {
        Ok(socket) => socket,
        Err(e) => {
            debug!("Multicast discovery can't join {}: {}", group, e);
            return Vec::new();
        }
    };
    let me = super::discovery::advertised_discovery(config);
    if let Err(e) = socket.send_to(packet(&keys[0], Kind::Query, id, &me, now_ms()).as_bytes(), group).await {
        debug!("Multicast discovery can't send to {}: {}", group, e);
        return Vec::new();
    }
    let mut found = Vec::new();
    let deadline = tokio::time::Instant::now() + QUERY_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, receive(&socket, &keys)).await {
            Err(_) => break,
            Ok(Some((heard, from))) if heard.kind == Kind::Announce && heard.id != id => {
                found.push(super::discovery::announced_at(&heard.addr, from));
            }
            Ok(_) => {}
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Announce this hex every `interval`, answer queries, and learn of the hexes
/// that announce themselves (they're probed from the next discovery round).
pub fn spawn(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    let Some(group) = group(&engine.config) else { return };
    crate::tasks::supervise("multicast discovery", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let keys = match engine.config.lattice_keys() {
                Ok(keys) => keys,
                Err(e) => {
                    warn!("❗ Multicast discovery is off: {:#}", e);
                    return;
                }
            };
            let socket = match join(group) {
                Ok(socket) => socket,
                Err(e) => {
                    warn!("❗ Multicast discovery can't join {} ({}); finding hexes by network.peers and the local scan only.", group, e);
                    return;
                }
            };
            info!("📡 Multicast discovery on {}.", group);
            let (id, me) = (engine.id.to_string(), super::discovery::advertised_discovery(&engine.config));
            let announcement = || packet(&keys[0], Kind::Announce, &id, &me, now_ms());
            let mut tick = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    _ = tick.tick() => send(&socket, &announcement(), group).await,
                    received = receive(&socket, &keys) => {
                        // This hex's own packets come back too (the loop is on, for hexes on this machine).
                        let Some((heard, from)) = received.filter(|(heard, _)| heard.id != id) else { continue };
                        if heard.kind == Kind::Query {
                            // A hex starting up: answer now rather than at the next round.
                            send(&socket, &announcement(), group).await;
                        }
                        super::discovery::learn_announced(&engine, &heard.addr, from);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_wildcard_host_is_replaced_by_the_sender() {
        let from: SocketAddr = "192.168.1.20:7702".parse().unwrap();
        use super::super::discovery::announced_at;
        assert_eq!(announced_at("0.0.0.0:7702", from), "192.168.1.20:7702");
        assert_eq!(announced_at("127.0.0.1:7705", from), "127.0.0.1:7705", "a hex on loopback is only reached on its machine");
        assert_eq!(announced_at("10.0.0.5:7702", from), "10.0.0.5:7702");
        assert_eq!(announced_at("0.0.0.0:7702", "127.0.0.1:7702".parse().unwrap()), "127.0.0.1:7702");
    }

    #[test]
    fn announcements_are_signed_recent_and_from_the_lattice() {
        let key = [7u8; 32];
        let other = [8u8; 32];
        let now = 1_800_000_000_000;
        let text = packet(&key, Kind::Announce, "01HEX", "10.0.0.5:7702", now);
        let heard = |kind, addr: &str| Some(Heard { kind, id: "01HEX".into(), addr: addr.into() });
        assert_eq!(parse(&text, &[key], now + 1_000), heard(Kind::Announce, "10.0.0.5:7702"));
        // A previous key in the ring still counts (a secret being rotated).
        assert!(parse(&text, &[other, key], now).is_some());
        assert_eq!(parse(&text, &[other], now), None, "another lattice's announcement");
        assert_eq!(parse(&text, &[key], now + MAX_AGE_MS + 1), None, "a stale replay");
        assert_eq!(parse(&text.replace("10.0.0.5", "10.0.0.6"), &[key], now), None, "a changed address");
        assert_eq!(parse(&text.replace("announce", "query"), &[key], now), None, "a changed kind");
        assert_eq!(parse(&text.replace("01HEX", "01HEY"), &[key], now), None, "a changed sender");
        assert_eq!(parse("HEXDB-MCAST 2 announce x a 1 b", &[key], now), None);
        assert_eq!(parse("hello", &[key], now), None);
        let query = packet(&key, Kind::Query, "01HEX", "0.0.0.0:7703", now);
        assert_eq!(parse(&query, &[key], now), heard(Kind::Query, "0.0.0.0:7703"));
    }
}
