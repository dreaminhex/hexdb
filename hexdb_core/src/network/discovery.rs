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
// when each peer was last seen, and re-runs the role election. A prober also
// tells the hex it probed its own discovery address (a signed `HEXDB_SEEN`
// line after the identity), and that hex probes it back from then on; and each
// identity lists the discovery addresses of the hexes it sees (`knows`), which
// the prober probes too. So a new hex needs only one seed address, and the
// whole lattice sees it and it sees the whole lattice.
//
// The election:
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
use tracing::{debug, error, info, warn};

pub const ROLE_OVERSEER: &str = "Overseer";
pub const ROLE_HARVESTER: &str = "Harvester";
pub const ROLE_REPLICANT: &str = "Replicant";
/// The lattice's AI hex (`identity.role = "oracle"`): it never leads and keeps
/// no user data. It answers the AI questions (MCP, the chat, A2A) the other
/// hexes pass to it and reads what they need from the hexes that hold the
/// data, so AI traffic and its waiting stay off them (see `hexdb_api::oracle`).
pub const ROLE_ORACLE: &str = "Oracle";

/// A role with its article, for messages: "an Overseer", "a Harvester".
pub fn a_role(role: &str) -> String {
    let article = if role.starts_with(['A', 'E', 'I', 'O', 'U']) { "an" } else { "a" };
    format!("{} {}", article, role)
}

const LOCAL_PORTS: std::ops::Range<u16> = 7702..7710;
/// How long to wait for one peer to connect and identify itself. Generous: a
/// hex on a throttled shared CPU is slow, not gone, and probes run in parallel.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a probe's answer may take to build before the last round's
/// identity is sent instead (see `answer_identity`).
const FRESH_IDENTITY_TIMEOUT: Duration = Duration::from_millis(250);
/// At most this many addresses learned from probers (see `HEXDB_SEEN`).
const MAX_LEARNED: usize = 64;
/// A peer that didn't answer this many consecutive rounds is marked lost...
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
    /// The lattice configuration version in effect, its checksum, and
    /// `applied`, `rejected` or `default` (see `crate::lattice_config`).
    #[serde(default)]
    pub config_version: u64,
    #[serde(default)]
    pub config_sha256: String,
    #[serde(default)]
    pub config_state: String,
    /// Signs its lattice API responses (HexDB 2.0 and later): other hexes
    /// then refuse unsigned answers from it.
    #[serde(default)]
    pub signs_responses: bool,
    /// Signs its JSON answers to writes and reads other hexes forward to it
    /// (2.4.4 and later): a replica then refuses an unsigned or altered one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signs_forwarded: bool,
    /// `replication.slicing` and `replication.slice_copies` (the Overseer's apply).
    /// The aliases read 2.1.0 hexes, which called slices shards, during an upgrade.
    #[serde(default, alias = "sharding")]
    pub slicing: bool,
    #[serde(default, alias = "shard_copies")]
    pub slice_copies: usize,
    /// The slices this hex keeps (all of them unless it's a sliced replica).
    #[serde(default = "all_slices", alias = "shards_held")]
    pub slices_held: crate::slicing::SliceMask,
    /// The history (Overseer) this hex's data follows: its applied_seq counts in it.
    #[serde(default)]
    pub replication_source: String,
    /// The Fly.io Machine the hex runs on (`FLY_MACHINE_ID`), for hosted rolling updates.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_id: String,
    /// Where the hex runs (`identity.region`, or Fly.io's region).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    /// The discovery addresses of the hexes this one sees (see `learn`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knows: Vec<String>,
    /// A streaming replica's lag in time (see `ReplicationStatus::lag_ms`), so
    /// every hex can show every replica's (2.3.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<u64>,
    /// When this hex's discovery listener started (epoch ms; 0 from hexes
    /// before 2.5, which count as listening since long ago). Hexes started
    /// together settle on the one that listened first (see `settle_start`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub listening_since: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// When this process's discovery listener started (0 until it has).
static LISTENING_SINCE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// For a hex taking its second look at startup: of the hexes claiming the
/// Overseer role (itself included), only the one whose discovery listener
/// started first keeps the claim, so every hex settles on the same one. A
/// later starter always sees it (it looks after starting its own listener,
/// so after that one's); the first may not see the later ones, and doesn't
/// need to. Returns `local` and `peers` with the other claims dropped.
pub fn settle_start(mut local: PeerHex, mut peers: Vec<PeerHex>) -> (PeerHex, Vec<PeerHex>) {
    let first = std::iter::once(&local)
        .chain(peers.iter())
        .filter(|h| h.role == ROLE_OVERSEER)
        .min_by(|a, b| a.listening_since.cmp(&b.listening_since).then(a.id.cmp(&b.id)))
        .map(|h| h.id.clone());
    let Some(first) = first else { return (local, peers) };
    if local.role == ROLE_OVERSEER && local.id != first {
        local.role = String::new();
    }
    for peer in peers.iter_mut().filter(|p| p.role == ROLE_OVERSEER && p.id != first) {
        peer.role = String::new();
    }
    (local, peers)
}

fn all_slices() -> crate::slicing::SliceMask {
    crate::slicing::ALL_SLICES
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
    /// Consecutive rounds, run on time, in which it didn't answer.
    #[serde(skip)]
    pub missed: u32,
}

/// Normalize and check a configured role preference.
pub fn parse_preference(value: &str) -> anyhow::Result<String> {
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "auto" | "overseer" | "harvester" | "replicant" | "oracle" => Ok(lower),
        _ => anyhow::bail!("identity.role must be auto, overseer, harvester, replicant or oracle (got '{}')", value),
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
    let lattice_config = engine.lattice_config();
    let replication = if engine.is_writable() {
        ("leading".to_string(), engine.changes.published_seq(), None)
    } else {
        let status = engine.replication.lock().unwrap();
        (status.state.clone(), status.applied_seq, status.lag_ms(chrono::Utc::now()))
    };
    let (slices_held, replication_source) = {
        let status = engine.replication.lock().unwrap();
        (status.slices_held, if engine.is_writable() { engine.history_id() } else { status.history.clone().unwrap_or_default() })
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
        // An Overseer handing over its role stops being a candidate once its replicas caught up.
        preference: if crate::updates::withdrawn() {
            "replicant".into()
        } else {
            parse_preference(&config.identity.role).unwrap_or_else(|_| "auto".into())
        },
        last_seq: engine.stats().await.next_seq.saturating_sub(1),
        version: engine.version.clone(),
        replication_state: replication.0,
        applied_seq: replication.1,
        tls: config.tls.enabled(),
        config_version: lattice_config.version,
        config_sha256: lattice_config.sha256.clone(),
        config_state: lattice_config.state().to_string(),
        signs_responses: true,
        signs_forwarded: true,
        slicing: config.replication.slicing,
        slice_copies: config.replication.slice_copies.max(1),
        slices_held,
        replication_source,
        machine_id: crate::updates::machine_id(),
        region: config.identity.effective_region(),
        knows: sees(&engine.peers.lock().await),
        lag_ms: replication.2,
        listening_since: LISTENING_SINCE.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Discovery handshakes served at once.
const MAX_HANDSHAKES: usize = 64;

/// Answer discovery handshakes with this hex's current identity.
/// Bind the discovery endpoint. The server does this at startup and stops if
/// it can't: a hex that can't be reached can't take part in its lattice
/// (no replication, elections or failover), so running on would only hide it.
pub async fn bind_discovery(addr: &str) -> anyhow::Result<std::net::TcpListener> {
    TcpListener::bind(addr)
        .await
        .and_then(|l| l.into_std())
        .map_err(|e| anyhow::anyhow!("Failed to bind the discovery endpoint {} (network.discovery_endpoint): {}", addr, e))
}

pub async fn start_discovery_listener(engine: Arc<HexDBEngine>, listener: Arc<TcpListener>) {
    let addr = engine.config.network.discovery_endpoint.clone();
    info!(%addr, "📡 Discovery listener active.");

    // Handshakes are short; strangers holding connections open can't use more than this many.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
    loop {
        match listener.accept().await {
            Ok((mut socket, _)) => {
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    debug!("📡 Dropped a discovery connection: too many handshakes at once.");
                    continue;
                };
                let engine = engine.clone();
                tokio::spawn(async move {
                    let _slot = slot;
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
                    if !main_runtime_alive() {
                        warn!("⏱️ Not answering discovery: this hex's main runtime hasn't run for over {} s.", STUCK_AFTER_SECONDS);
                        return;
                    }
                    let identity = answer_identity(&engine).await;
                    let Ok(json) = serde_json::to_string(&identity) else { return };
                    let reply = engine.lattice_keys.identity_reply(key_index, &nonce, &json);
                    if socket.write_all(format!("{}\n", reply).as_bytes()).await.is_err() {
                        return;
                    }
                    // The prober's own address, so this hex probes it back. Hexes
                    // before 2.3.3 don't send one and close the connection instead.
                    let mut line = String::new();
                    let read = tokio::time::timeout(PROBE_TIMEOUT, BufReader::new(&mut socket).take(512).read_line(&mut line)).await;
                    if matches!(read, Ok(Ok(n)) if n > 0) {
                        if let Some(addr) = engine.lattice_keys.check_seen(key_index, &nonce, &line) {
                            learn(&engine, addr, socket.peer_addr().ok());
                        }
                    }
                });
            }
            Err(e) => warn!(%e, "❗ Discovery accept failed."),
        }
    }
}

/// Remember a hex's discovery address, to probe it in the next rounds: a
/// prober's own (`from` is where it connected from: a loopback or wildcard
/// host is reached there), or one a peer knows.
fn learn(engine: &HexDBEngine, addr: &str, from: Option<std::net::SocketAddr>) {
    let mut addr = addr.to_string();
    if is_local_or_wildcard(host_of(&addr)) {
        let Some(from) = from else {
            // A loopback address a peer knows is on its machine, which may be this one.
            if !matches!(host_of(&addr), "0.0.0.0" | "[::]") {
                remember(engine, addr);
            }
            return;
        };
        if !from.ip().is_loopback() {
            let ip = match from.ip() {
                std::net::IpAddr::V4(v4) => v4.to_string(),
                std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_else(|| format!("[{}]", v6)),
            };
            addr = with_host(&addr, &ip);
        } else if matches!(host_of(&addr), "0.0.0.0" | "[::]") {
            addr = with_host(&addr, "127.0.0.1");
        }
    }
    remember(engine, addr);
}

fn remember(engine: &HexDBEngine, addr: String) {
    let mut learned = engine.learned_peers.lock().unwrap();
    if learned.len() >= MAX_LEARNED && !learned.contains_key(&addr) {
        return;
    }
    if learned.insert(addr.clone(), std::time::Instant::now()).is_none() {
        debug!(%addr, "📡 Learned of a hex; probing it from now on.");
    }
}

/// The identity to answer a probe with: the last round's, with the current
/// role, so answering never waits for anything the rest of the hex is busy
/// with; before the first round, a fresh one. A busy hex stays visible to its
/// lattice instead of looking gone.
async fn answer_identity(engine: &HexDBEngine) -> PeerHex {
    let cached = engine.identity_cache.read().unwrap().clone();
    match cached {
        Some(mut identity) => {
            identity.role = engine.role();
            if crate::updates::withdrawn() {
                identity.preference = "replicant".into();
            }
            identity
        }
        None => match tokio::time::timeout(FRESH_IDENTITY_TIMEOUT * 20, local_identity(engine)).await {
            Ok(fresh) => fresh,
            Err(_) => local_identity(engine).await,
        },
    }
}

/// When the main runtime last ran its heartbeat (epoch seconds; 0: not yet).
static HEARTBEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// The lifeline stops answering once the heartbeat is this old: the hex is stuck, not just busy.
const STUCK_AFTER_SECONDS: i64 = 60;

/// Answer discovery from a thread of its own (the lifeline), with its own
/// small runtime, so a hex whose main runtime is saturated (a throttled CPU,
/// a burst of work) still answers its lattice and isn't voted out for being
/// slow. It answers only while the main runtime's heartbeat, ticked every
/// second, is under a minute old: a hex that's truly stuck still drops out.
/// Call it from the main runtime, which runs the heartbeat.
pub fn spawn_discovery_lifeline(engine: Arc<HexDBEngine>, listener: std::net::TcpListener) -> anyhow::Result<()> {
    tokio::spawn(async {
        loop {
            HEARTBEAT.store(Utc::now().timestamp(), std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    listener.set_nonblocking(true)?;
    LISTENING_SINCE.store(Utc::now().timestamp_millis(), std::sync::atomic::Ordering::Relaxed);
    std::thread::Builder::new().name("hexdb-lifeline".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                error!("❌ The discovery lifeline couldn't start: {}", e);
                return;
            }
        };
        let listener = match runtime.block_on(async { TcpListener::from_std(listener) }) {
            Ok(listener) => Arc::new(listener),
            Err(e) => {
                error!("❌ The discovery lifeline couldn't take the discovery socket: {}", e);
                return;
            }
        };
        // Like a supervised task: started again if it ever panics.
        loop {
            let run = std::panic::AssertUnwindSafe(start_discovery_listener(engine.clone(), listener.clone()));
            if let Err(panic) = runtime.block_on(futures::FutureExt::catch_unwind(run)) {
                let message = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
                error!("❌ The discovery lifeline panicked ({}); restarting it.", message);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    })?;
    Ok(())
}

/// False once the main runtime's heartbeat is a minute old (see `spawn_discovery_lifeline`).
fn main_runtime_alive() -> bool {
    let beat = HEARTBEAT.load(std::sync::atomic::Ordering::Relaxed);
    beat == 0 || Utc::now().timestamp() - beat < STUCK_AFTER_SECONDS
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

/// Every address each target resolves to. A peer can be a name for a group of
/// hexes (Fly.io's `<app>.internal`, a Kubernetes headless service, round-robin
/// DNS), and connecting by name would only reach one of them. Addresses stay as
/// they are; a name that doesn't resolve is kept, so its probe reports it.
async fn expand_targets(targets: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for target in targets {
        let host = host_of(&target).trim_start_matches('[').trim_end_matches(']');
        if host.parse::<std::net::IpAddr>().is_ok() {
            out.push(target);
            continue;
        }
        let resolved: Vec<String> = match tokio::net::lookup_host(target.as_str()).await {
            Ok(addrs) => addrs.map(|a| a.to_string()).collect(),
            Err(_) => Vec::new(),
        };
        if resolved.is_empty() {
            out.push(target);
        } else {
            out.extend(resolved);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Send an authenticated hello to one address and verify the reply. A hex
/// that doesn't answer the current key is tried with previous keys (a
/// lattice part-way through a secret rotation).
async fn probe_peer(addr: String, keys: LatticeKeys, me: String) -> Option<PeerHex> {
    let attempt = PROBE_TIMEOUT;
    for index in 0..keys.len() {
        match tokio::time::timeout(attempt, probe_with_key(&addr, &keys, index, &me)).await {
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

async fn probe_with_key(addr: &str, keys: &LatticeKeys, index: usize, me: &str) -> Probe {
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
    // Tell it where this hex is, so it probes back (see `learn`).
    if !me.is_empty() {
        let _ = stream.write_all(format!("{}\n", keys.seen_line(index, &nonce, me)).as_bytes()).await;
    }
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
    discover_peers_with(config, local_id, local_name, Vec::new()).await
}

/// `discover_peers`, also probing `learned` addresses (hexes that probed this one).
async fn discover_peers_with(config: &HexConfig, local_id: &str, local_name: &str, learned: Vec<String>) -> Vec<PeerHex> {
    let keys = match config.lattice_keys() {
        Ok(keys) => LatticeKeys::with_previous(&keys),
        Err(e) => {
            warn!("❗ Discovery is disabled: {:#}", e);
            return Vec::new();
        }
    };
    let me = advertised(config, &config.network.discovery_endpoint);
    let mut targets = probe_targets(config);
    targets.extend(learned.into_iter().filter(|t| *t != me));
    targets.sort();
    targets.dedup();
    let probes = expand_targets(targets).await.into_iter().map(|addr| {
        let keys = keys.clone();
        let me = me.clone();
        let budget = PROBE_TIMEOUT * keys.len() as u32;
        async move { tokio::time::timeout(budget, probe_peer(addr, keys, me)).await.ok().flatten() }
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
    let id = Ulid::from_string(saved_id).unwrap_or_else(|_| Ulid::generate());
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

    // A sitting Overseer keeps the role, whatever the others prefer: a hex that
    // took it would lack the writes the sitting one accepted, and the sitting one
    // would then copy the taker's data and drop them. A hex that prefers to lead
    // gets the role back through a handover once it has caught up
    // (`updates::hand_over_to_preferred`). Among several sitting Overseers (two
    // sides of a healed partition), a preferred one, then the best-ranked, wins.
    let sitting: Vec<&PeerHex> = candidates.iter().copied().filter(|h| h.role == ROLE_OVERSEER).collect();
    if sitting.is_empty() && candidates.iter().any(|h| h.preference == "overseer") {
        candidates.retain(|h| h.preference == "overseer");
    }
    let leader = if !sitting.is_empty() {
        sitting.into_iter().min_by(|a, b| (b.preference == "overseer").cmp(&(a.preference == "overseer")).then(rank(a, b)))
    } else if local.slicing {
        // Sliced replicas each hold part of the data; the one furthest along
        // the old Overseer's history rebuilds the rest most completely. Among
        // those equally far along, the one holding the most slices has the
        // least to collect (none, if it holds them all), so writes resume sooner.
        candidates.into_iter().min_by(|a, b| {
            b.applied_seq
                .cmp(&a.applied_seq)
                .then(crate::slicing::count(b.slices_held).cmp(&crate::slicing::count(a.slices_held)))
                .then(rank(a, b))
        })
    } else {
        candidates.into_iter().min_by(|a, b| rank(a, b))
    };

    match leader {
        Some(leader) if leader.id == local.id => ROLE_OVERSEER.into(),
        _ if local.preference == "replicant" => ROLE_REPLICANT.into(),
        _ if local.preference == "oracle" => ROLE_ORACLE.into(),
        _ => ROLE_HARVESTER.into(),
    }
}

/// Re-probe peers every `interval`, track their liveness, and re-run the election.
pub fn spawn_discovery_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("discovery", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let mut last_round = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        debug!("🛑 Discovery task is shutting down...");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {}
                }
                // A round that starts much later than planned means this hex was
                // starved of CPU: peers that "didn't answer" may simply not have
                // been asked in time, so this round doesn't count against them.
                let on_time = last_round.elapsed() <= interval * 2 + PROBE_TIMEOUT;
                if !on_time {
                    warn!("⏱️ This hex's discovery round ran {:.1} s late (the hex is short of CPU); peers that miss it aren't counted as lost.", (last_round.elapsed() - interval).as_secs_f64());
                }
                last_round = std::time::Instant::now();
                discovery_round(&engine, interval, on_time).await;
                // Its own task: a handover waits for the replicas, and discovery must keep running meanwhile.
                let handing = engine.clone();
                tokio::spawn(async move { crate::updates::hand_over_to_preferred(&handing).await });
            }
        }
    });
}

/// Run a discovery round on this hex and ask every active peer to run one now,
/// so a change of candidates (an Overseer handing over) is decided in a
/// moment instead of at the next scheduled rounds. These extra rounds count no
/// missed answers, so a slow peer isn't declared lost sooner.
pub async fn elect_everywhere(engine: &HexDBEngine) {
    let peers: Vec<PeerHex> = engine.peers.lock().await.iter().filter(|p| p.status == "active").map(|p| p.hex.clone()).collect();
    let nudges = peers.iter().map(|peer| async move {
        let asked = tokio::time::timeout(Duration::from_secs(2), crate::replication::post_to_peer(engine, peer, "/lattice/elect", &serde_json::json!({}))).await;
        if !matches!(asked, Ok(Ok(()))) {
            debug!("📡 Couldn't ask '{}' to re-elect now; it will at its next round.", peer.name);
        }
    });
    futures::future::join_all(nudges).await;
    elect_now(engine).await;
}

/// One discovery round now, outside the schedule (counting no missed answers).
pub async fn elect_now(engine: &HexDBEngine) {
    let interval = Duration::from_secs(engine.config.network.discovery_interval_seconds.max(1));
    discovery_round(engine, interval, false).await;
}

/// One discovery round: probe, update members, and re-elect. A peer is lost
/// after `LOST_AFTER_ROUNDS` consecutive rounds it didn't answer; rounds that
/// weren't `on_time` (this hex was starved) count no misses. And a peer that
/// another hex answering this round still sees isn't lost yet: one hex cut off
/// from the Overseer (a starved CPU, a broken link) can't elect itself alone
/// and split the lattice. When the Overseer is really gone, the others stop
/// seeing it within a round, so failover waits at most one more round.
pub async fn discovery_round(engine: &HexDBEngine, interval: Duration, on_time: bool) {
    let started = std::time::Instant::now();
    // Addresses learned from probers, until they stop probing for as long as a lost peer is kept.
    let learned: Vec<String> = {
        let mut learned = engine.learned_peers.lock().unwrap();
        learned.retain(|_, heard| heard.elapsed() <= interval * FORGET_AFTER_ROUNDS);
        learned.keys().cloned().collect()
    };
    let found = discover_peers_with(&engine.config, &engine.id.to_string(), &engine.name, learned).await;
    // Probing itself took far longer than its timeout allows: this hex was starved while asking.
    let on_time = on_time && started.elapsed() <= PROBE_TIMEOUT * 2 + Duration::from_secs(1);
    let now = Utc::now();
    let forget_after = chrono::Duration::from_std(interval * FORGET_AFTER_ROUNDS).unwrap_or_default();

    let live: Vec<PeerHex> = {
        let mut members = engine.peers.lock().await;
        let answered: Vec<String> = found.iter().map(|p| p.id.clone()).collect();
        // Who the hexes that answered still see.
        let seen_by_others: std::collections::HashSet<String> = found.iter().flat_map(|p| p.knows.iter().cloned()).collect();
        for peer in &found {
            for addr in &peer.knows {
                learn(engine, addr, None);
            }
        }
        for peer in found {
            match members.iter_mut().find(|m| m.hex.id == peer.id) {
                Some(member) => {
                    if member.status != "active" {
                        info!("🤝 Hex '{}' ({}) is back.", peer.name, peer.role);
                    }
                    member.hex = peer;
                    member.last_seen = now;
                    member.status = "active".into();
                    member.missed = 0;
                }
                None => {
                    info!("🤝 Discovered hex '{}' ({}) at {}.", peer.name, peer.role, peer.api_endpoint);
                    members.push(LatticeMember { hex: peer, last_seen: now, status: "active".into(), missed: 0 });
                }
            }
        }
        count_misses(&mut members, &answered, &seen_by_others, on_time);
        members.retain(|m| now - m.last_seen <= forget_after);
        let active: Vec<PeerHex> = members.iter().filter(|m| m.status == "active").map(|m| m.hex.clone()).collect();
        // The count the quorum check uses while the list is busy.
        engine.active_peers_seen.store(active.len(), std::sync::atomic::Ordering::Relaxed);
        active
    };

    let local = local_identity(engine).await;
    *engine.identity_cache.write().unwrap() = Some(local.clone());
    let role = elect(&local, &live);
    if role != local.role {
        info!("🎖️ Role changed: {} -> {}.", local.role, role);
        engine.set_role(&role);
    }
}

/// The discovery addresses of the members this hex has heard from within the
/// last `LOST_AFTER_ROUNDS` rounds. A member kept active only because others
/// still saw it isn't listed, so two hexes that have both lost the Overseer
/// don't keep vouching for it to each other.
fn sees(members: &[LatticeMember]) -> Vec<String> {
    members.iter().filter(|m| m.status == "active" && m.missed < LOST_AFTER_ROUNDS).map(|m| m.hex.ip.clone()).collect()
}

/// Count a missed round against every member that didn't answer (unless
/// this hex was starved), and mark lost those past `LOST_AFTER_ROUNDS` that
/// no hex that answered still sees.
fn count_misses(members: &mut [LatticeMember], answered: &[String], seen_by_others: &std::collections::HashSet<String>, on_time: bool) {
    for member in members.iter_mut().filter(|m| !answered.contains(&m.hex.id)) {
        if on_time {
            member.missed += 1;
        }
        if member.status == "active" && member.missed >= LOST_AFTER_ROUNDS {
            if seen_by_others.contains(&member.hex.ip) {
                if member.missed == LOST_AFTER_ROUNDS {
                    warn!("⚠️ Hex '{}' hasn't answered for {} rounds, but other hexes still see it: not counting it lost yet.", member.hex.name, member.missed);
                }
                continue;
            }
            warn!("⚠️ Lost contact with hex '{}' ({} rounds without an answer).", member.hex.name, member.missed);
            member.status = "lost".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_others_still_see_is_not_lost_yet() {
        let mut overseer = hex("o", ROLE_OVERSEER, "auto", 1024);
        overseer.ip = "10.0.0.1:7702".into();
        let mut members = vec![LatticeMember { hex: overseer, last_seen: Utc::now(), status: "active".into(), missed: 0 }];
        let seen: std::collections::HashSet<String> = ["10.0.0.1:7702".to_string()].into();
        for _ in 0..5 {
            count_misses(&mut members, &[], &seen, true);
        }
        assert_eq!((members[0].status.as_str(), members[0].missed), ("active", 5), "another hex still sees it");
        // A member kept only on others' word isn't vouched for in turn.
        assert!(sees(&members).is_empty());
        // Once nobody else sees it either, it's lost at once.
        count_misses(&mut members, &[], &Default::default(), true);
        assert_eq!(members[0].status, "lost");
        // Alone (no other hex to ask), three missed rounds are enough; starved rounds count none.
        let mut alone = vec![LatticeMember { hex: hex("p", ROLE_HARVESTER, "auto", 1024), last_seen: Utc::now(), status: "active".into(), missed: 0 }];
        for _ in 0..4 {
            count_misses(&mut alone, &[], &Default::default(), false);
        }
        assert_eq!(alone[0].status, "active");
        for _ in 0..3 {
            count_misses(&mut alone, &[], &Default::default(), true);
        }
        assert_eq!(alone[0].status, "lost");
    }

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
            config_version: 0,
            config_sha256: String::new(),
            config_state: String::new(),
            signs_responses: false,
            signs_forwarded: false,
            slicing: false,
            slice_copies: 1,
            slices_held: crate::slicing::ALL_SLICES,
            replication_source: String::new(),
            machine_id: String::new(),
            region: String::new(),
            knows: Vec::new(),
            lag_ms: None,
            listening_since: 0,
        }
    }

    #[test]
    fn sliced_lattices_elect_the_most_up_to_date_replica() {
        let mut a = hex("a", ROLE_HARVESTER, "auto", 8000);
        let mut b = hex("b", ROLE_HARVESTER, "auto", 1000);
        a.applied_seq = 10;
        b.applied_seq = 50;
        // Without slicing, RAM decides; with it, the furthest replica leads.
        assert_eq!(elect(&a, std::slice::from_ref(&b)), ROLE_OVERSEER);
        a.slicing = true;
        assert_eq!(elect(&a, std::slice::from_ref(&b)), ROLE_HARVESTER);
        // A sitting Overseer keeps the role regardless.
        let overseer = hex("c", ROLE_OVERSEER, "auto", 10);
        assert_eq!(elect(&a, &[b, overseer]), ROLE_HARVESTER);
    }

    #[test]
    fn a_sitting_overseer_keeps_the_role_from_a_hex_that_prefers_to_lead() {
        // The preferred hex is back after a handover: it follows; the sitting one hands back later.
        let sitting = hex("sitting", ROLE_OVERSEER, "auto", 1000);
        let preferred = hex("preferred", ROLE_HARVESTER, "overseer", 8000);
        assert_eq!(elect(&preferred, std::slice::from_ref(&sitting)), ROLE_HARVESTER);
        assert_eq!(elect(&sitting, std::slice::from_ref(&preferred)), ROLE_OVERSEER);
        // With no Overseer sitting, the preferred hex is elected.
        let follower = hex("sitting", ROLE_HARVESTER, "auto", 1000);
        assert_eq!(elect(&preferred, std::slice::from_ref(&follower)), ROLE_OVERSEER);
        // Two sitting (a healed partition): the preferred one keeps it.
        let both = hex("preferred", ROLE_OVERSEER, "overseer", 10);
        assert_eq!(elect(&both, std::slice::from_ref(&sitting)), ROLE_OVERSEER);
        assert_eq!(elect(&sitting, std::slice::from_ref(&both)), ROLE_HARVESTER);
    }

    #[test]
    fn sliced_lattices_prefer_the_hex_with_the_most_slices_among_the_most_up_to_date() {
        // Equally far along: the hex holding every slice leads, though it has less RAM.
        let mut partial = hex("partial", ROLE_HARVESTER, "auto", 8000);
        let mut full = hex("full", ROLE_HARVESTER, "auto", 1000);
        partial.slicing = true;
        full.slicing = true;
        partial.applied_seq = 50;
        full.applied_seq = 50;
        partial.slices_held = (1u64 << 41) - 1;
        full.slices_held = crate::slicing::ALL_SLICES;
        assert_eq!(elect(&partial, std::slice::from_ref(&full)), ROLE_HARVESTER);
        assert_eq!(elect(&full, std::slice::from_ref(&partial)), ROLE_OVERSEER);
        // Further along still wins: electing a hex that's behind would lose acknowledged writes.
        partial.applied_seq = 51;
        assert_eq!(elect(&partial, std::slice::from_ref(&full)), ROLE_OVERSEER);
    }

    #[tokio::test]
    async fn peer_names_expand_to_every_address() {
        let targets = expand_targets(vec!["localhost:7702".into(), "10.0.0.5:7702".into(), "no-such-host.invalid:7702".into()]).await;
        assert!(targets.contains(&"127.0.0.1:7702".to_string()), "{:?}", targets);
        assert!(targets.contains(&"10.0.0.5:7702".to_string()));
        // A name that doesn't resolve stays, so its probe reports it.
        assert!(targets.contains(&"no-such-host.invalid:7702".to_string()));
        assert!(!targets.contains(&"localhost:7702".to_string()));
    }

    #[test]
    fn announcements_from_2_1_0_hexes_still_read() {
        // 2.1.0 called slices shards; during an upgrade its announcements must still be understood.
        let mut old = serde_json::to_value(hex("a", ROLE_HARVESTER, "auto", 100)).unwrap();
        let fields = old.as_object_mut().unwrap();
        for field in ["slicing", "slice_copies", "slices_held"] {
            fields.remove(field);
        }
        fields.insert("sharding".into(), true.into());
        fields.insert("shard_copies".into(), 2.into());
        fields.insert("shards_held".into(), 0b1011u64.into());
        let read: PeerHex = serde_json::from_value(old).unwrap();
        assert!(read.slicing);
        assert_eq!(read.slice_copies, 2);
        assert_eq!(read.slices_held, 0b1011);
    }

    fn names() -> Vec<String> {
        vec!["Lion".to_string(), "Beagle".to_string()]
    }

    #[test]
    fn configured_name_wins_and_saved_id_is_kept() {
        let saved = Ulid::generate();
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

        // The Oracle never leads, even as the biggest hex or one that led before.
        let oracle = hex("o", ROLE_OVERSEER, "oracle", 65536);
        let small = hex("s", "", "auto", 512);
        assert_eq!(elect(&oracle, std::slice::from_ref(&small)), ROLE_ORACLE);
        assert_eq!(elect(&small, std::slice::from_ref(&oracle)), ROLE_OVERSEER);
        assert_eq!(parse_preference("Oracle").unwrap(), "oracle");
    }

    #[test]
    fn hexes_started_together_settle_on_the_first_to_listen() {
        let claim = |id: &str, since: i64| PeerHex { listening_since: since, ..hex(id, ROLE_OVERSEER, "auto", 1024) };
        // The later starter sees the earlier one's claim and drops its own, whatever their sizes.
        let early = claim("a", 1_000);
        let late = PeerHex { ram_mb: 65536, ..claim("b", 2_000) };
        let (local, peers) = settle_start(late.clone(), vec![early.clone()]);
        assert_eq!(elect(&local, &peers), ROLE_HARVESTER);
        // The earlier one, if it sees the later, keeps the role.
        let (local, peers) = settle_start(early.clone(), vec![late.clone()]);
        assert_eq!(elect(&local, &peers), ROLE_OVERSEER);
        // An Overseer from before 2.5 (no time) counts as listening first; same times fall to the ID.
        let old = claim("z", 0);
        let (local, peers) = settle_start(early.clone(), vec![old]);
        assert_eq!(elect(&local, &peers), ROLE_HARVESTER);
        let (local, peers) = settle_start(claim("b", 1_000), vec![claim("a", 1_000)]);
        assert_eq!(elect(&local, &peers), ROLE_HARVESTER);
        // Nobody claiming: nothing changes.
        let (local, peers) = settle_start(hex("c", "", "auto", 1), vec![hex("d", ROLE_HARVESTER, "auto", 1)]);
        assert_eq!((local.role.as_str(), peers[0].role.as_str()), ("", ROLE_HARVESTER));
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
