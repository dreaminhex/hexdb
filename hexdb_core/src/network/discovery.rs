// HexDB lattice discovery and role election.
//
// Every hex runs a small TCP discovery listener. A peer connects, sends an
// authenticated hello, and gets back an authenticated `HEXDB_IDENTITY <json>`
// describing the hex: its lattice, role, API address, resources and
// replication position. Both sides prove they hold the lattice key (see
// `lattice_auth`); hexes without it are ignored and learn nothing.
//
// A background task probes the configured seed addresses (and, by default, the
// local discovery ports 7702-7709) every `discovery_interval_seconds`, tracks
// when each peer was last seen, and re-runs the role election:
//
// - Candidates for Overseer are live hexes whose preference is `auto` or
//   `overseer`; if any prefer `overseer`, only those are considered.
// - A hex that is already Overseer keeps the role while it is alive, so
//   leadership doesn't flap when a larger node joins. If two hexes both claim
//   it (for example after a network partition heals), the better-ranked wins.
// - Otherwise candidates are ranked by RAM, then disk, then ID.
// - Every other hex is a Harvester, or a Replicant if it prefers that.
//
// Every hex applies the same rule to the same view, so they agree without a
// consensus round. Views can briefly differ while a peer joins or leaves.

use crate::network::lattice_auth::LatticeKeys;
use crate::{HexConfig, HexDBEngine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, sync::Arc, time::Duration};
use ulid::Ulid;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tracing::{debug, info, warn};

pub const ROLE_OVERSEER: &str = "Overseer";
pub const ROLE_HARVESTER: &str = "Harvester";
pub const ROLE_REPLICANT: &str = "Replicant";

const LOCAL_PORTS: std::ops::Range<u16> = 7702..7710;
/// How long to wait for one peer to connect and identify itself.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// A peer not seen for this many discovery rounds is marked lost...
const LOST_AFTER_ROUNDS: u32 = 3;
/// ...and forgotten after this many.
const FORGET_AFTER_ROUNDS: u32 = 30;

/// A hex as described in the discovery handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerHex {
    pub id: String,
    pub name: String,
    pub role: String,
    pub lattice: String,
    /// Discovery address.
    pub ip: String,
    /// REST/GraphQL address other hexes can reach.
    #[serde(default)]
    pub api_endpoint: String,
    #[serde(default)]
    pub ram_mb: u64,
    #[serde(default)]
    pub disk_mb: u64,
    /// Configured role preference: auto, overseer, harvester or replicant.
    #[serde(default = "default_preference")]
    pub preference: String,
    /// Highest sequence number this hex has applied.
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default)]
    pub version: String,
    /// Replication state: leading, streaming, syncing, waiting, or error.
    #[serde(default)]
    pub replication_state: String,
    /// The Overseer sequence number this hex has applied (for the Overseer,
    /// its own latest published sequence number).
    #[serde(default)]
    pub applied_seq: u64,
    /// True when the API is served over HTTPS.
    #[serde(default)]
    pub tls: bool,
}

fn default_preference() -> String {
    "auto".into()
}

/// A peer as tracked by this hex.
#[derive(Debug, Clone, Serialize)]
pub struct LatticeMember {
    #[serde(flatten)]
    pub hex: PeerHex,
    pub last_seen: DateTime<Utc>,
    /// "active", or "lost" when it hasn't answered for a few rounds.
    pub status: String,
}

/// Normalize and check a configured role preference.
pub fn parse_preference(value: &str) -> anyhow::Result<String> {
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "auto" | "overseer" | "harvester" | "replicant" => Ok(lower),
        _ => anyhow::bail!("identity.role must be auto, overseer, harvester or replicant (got '{}')", value),
    }
}

/// Replace the host of `endpoint` with `host`.
fn with_host(endpoint: &str, host: &str) -> String {
    match endpoint.rsplit_once(':') {
        Some((_, port)) => format!("{}:{}", host, port),
        None => endpoint.to_string(),
    }
}

fn host_of(endpoint: &str) -> &str {
    endpoint.rsplit_once(':').map(|(h, _)| h).unwrap_or(endpoint)
}

fn is_local_or_wildcard(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "[::]" | "127.0.0.1" | "localhost" | "[::1]")
}

/// An endpoint as other hexes should see it.
fn advertised(config: &HexConfig, endpoint: &str) -> String {
    match &config.network.advertise_host {
        Some(host) if !host.trim().is_empty() => with_host(endpoint, host.trim()),
        _ => endpoint.to_string(),
    }
}

/// This hex's current identity.
pub async fn local_identity(engine: &HexDBEngine) -> PeerHex {
    let config = &engine.config;
    let replication = if engine.is_writable() {
        ("leading".to_string(), engine.changes.published_seq())
    } else {
        let status = engine.replication.lock().unwrap();
        (status.state.clone(), status.applied_seq)
    };
    PeerHex {
        id: engine.id.to_string(),
        name: engine.name.clone(),
        role: engine.role(),
        lattice: config.network.lattice_name.clone(),
        ip: advertised(config, &config.network.discovery_endpoint),
        api_endpoint: advertised(config, &config.network.api_endpoint),
        ram_mb: config.memory.ram_mb,
        disk_mb: config.storage.disk_mb,
        preference: parse_preference(&config.identity.role).unwrap_or_else(|_| "auto".into()),
        last_seq: engine.stats().await.next_seq.saturating_sub(1),
        version: engine.version.clone(),
        replication_state: replication.0,
        applied_seq: replication.1,
        tls: config.tls.enabled(),
    }
}

/// Answer discovery handshakes with this hex's current identity.
pub async fn start_discovery_listener(engine: Arc<HexDBEngine>) {
    let addr = engine.config.network.discovery_endpoint.clone();
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            warn!(%addr, "❗ Failed to bind the discovery endpoint ({}); other hexes won't find this one.", e);
            return;
        }
    };
    info!(%addr, "📡 Discovery listener active.");

    loop {
        match listener.accept().await {
            Ok((mut socket, _)) => {
                let engine = engine.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(&mut socket);
                    let mut line = String::new();
                    // Hellos are one short line; don't buffer more from strangers.
                    let read = tokio::time::timeout(PROBE_TIMEOUT * 4, (&mut reader).take(512).read_line(&mut line)).await;
                    if !matches!(read, Ok(Ok(_))) {
                        return;
                    }
                    let Some((nonce, key_index)) = engine.lattice_keys.check_hello(&line, &engine.lattice_nonces) else {
                        debug!("📡 Ignored an unauthenticated discovery hello.");
                        return;
                    };
                    let identity = local_identity(&engine).await;
                    if let Ok(json) = serde_json::to_string(&identity) {
                        let reply = engine.lattice_keys.identity_reply(key_index, &nonce, &json);
                        let _ = socket.write_all(format!("{}\n", reply).as_bytes()).await;
                    }
                });
            }
            Err(e) => warn!(%e, "❗ Discovery accept failed."),
        }
    }
}

/// Discovery addresses to probe: configured seeds, plus local ports if enabled.
fn probe_targets(config: &HexConfig) -> Vec<String> {
    let mut targets: Vec<String> = config.network.peers.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if config.network.scan_local_ports {
        for port in LOCAL_PORTS {
            targets.push(format!("127.0.0.1:{}", port));
        }
    }
    let own = &config.network.discovery_endpoint;
    let own_port = own.rsplit_once(':').map(|(_, p)| p).unwrap_or_default();
    targets.retain(|t| {
        // Skip ourselves (by exact address, or a loopback address on our port).
        t != own && !(is_local_or_wildcard(host_of(t)) && is_local_or_wildcard(host_of(own)) && t.ends_with(&format!(":{}", own_port)))
    });
    targets.sort();
    targets.dedup();
    targets
}

/// Send an authenticated hello to one address and verify the reply. A hex
/// that doesn't answer the current key is tried with previous keys (a
/// lattice part-way through a secret rotation).
async fn probe_peer(addr: String, keys: LatticeKeys) -> Option<PeerHex> {
    let attempt = PROBE_TIMEOUT;
    for index in 0..keys.len() {
        match tokio::time::timeout(attempt, probe_with_key(&addr, &keys, index)).await {
            Ok(Probe::Found(peer)) => return Some(*peer),
            Ok(Probe::Unreachable) => return None,
            Ok(Probe::Refused) | Err(_) => continue,
        }
    }
    None
}

enum Probe {
    Found(Box<PeerHex>),
    /// Nothing listens there.
    Unreachable,
    /// Something answered, but not with a valid identity for this key.
    Refused,
}

async fn probe_with_key(addr: &str, keys: &LatticeKeys, index: usize) -> Probe {
    let Ok(mut stream) = TcpStream::connect(addr).await else { return Probe::Unreachable };
    let (hello, nonce) = keys.hello_with(index);
    if stream.write_all(format!("{}\n", hello).as_bytes()).await.is_err() {
        return Probe::Refused;
    }
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    if (&mut reader).take(64 * 1024).read_line(&mut line).await.is_err() {
        return Probe::Refused;
    }
    let Some(payload) = keys.check_identity(index, &nonce, &line) else {
        if !line.is_empty() {
            warn!(%addr, "🚫 {} answered discovery without proving it holds the lattice key; ignoring it.", addr);
        }
        return Probe::Refused;
    };
    let Ok(mut peer) = serde_json::from_str::<PeerHex>(payload) else { return Probe::Refused };
    Probe::Found(Box::new(fix_endpoint(addr, &mut peer)))
}

/// A peer that advertises a loopback or wildcard address is reachable at the
/// host we just connected to.
fn fix_endpoint(addr: &str, peer: &mut PeerHex) -> PeerHex {
    let mut peer = peer.clone();

    let probed_host = host_of(addr).to_string();
    if !is_local_or_wildcard(&probed_host) {
        if is_local_or_wildcard(host_of(&peer.api_endpoint)) {
            peer.api_endpoint = with_host(&peer.api_endpoint, &probed_host);
        }
        peer.ip = addr.to_string();
    }
    peer
}

/// Probe every target once and return the hexes in our lattice (excluding ourselves).
pub async fn discover_peers(config: &HexConfig, local_id: &str, local_name: &str) -> Vec<PeerHex> {
    let keys = match config.lattice_keys() {
        Ok(keys) => LatticeKeys::with_previous(&keys),
        Err(e) => {
            warn!("❗ Discovery is disabled: {:#}", e);
            return Vec::new();
        }
    };
    let probes = probe_targets(config).into_iter().map(|addr| {
        let keys = keys.clone();
        let budget = PROBE_TIMEOUT * keys.len() as u32;
        async move { tokio::time::timeout(budget, probe_peer(addr, keys)).await.ok().flatten() }
    });

    let mut peers: Vec<PeerHex> = Vec::new();
    for peer in futures::future::join_all(probes).await.into_iter().flatten() {
        if peer.lattice != config.network.lattice_name || peer.id == local_id || peers.iter().any(|p| p.id == peer.id) {
            continue;
        }
        if peer.name == local_name {
            warn!(name = %peer.name, "🎭 Name collision detected.");
        }
        peers.push(peer);
    }
    peers
}

const NAME_ADJECTIVES: &[&str] = &[
    "Amber", "Azure", "Bright", "Cobalt", "Crimson", "Distant", "Drifting", "Ember", "Frozen", "Gilded", "Hidden", "Indigo", "Iron", "Jade",
    "Lunar", "Molten", "Northern", "Obsidian", "Pale", "Quiet", "Radiant", "Scarlet", "Silent", "Silver", "Solar", "Stellar", "Swift",
    "Twin", "Umbral", "Velvet", "Violet", "Wandering", "Winter", "Zenith",
];
const NAME_NOUNS: &[&str] = &[
    "Aurora", "Comet", "Corona", "Cosmos", "Eclipse", "Equinox", "Galaxy", "Halo", "Horizon", "Meridian", "Meteor", "Nebula", "Nova", "Orbit",
    "Parallax", "Pulsar", "Quasar", "Singularity", "Solstice", "Spiral", "Starfield", "Tide", "Vortex", "Zephyr",
];

/// A new lattice name, e.g. "Silent Quasar".
pub fn generate_lattice_name() -> String {
    use rand::seq::IndexedRandom;
    let mut rng = rand::rng();
    format!("{} {}", NAME_ADJECTIVES.choose(&mut rng).unwrap_or(&"Nameless"), NAME_NOUNS.choose(&mut rng).unwrap_or(&"Lattice"))
}

/// The lattice name to use: the configured one, else the one saved in the
/// data directory, else a new one. Returns (name, whether it was generated).
pub fn resolve_lattice_name(config: &HexConfig, keys: &crate::crypt::KeyRing) -> anyhow::Result<(String, bool)> {
    let configured = config.network.lattice_name.trim();
    if !configured.is_empty() {
        return Ok((configured.to_string(), false));
    }
    if let Some(catalog) = crate::catalog::Catalog::load(&config.storage_dir(), keys)? {
        if !catalog.lattice_name.is_empty() {
            return Ok((catalog.lattice_name, false));
        }
    }
    Ok((generate_lattice_name(), true))
}

/// Where a hex's name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSource {
    /// `identity.name` in the config.
    Configured,
    /// Saved in the data directory by an earlier start.
    Saved,
    /// Drawn from the built-in list just now.
    Generated,
}

/// This hex's identity: the ID and name saved in the data directory, or new
/// ones. A configured `identity.name` always wins and is saved; otherwise the
/// name picked on the first start is kept.
pub fn resolve_hex_identity(config: &HexConfig, keys: &crate::crypt::KeyRing, names: &[String]) -> anyhow::Result<(Ulid, String, NameSource)> {
    let configured = if config.identity.name.trim().is_empty() {
        None
    } else {
        Some(crate::config::validate_hex_name(&config.identity.name).map_err(|e| anyhow::anyhow!("{}", e))?)
    };
    let catalog = crate::catalog::Catalog::load(&config.storage_dir(), keys)?;
    let (saved_id, saved_name) = match &catalog {
        Some(c) => (c.hex_id.as_str(), c.hex_name.as_str()),
        None => ("", ""),
    };
    Ok(choose_hex_identity(configured.as_deref(), saved_id, saved_name, names))
}

/// The pure part of `resolve_hex_identity`.
pub fn choose_hex_identity(configured: Option<&str>, saved_id: &str, saved_name: &str, names: &[String]) -> (Ulid, String, NameSource) {
    let id = Ulid::from_string(saved_id).unwrap_or_else(|_| Ulid::new());
    let saved_name = saved_name.trim();
    match configured {
        Some(name) => (id, name.to_string(), NameSource::Configured),
        None if !saved_name.is_empty() => (id, saved_name.to_string(), NameSource::Saved),
        None => (id, HexDBEngine::pick_random_name(names).unwrap_or_else(|| "Unnamed Hex".to_string()), NameSource::Generated),
    }
}

/// Better candidates sort first: more RAM, then more disk, then lower ID.
fn rank(a: &PeerHex, b: &PeerHex) -> Ordering {
    b.ram_mb.cmp(&a.ram_mb).then(b.disk_mb.cmp(&a.disk_mb)).then(a.id.cmp(&b.id))
}

/// Decide this hex's role from its own identity and the live peers. Pure, so
/// every hex with the same view reaches the same answer.
pub fn elect(local: &PeerHex, live_peers: &[PeerHex]) -> String {
    let all: Vec<&PeerHex> = std::iter::once(local).chain(live_peers.iter()).collect();
    let mut candidates: Vec<&PeerHex> = all.iter().copied().filter(|h| h.preference == "auto" || h.preference == "overseer").collect();
    if candidates.iter().any(|h| h.preference == "overseer") {
        candidates.retain(|h| h.preference == "overseer");
    }

    // A sitting Overseer keeps the role; among several, the best-ranked wins.
    let sitting: Vec<&PeerHex> = candidates.iter().copied().filter(|h| h.role == ROLE_OVERSEER).collect();
    let pool = if sitting.is_empty() { candidates } else { sitting };
    let leader = pool.into_iter().min_by(|a, b| rank(a, b));

    match leader {
        Some(leader) if leader.id == local.id => ROLE_OVERSEER.into(),
        _ if local.preference == "replicant" => ROLE_REPLICANT.into(),
        _ => ROLE_HARVESTER.into(),
    }
}

/// Re-probe peers every `interval`, track their liveness, and re-run the election.
pub fn spawn_discovery_task(engine: Arc<HexDBEngine>, interval: Duration, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 Discovery task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {}
            }
            discovery_round(&engine, interval).await;
        }
    });
}

/// One discovery round: probe, update members, and re-elect.
pub async fn discovery_round(engine: &HexDBEngine, interval: Duration) {
    let found = discover_peers(&engine.config, &engine.id.to_string(), &engine.name).await;
    let now = Utc::now();
    let lost_after = chrono::Duration::from_std(interval * LOST_AFTER_ROUNDS).unwrap_or_default();
    let forget_after = chrono::Duration::from_std(interval * FORGET_AFTER_ROUNDS).unwrap_or_default();

    let live: Vec<PeerHex> = {
        let mut members = engine.peers.lock().await;
        for peer in found {
            match members.iter_mut().find(|m| m.hex.id == peer.id) {
                Some(member) => {
                    if member.status != "active" {
                        info!("🤝 Hex '{}' ({}) is back.", peer.name, peer.role);
                    }
                    member.hex = peer;
                    member.last_seen = now;
                    member.status = "active".into();
                }
                None => {
                    info!("🤝 Discovered hex '{}' ({}) at {}.", peer.name, peer.role, peer.api_endpoint);
                    members.push(LatticeMember { hex: peer, last_seen: now, status: "active".into() });
                }
            }
        }
        for member in members.iter_mut() {
            if member.status == "active" && now - member.last_seen > lost_after {
                warn!("⚠️ Lost contact with hex '{}'.", member.hex.name);
                member.status = "lost".into();
            }
        }
        members.retain(|m| now - m.last_seen <= forget_after);
        members.iter().filter(|m| m.status == "active").map(|m| m.hex.clone()).collect()
    };

    let local = local_identity(engine).await;
    let role = elect(&local, &live);
    if role != local.role {
        info!("🎖️ Role changed: {} -> {}.", local.role, role);
        engine.set_role(&role);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(id: &str, role: &str, preference: &str, ram_mb: u64) -> PeerHex {
        PeerHex {
            id: id.into(),
            name: id.into(),
            role: role.into(),
            lattice: "l".into(),
            ip: String::new(),
            api_endpoint: String::new(),
            ram_mb,
            disk_mb: 100,
            preference: preference.into(),
            last_seq: 0,
            version: String::new(),
            replication_state: String::new(),
            applied_seq: 0,
            tls: false,
        }
    }

    fn names() -> Vec<String> {
        vec!["Lion".to_string(), "Beagle".to_string()]
    }

    #[test]
    fn configured_name_wins_and_saved_id_is_kept() {
        let saved = Ulid::new();
        let (id, name, source) = choose_hex_identity(Some("Great Dane"), &saved.to_string(), "Lion", &names());
        assert_eq!(id, saved);
        assert_eq!(name, "Great Dane");
        assert_eq!(source, NameSource::Configured);
    }

    #[test]
    fn saved_name_is_reused() {
        let (_, name, source) = choose_hex_identity(None, "", "Ocelot", &names());
        assert_eq!(name, "Ocelot");
        assert_eq!(source, NameSource::Saved);
    }

    #[test]
    fn fresh_hex_draws_a_name_and_a_new_id() {
        let (id, name, source) = choose_hex_identity(None, "not a ulid", "  ", &names());
        assert!(names().contains(&name));
        assert_eq!(source, NameSource::Generated);
        assert_ne!(id.to_string(), "not a ulid");
    }

    #[test]
    fn alone_a_hex_leads() {
        assert_eq!(elect(&hex("a", "", "auto", 1), &[]), ROLE_OVERSEER);
    }

    #[test]
    fn ranks_by_ram_then_id() {
        let big = hex("b", "", "auto", 4096);
        let small = hex("a", "", "auto", 1024);
        assert_eq!(elect(&big, std::slice::from_ref(&small)), ROLE_OVERSEER);
        assert_eq!(elect(&small, &[big]), ROLE_HARVESTER);
        // Tie on resources: lower ID wins.
        let a = hex("a", "", "auto", 1024);
        let b = hex("b", "", "auto", 1024);
        assert_eq!(elect(&a, std::slice::from_ref(&b)), ROLE_OVERSEER);
        assert_eq!(elect(&b, &[a]), ROLE_HARVESTER);
    }

    #[test]
    fn sitting_overseer_keeps_the_role() {
        let sitting = hex("z", ROLE_OVERSEER, "auto", 512);
        let bigger_newcomer = hex("a", "", "auto", 8192);
        assert_eq!(elect(&bigger_newcomer, std::slice::from_ref(&sitting)), ROLE_HARVESTER);
        assert_eq!(elect(&sitting, &[bigger_newcomer]), ROLE_OVERSEER);
    }

    #[test]
    fn split_brain_resolves_to_the_better_ranked() {
        let a = hex("a", ROLE_OVERSEER, "auto", 1024);
        let b = hex("b", ROLE_OVERSEER, "auto", 2048);
        assert_eq!(elect(&a, std::slice::from_ref(&b)), ROLE_HARVESTER);
        assert_eq!(elect(&b, &[a]), ROLE_OVERSEER);
    }

    #[test]
    fn preferences_are_honored() {
        let preferred = hex("z", "", "overseer", 1);
        let big = hex("a", "", "auto", 8192);
        assert_eq!(elect(&preferred, std::slice::from_ref(&big)), ROLE_OVERSEER);
        assert_eq!(elect(&big, &[preferred]), ROLE_HARVESTER);

        let replicant = hex("r", "", "replicant", 8192);
        assert_eq!(elect(&replicant, &[]), ROLE_REPLICANT, "a replicant never leads, even alone");
        let harvester = hex("h", "", "harvester", 8192);
        assert_eq!(elect(&harvester, &[]), ROLE_HARVESTER);
    }

    #[test]
    fn parses_preferences() {
        assert_eq!(parse_preference(" Overseer ").unwrap(), "overseer");
        assert!(parse_preference("boss").is_err());
    }

    #[test]
    fn rewrites_hosts() {
        assert_eq!(with_host("0.0.0.0:7700", "10.0.0.5"), "10.0.0.5:7700");
        assert!(is_local_or_wildcard(host_of("127.0.0.1:7702")));
        assert!(!is_local_or_wildcard(host_of("10.0.0.5:7702")));
    }
}
