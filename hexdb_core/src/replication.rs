// HexDB Core Replication
//
// Hexes in a lattice keep full copies of the data. The Overseer accepts writes;
// Harvesters and Replicants are read-only replicas that follow it:
//
//   1. Full sync: read a snapshot from the Overseer (`/lattice/snapshot`):
//      its sequence number S, its tessellations and index definitions, then
//      every document with its version. Local data the Overseer doesn't have
//      is removed.
//   2. Streaming: follow the Overseer's change feed after S
//      (`/lattice/changes`, long polling), applying each batch atomically
//      together with the replication cursor.
//
// A change for a document whose snapshot version is newer than the change is
// skipped, so changes made while the snapshot was read apply exactly once.
//
// The cursor names the Overseer's history (`Catalog::history_id`, kept in its
// data directory) and a sequence number in it. After an Overseer restart the
// history is the same, so a replica resumes from its cursor, reading older
// changes from the Overseer's on-disk change history if needed. A new
// Overseer (failover) has a different history, so replicas take a full sync.
//
// Each poll for changes after N acknowledges N, which gives the Overseer each
// replica's exact position (for lag, and for `replication.min_acks`). With
// `min_acks = 0` replication is asynchronous: a write the Overseer
// acknowledged but no replica received yet is lost if the Overseer fails
// before it comes back.
//
// Slicing (`replication.slicing`, see `crate::slicing`): with two or more
// replicas, each keeps only its slices. A full sync and the change feed are
// filtered to them; when the placement changes, a replica copies the slices it
// gains from the Overseer and gives up the ones it loses once another replica
// holds them. A sliced replica also keeps the last `replication.recent_changes`
// changes of every slice. If the Overseer fails, the most up-to-date replica
// is elected; before taking requests it collects the slices it lacks from the
// other replicas and brings them to its own position with those changes.
//
// Internal endpoints require a fresh, single-use request signature made with
// the lattice key (see `network::lattice_auth`), so only hexes configured with
// the same lattice secret can replicate from each other, and captured requests
// can't be replayed. With `[tls]` configured, replication uses HTTPS.

use crate::slicing::{self, SliceMask, ALL_SLICES};
use crate::{
    catalog::TessellationInfo,
    changes::{Change, ChangeKind},
    config::HexConfig,
    document::Document,
    engine::{HexDBEngine, ReplicaCursor, ReplicaWrite, REPLICATION_TESSELLATION},
    hex::DocKey,
    network::discovery::{LatticeMember, ROLE_OVERSEER},
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use ulid::Ulid;

pub use crate::network::lattice_auth::{LATTICE_RESPONSE_HEADER, LATTICE_SIGNATURE_HEADER};
/// Documents per snapshot page and per applied batch.
const PAGE: usize = 500;
/// How long a replica long-polls the Overseer for changes.
const POLL_SECONDS: u64 = 5;
/// How often a streaming replica re-checks the Overseer's catalog (new tessellations, indexes).
const CATALOG_EVERY: Duration = Duration::from_secs(15);

/// An HTTP client for talking to other hexes: HTTPS-capable, trusting the
/// system roots plus `tls.ca_file` if set.
pub fn lattice_client(config: &HexConfig, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = crate::crypt::http_client().timeout(timeout).https_only(config.tls.enabled());
    if !config.tls.ca_file.is_empty() {
        let pem = std::fs::read(&config.tls.ca_file).with_context(|| format!("Failed to read tls.ca_file {}", config.tls.ca_file))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem).context("tls.ca_file is not valid PEM")? {
            builder = builder.add_root_certificate(cert);
        }
    }
    Ok(builder.build()?)
}

/// The base URL of another hex's API.
pub fn peer_base_url(peer: &crate::network::discovery::PeerHex) -> String {
    format!("{}://{}", if peer.tls { "https" } else { "http" }, peer.api_endpoint)
}

/// Path and query of a URL, as signed.
fn path_and_query(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match u.query() {
            Some(q) => format!("{}?{}", u.path(), q),
            None => u.path().to_string(),
        },
        Err(_) => url.to_string(),
    }
}

/// A lattice answer's body, once its signature checks out. A hex that
/// announces `signs_responses` (2.0 and later) must sign; an unsigned answer
/// is accepted only from a hex known not to (an older one, mid-upgrade).
pub(crate) async fn verified_body(engine: &HexDBEngine, url: &str, request_signature: &str, response: reqwest::Response) -> Result<Vec<u8>> {
    let status = response.status().as_u16();
    let tag = response.headers().get(LATTICE_RESPONSE_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
    let body = response.bytes().await.with_context(|| format!("reading {}", url))?;
    match tag {
        Some(tag) if engine.lattice_keys.verify_response(request_signature, status, &body, &tag) => Ok(body.to_vec()),
        Some(_) => bail!("{}: the answer's lattice signature doesn't match (changed in transit?)", url),
        None if !peer_signs(engine, url).await => Ok(body.to_vec()),
        None => bail!("{}: the answer isn't signed with the lattice key (changed in transit, or not a hex of this lattice?)", url),
    }
}

/// Whether the hex at `url` signs its answers (true when it isn't known).
async fn peer_signs(engine: &HexDBEngine, url: &str) -> bool {
    let peers = engine.peers.lock().await;
    peers.iter().find(|p| url.starts_with(&peer_base_url(&p.hex))).is_none_or(|p| p.hex.signs_responses)
}

/// POST a JSON body to the Overseer's lattice API (e.g. to forward a sign-out from a replica).
pub async fn post_to_overseer(engine: &HexDBEngine, path: &str, body: &serde_json::Value) -> Result<()> {
    let overseer = {
        let peers = engine.peers.lock().await;
        peers.iter().find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER).map(|p| p.hex.clone())
    }
    .ok_or_else(|| anyhow!("no Overseer is reachable"))?;
    let url = format!("{}{}", peer_base_url(&overseer), path);
    let bytes = serde_json::to_vec(body)?;
    let client = lattice_client(&engine.config, Duration::from_secs(10))?;
    // An Overseer that hasn't been given a new lattice secret yet accepts a previous one.
    for index in 0..engine.lattice_keys.len() {
        let signature = engine.lattice_keys.sign_request_with(index, "POST", path, &bytes);
        let response = client
            .post(&url)
            .header(LATTICE_SIGNATURE_HEADER, &signature)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes.clone())
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && index + 1 < engine.lattice_keys.len() {
            continue;
        }
        if !status.is_success() {
            bail!("POST {} returned {}", url, status);
        }
        verified_body(engine, &url, &signature, response).await?;
        return Ok(());
    }
    Ok(())
}

/// What replication is doing on this hex, for `/status` and the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct ReplicationStatus {
    /// "leading" (this hex is the Overseer), "waiting" (no Overseer reachable),
    /// "syncing" (full sync in progress), "streaming", or "error".
    pub state: String,
    /// The Overseer being followed.
    pub source_id: Option<String>,
    pub source_name: Option<String>,
    /// Overseer sequence number applied up to.
    pub applied_seq: u64,
    /// Overseer's latest published sequence number, as last seen.
    pub source_seq: u64,
    /// Documents copied by the last full sync.
    pub synced_documents: u64,
    pub last_sync: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// The slices this hex keeps (all of them unless it's a sliced replica).
    pub slices_held: crate::slicing::SliceMask,
    /// The slices it should keep, by the current placement.
    pub slices_wanted: crate::slicing::SliceMask,
    /// The history (Overseer) this hex's data follows.
    pub history: Option<String>,
    /// Set while a newly elected Overseer collects slices from replicas.
    pub rebuilding: Option<String>,
    /// While rebuilding: the hexes, not reachable now, that hold slices no reachable hex has.
    pub waiting_for: Vec<String>,
}

/// The Overseer answered 503: it's still collecting slices after a failover.
#[derive(Debug)]
struct OverseerRebuilding;

impl std::fmt::Display for OverseerRebuilding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Overseer is collecting slices after a failover")
    }
}

impl std::error::Error for OverseerRebuilding {}

/// "Carl", "Carl and Donut", "Carl, Donut and Katia".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

impl Default for ReplicationStatus {
    fn default() -> Self {
        ReplicationStatus {
            state: String::new(),
            source_id: None,
            source_name: None,
            applied_seq: 0,
            source_seq: 0,
            synced_documents: 0,
            last_sync: None,
            last_error: None,
            slices_held: crate::slicing::ALL_SLICES,
            slices_wanted: crate::slicing::ALL_SLICES,
            history: None,
            rebuilding: None,
            waiting_for: Vec::new(),
        }
    }
}

impl ReplicationStatus {
    /// Changes the Overseer has published that this hex hasn't applied.
    pub fn lag(&self) -> u64 {
        self.source_seq.saturating_sub(self.applied_seq)
    }
}

// ---------------------------------------------------------------------------
// Wire format (shared with the API handlers)
// ---------------------------------------------------------------------------

/// `GET /lattice/slice-snapshot/{tessellation}`: a replica's documents in some
/// slices, as of its position in its history (for a rebuild after a failover).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlicePage {
    pub source_id: String,
    pub applied_seq: u64,
    pub documents: Vec<Document>,
    pub next: Option<Ulid>,
}

/// `GET /lattice/snapshot`: where a full sync starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// The Overseer's history ID.
    pub source_id: String,
    pub source_name: String,
    /// The Overseer's hex ID.
    #[serde(default)]
    pub hex_id: String,
    /// The oldest position its change history can resume after.
    #[serde(default)]
    pub available_after: u64,
    /// Follow changes after this sequence number once the documents are copied.
    pub seq: u64,
    pub tessellations: Vec<(String, TessellationInfo)>,
    /// The key that names file chunks, sealed with the lattice transfer key
    /// (hex). Every hex in a lattice names a chunk the same way.
    #[serde(default)]
    pub content_key: String,
}

/// `GET /lattice/snapshot/{tessellation}`: one page of documents with versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPage {
    pub documents: Vec<(Document, u64)>,
    pub next: Option<Ulid>,
}

/// `GET /lattice/changes`: changes after a sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub source_id: String,
    pub changes: Vec<Change>,
    /// Pass back as `after`.
    pub last_seq: u64,
    /// The Overseer's latest published sequence number.
    pub published_seq: u64,
    /// A fingerprint of the Overseer's catalog (tessellations, index
    /// definitions, schemas). When it changes, the replica re-reads the
    /// catalog at once instead of waiting for its periodic check.
    #[serde(default)]
    pub catalog: String,
}

impl HexDBEngine {
    /// Fingerprint of what replicas copy from the catalog (see `ChangeBatch::catalog`).
    pub fn catalog_fingerprint(&self) -> String {
        let details: Vec<(String, crate::catalog::TessellationInfo)> =
            self.tessellation_details().into_iter().filter(|(name, _)| name != REPLICATION_TESSELLATION).collect();
        let bytes = serde_json::to_vec(&details).unwrap_or_default();
        blake3::hash(&bytes).to_hex()[..16].to_string()
    }

    /// Snapshot metadata. The sequence number is taken first, so every change
    /// after it is either in the documents read afterwards or in the feed.
    pub fn snapshot_meta(&self) -> SnapshotMeta {
        let seq = self.changes.published_seq();
        SnapshotMeta {
            source_id: self.history_id(),
            source_name: self.name.clone(),
            hex_id: self.id.to_string(),
            available_after: self.history_available_after(),
            seq,
            tessellations: self.tessellation_details().into_iter().filter(|(name, _)| name != REPLICATION_TESSELLATION).collect(),
            content_key: self.sealed_content_key(),
        }
    }
}

// ---------------------------------------------------------------------------
// Follower
// ---------------------------------------------------------------------------

/// Start following the Overseer whenever this hex isn't one.
pub fn spawn_replication_task(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("replication", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let mut follower = Follower::new(engine.clone());
            loop {
                let pause = tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    pause = follower.step() => pause,
                };
                if !pause.is_zero() {
                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        _ = tokio::time::sleep(pause) => {}
                    }
                }
            }
            debug!("🛑 Replication task is shutting down...");
        }
    });
}

struct Session {
    /// The Overseer's history ID.
    source_id: String,
    /// The Overseer's hex ID (a different hex means start over).
    overseer_id: String,
    base: String,
    cursor: u64,
    /// Documents whose snapshot version is newer than the snapshot sequence.
    guard: HashMap<DocKey, u64>,
    last_catalog: std::time::Instant,
    /// The Overseer's catalog fingerprint as of the last catalog sync.
    catalog_seen: String,
    /// The slices this hex keeps.
    held: SliceMask,
    /// Whether the Overseer slices the data (recent changes are kept then).
    sliced: bool,
}

/// How often a sliced replica re-checks the placement for slices to copy or give up.
const REBALANCE_EVERY: Duration = Duration::from_secs(5);

/// The last changes of every slice, kept by a sliced replica (see the module comment).
#[derive(Default)]
pub struct RecentChanges {
    source: String,
    entries: std::collections::VecDeque<Change>,
}

impl RecentChanges {
    fn push(&mut self, source: &str, change: &Change, limit: usize) {
        if self.source != source {
            self.source = source.to_string();
            self.entries.clear();
        }
        self.entries.push_back(change.clone());
        while self.entries.len() > limit.max(1) {
            self.entries.pop_front();
        }
    }
}

struct Follower {
    engine: Arc<HexDBEngine>,
    client: reqwest::Client,
    session: Option<Session>,
    was_leading: Option<bool>,
    /// The lattice key the Overseer last accepted (0 is the current key).
    key_index: std::sync::atomic::AtomicUsize,
    last_rebalance: std::time::Instant,
}

impl Follower {
    fn new(engine: Arc<HexDBEngine>) -> Self {
        let client = lattice_client(&engine.config, Duration::from_secs(POLL_SECONDS + 30)).unwrap_or_else(|e| {
            warn!("⚠️ Replication client: {:#}. Using system certificates only.", e);
            crate::crypt::http_client().build().unwrap_or_default()
        });
        Follower {
            engine,
            client,
            session: None,
            was_leading: None,
            key_index: std::sync::atomic::AtomicUsize::new(0),
            last_rebalance: std::time::Instant::now(),
        }
    }

    fn set_status(&self, update: impl FnOnce(&mut ReplicationStatus)) {
        let mut status = self.engine.replication.lock().unwrap();
        update(&mut status);
    }

    /// Do one unit of work; returns how long to wait before the next.
    async fn step(&mut self) -> Duration {
        if self.engine.role() == ROLE_OVERSEER {
            if self.engine.rebuilding().is_some() {
                return match self.rebuild().await {
                    Ok(true) => Duration::ZERO,
                    Ok(false) => Duration::from_secs(2),
                    Err(e) => {
                        warn!("⚠️ Collecting slices after the failover failed: {:#}", e);
                        Duration::from_secs(2)
                    }
                };
            }
            if self.was_leading != Some(true) {
                // Remember that this hex leads its own history now, so after a
                // restart it reports where it really is (see restore_replication_state).
                let led = ReplicaCursor { source_id: self.engine.history_id(), applied_seq: self.engine.changes.published_seq(), held_slices: ALL_SLICES };
                if let Err(e) = self.engine.apply_replicated(Vec::new(), Some(&led)).await {
                    warn!("⚠️ Couldn't record that this hex leads the lattice: {:#}", e);
                }
                if self.was_leading == Some(false) {
                    info!("🎖️ This hex is now the Overseer; replication stops and writes are accepted.");
                    // A promoted replica makes sure the default roles and admin exist.
                    if let Err(e) = crate::users::bootstrap(&self.engine, &self.engine.config.security).await {
                        warn!("⚠️ Couldn't initialize security after promotion: {:#}", e);
                    }
                    self.engine.resume_schema_migrations().await;
                }
                self.was_leading = Some(true);
                self.session = None;
                self.set_status(|s| {
                    *s = ReplicationStatus { state: "leading".into(), ..Default::default() };
                });
                self.engine.recent_changes.lock().unwrap().entries.clear();
            }
            return Duration::from_secs(1);
        }
        self.was_leading = Some(false);

        let overseer = {
            let peers = self.engine.peers.lock().await;
            peers.iter().find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER).cloned()
        };
        let Some(overseer) = overseer else {
            *self.engine.overseer_endpoint.write().unwrap() = None;
            self.set_status(|s| {
                s.state = "waiting".into();
                s.last_error = None;
            });
            return Duration::from_secs(1);
        };
        *self.engine.overseer_endpoint.write().unwrap() = Some(overseer.hex.api_endpoint.clone());

        // Keep streaming only from the same hex at the same address. A hex keeps
        // its ID across restarts but can come back on another address; then the
        // session starts again, resuming from the saved cursor (no full sync).
        let base = peer_base_url(&overseer.hex);
        let wanted = self.wanted_slices(&overseer).await;
        self.set_status(|s| s.slices_wanted = wanted);
        if let Some(session) = self.session.as_mut() {
            session.sliced = overseer.hex.slicing;
        }
        let result = match &self.session {
            Some(session) if session.overseer_id == overseer.hex.id && session.base == base => {
                if session.held != wanted && self.last_rebalance.elapsed() >= REBALANCE_EVERY {
                    self.last_rebalance = std::time::Instant::now();
                    if let Err(e) = self.rebalance(wanted, &overseer).await {
                        warn!("⚠️ Moving slices failed: {:#}", e);
                    }
                }
                self.stream().await
            }
            _ => self.start(&overseer).await,
        };
        match result {
            Ok(pause) => pause,
            // Not a failure: a new Overseer is collecting slices, and serves them when it has.
            Err(e) if e.downcast_ref::<OverseerRebuilding>().is_some() => {
                let note = format!("The Overseer '{}' is collecting slices after a failover; this hex follows it once it has them.", overseer.hex.name);
                self.set_status(|s| {
                    s.state = "waiting".into();
                    s.last_error = Some(note.clone());
                });
                Duration::from_secs(3)
            }
            Err(e) => {
                warn!("⚠️ Replication from '{}' failed: {:#}", overseer.hex.name, e);
                self.set_status(|s| {
                    s.state = "error".into();
                    s.last_error = Some(format!("{:#}", e));
                });
                Duration::from_secs(2)
            }
        }
    }

    /// Resume from the saved cursor if it belongs to this Overseer; otherwise full sync.
    async fn start(&mut self, overseer: &LatticeMember) -> Result<Duration> {
        let base = peer_base_url(&overseer.hex);
        self.set_status(|s| {
            s.source_id = Some(overseer.hex.id.clone());
            s.source_name = Some(overseer.hex.name.clone());
        });
        if let Some(cursor) = self.engine.load_replica_cursor().await {
            // Resume if the Overseer has the same history and still reaches back to the cursor.
            let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/catalog", base)).await?;
            if cursor.source_id == meta.source_id && cursor.applied_seq >= meta.available_after && cursor.applied_seq <= meta.seq {
                info!("🔁 Resuming replication from '{}' after sequence {}.", overseer.hex.name, cursor.applied_seq);
                let held = cursor.held_slices;
                self.set_status(|s| {
                    s.slices_held = held;
                    s.history = Some(cursor.source_id.clone());
                    s.applied_seq = cursor.applied_seq;
                });
                self.session = Some(Session {
                    source_id: cursor.source_id,
                    overseer_id: overseer.hex.id.clone(),
                    base,
                    cursor: cursor.applied_seq,
                    guard: HashMap::new(),
                    last_catalog: std::time::Instant::now() - CATALOG_EVERY,
                    catalog_seen: String::new(),
                    held,
                    sliced: overseer.hex.slicing,
                });
                return Ok(Duration::ZERO);
            }
            if cursor.source_id == meta.source_id {
                info!("📥 '{}' no longer has changes after {}; a full sync is needed.", overseer.hex.name, cursor.applied_seq);
            }
        }
        self.full_sync(overseer, base).await?;
        Ok(Duration::ZERO)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<Result<T, reqwest::StatusCode>> {
        let keys = &self.engine.lattice_keys;
        let mut index = self.key_index.load(std::sync::atomic::Ordering::Relaxed).min(keys.len() - 1);
        let mut tried = 0;
        loop {
            let signature = keys.sign_request_with(index, "GET", &path_and_query(url), b"");
            let response = self
                .client
                .get(url)
                .header(LATTICE_SIGNATURE_HEADER, &signature)
                .send()
                .await
                .with_context(|| format!("GET {}", url))?;
            let status = response.status();
            tried += 1;
            // During a lattice secret rotation the Overseer may only know a
            // previous key; remember whichever key it accepts.
            if status == reqwest::StatusCode::UNAUTHORIZED && tried < keys.len() {
                index = (index + 1) % keys.len();
                continue;
            }
            if !status.is_success() {
                return Ok(Err(status));
            }
            self.key_index.store(index, std::sync::atomic::Ordering::Relaxed);
            let body = verified_body(&self.engine, url, &signature, response).await?;
            return Ok(Ok(serde_json::from_slice(&body).with_context(|| format!("GET {}: unreadable response", url))?));
        }
    }

    async fn get_ok<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        match self.get(url).await? {
            Ok(value) => Ok(value),
            Err(status) if status == reqwest::StatusCode::UNAUTHORIZED => {
                bail!("the Overseer refused this hex's lattice signature; every hex in a lattice needs the same network.lattice_secret (or, without one, the same storage.encryption_key), and clocks within a minute of each other")
            }
            Err(status) if status == reqwest::StatusCode::SERVICE_UNAVAILABLE => Err(OverseerRebuilding.into()),
            Err(status) => bail!("GET {} returned {}", url, status),
        }
    }

    async fn full_sync(&mut self, overseer: &LatticeMember, base: String) -> Result<()> {
        self.session = None;
        self.set_status(|s| {
            s.state = "syncing".into();
            s.last_error = None;
        });
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/snapshot", base)).await?;
        if meta.hex_id != overseer.hex.id {
            bail!("expected Overseer {} but {} answered", overseer.hex.id, meta.hex_id);
        }
        info!("📥 Full sync from Overseer '{}' at sequence {}...", meta.source_name, meta.seq);

        let engine = self.engine.clone();
        engine.adopt_content_key(&meta.content_key)?;
        let slices = self.wanted_slices(overseer).await;
        let wanted: HashSet<&str> = meta.tessellations.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in engine.tessellation_details() {
            if name != REPLICATION_TESSELLATION && !wanted.contains(name.as_str()) {
                engine.drop_replicated_tessellation(&name).await?;
            }
        }

        let mut guard = HashMap::new();
        let mut copied = 0u64;
        for (name, info) in &meta.tessellations {
            engine.ensure_replicated_tessellation(name)?;
            engine.set_replicated_kind(name, &info.kind)?;
            let mut seen: HashSet<Ulid> = HashSet::new();
            let mut after: Option<Ulid> = None;
            let filtered = slices != ALL_SLICES && engine.is_sliced_tessellation(name);
            loop {
                let mut url = format!("{}/lattice/snapshot/{}?limit={}", base, name, PAGE);
                if let Some(a) = after {
                    url.push_str(&format!("&after={}", a));
                }
                if filtered {
                    url.push_str(&format!("&slices={}", slices));
                }
                let page: SnapshotPage = self.get_ok(&url).await?;
                let mut writes = Vec::with_capacity(page.documents.len());
                for (doc, seq) in page.documents {
                    if filtered && !slicing::holds(slices, slicing::slice_of(name, doc.id)) {
                        continue;
                    }
                    seen.insert(doc.id);
                    if seq > meta.seq {
                        guard.insert(DocKey::new(name, doc.id), seq);
                    }
                    writes.push(ReplicaWrite::Put(doc));
                }
                copied += writes.len() as u64;
                engine.apply_replicated(writes, None).await?;
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            // Remove local documents the Overseer doesn't have.
            let stale: Vec<ReplicaWrite> = engine
                .visible_ids(name)
                .await
                .into_iter()
                .filter(|id| !seen.contains(id))
                .map(|id| ReplicaWrite::Delete { tessellation: name.clone(), id })
                .collect();
            for chunk in stale.chunks(PAGE) {
                engine.apply_replicated(chunk.to_vec(), None).await?;
            }
        }
        for (name, info) in &meta.tessellations {
            engine.reconcile_indexes(name, &info.indexes).await?;
            engine.set_schemas_unchecked(name, &info.schemas, info.pending_schema.as_ref())?;
        }

        let cursor = ReplicaCursor { source_id: meta.source_id.clone(), applied_seq: meta.seq, held_slices: slices };
        engine.apply_replicated(Vec::new(), Some(&cursor)).await?;
        self.engine.recent_changes.lock().unwrap().entries.clear();
        info!("✅ Full sync from '{}' complete: {} documents in {} tessellations.", meta.source_name, copied, meta.tessellations.len());
        // Fetch the content of every file the snapshot brought.
        engine.request_chunk_catch_up();
        self.set_status(|s| {
            s.state = "streaming".into();
            s.applied_seq = meta.seq;
            s.source_seq = s.source_seq.max(meta.seq);
            s.synced_documents = copied;
            s.last_sync = Some(Utc::now());
            s.slices_held = slices;
            s.history = Some(meta.source_id.clone());
        });
        self.session = Some(Session {
            source_id: meta.source_id,
            overseer_id: meta.hex_id,
            base,
            cursor: meta.seq,
            guard,
            last_catalog: std::time::Instant::now(),
            catalog_seen: String::new(),
            held: slices,
            sliced: overseer.hex.slicing,
        });
        Ok(())
    }

    /// Long-poll the Overseer's change feed once and apply what arrives.
    async fn stream(&mut self) -> Result<Duration> {
        let (base, cursor, catalog_due) = {
            let s = self.session.as_ref().unwrap();
            (s.base.clone(), s.cursor, s.last_catalog.elapsed() >= CATALOG_EVERY)
        };
        if catalog_due {
            self.sync_catalog(&base).await?;
        }
        let url = format!("{}/lattice/changes?after={}&wait={}&limit=1000&hex={}", base, cursor, POLL_SECONDS, self.engine.id);
        let batch: ChangeBatch = match self.get(&url).await? {
            Ok(batch) => batch,
            Err(status) if status == reqwest::StatusCode::GONE => {
                info!("📥 The Overseer no longer has changes after {}; starting a full sync.", cursor);
                self.session = None;
                self.forget_cursor();
                return Ok(Duration::ZERO);
            }
            Err(status) => return Err(anyhow!("GET {} returned {}", url, status)),
        };
        let session = self.session.as_mut().unwrap();
        if batch.source_id != session.source_id {
            self.session = None;
            return Ok(Duration::ZERO);
        }
        // A new tessellation, index or schema version on the Overseer: copy it now.
        if !batch.catalog.is_empty() && batch.catalog != session.catalog_seen {
            let seen = batch.catalog.clone();
            self.sync_catalog(&base).await?;
            if let Some(s) = self.session.as_mut() {
                s.catalog_seen = seen;
            }
        }
        let session = self.session.as_mut().unwrap();

        let engine = self.engine.clone();
        let mut writes = Vec::new();
        let (held, sliced) = (session.held, session.sliced);
        if sliced {
            let limit = engine.config.replication.recent_changes;
            let mut recent = engine.recent_changes.lock().unwrap();
            for change in &batch.changes {
                if change.id.is_some() && engine.is_sliced_tessellation(&change.tessellation) {
                    // Kept in memory: with its sealed fields sealed (see crate::sealed).
                    let mut kept = change.clone();
                    if let Some(doc) = &kept.document {
                        kept.document = Some(engine.sealer.seal_document(doc, &engine.sealed_paths(&doc.tessellation)));
                    }
                    recent.push(&session.source_id, &kept, limit);
                }
            }
        }
        for change in &batch.changes {
            if held != ALL_SLICES
                && engine.is_sliced_tessellation(&change.tessellation)
                && change.id.is_some_and(|id| !slicing::holds(held, slicing::slice_of(&change.tessellation, id)))
            {
                continue;
            }
            let key = change.id.map(|id| DocKey::new(&change.tessellation, id));
            if let Some(key) = &key {
                if session.guard.get(key).is_some_and(|snap| change.seq <= *snap) {
                    continue;
                }
            }
            match change.kind {
                ChangeKind::Put => {
                    if let Some(doc) = &change.document {
                        writes.push(ReplicaWrite::Put(doc.clone()));
                    }
                }
                ChangeKind::Delete => {
                    if let Some(id) = change.id {
                        writes.push(ReplicaWrite::Delete { tessellation: change.tessellation.clone(), id });
                    }
                }
                ChangeKind::DropTessellation => {
                    // Apply what came before, then drop.
                    let cursor = ReplicaCursor { source_id: session.source_id.clone(), applied_seq: change.seq.saturating_sub(1), held_slices: held };
                    engine.apply_replicated(std::mem::take(&mut writes), Some(&cursor)).await?;
                    engine.drop_replicated_tessellation(&change.tessellation).await?;
                }
            }
        }
        let cursor = ReplicaCursor { source_id: session.source_id.clone(), applied_seq: batch.last_seq, held_slices: held };
        if !writes.is_empty() || batch.last_seq != session.cursor {
            engine.apply_replicated(writes, Some(&cursor)).await?;
        }
        session.cursor = batch.last_seq;
        // Guard entries are only needed until the feed passes their version.
        session.guard.retain(|_, seq| *seq > batch.last_seq);
        let applied = batch.last_seq;
        self.set_status(|s| {
            s.state = "streaming".into();
            s.applied_seq = applied;
            s.source_seq = batch.published_seq.max(applied);
            s.last_error = None;
        });
        Ok(Duration::ZERO)
    }

    /// The slices this hex should keep: its share of the Overseer's placement,
    /// or all of them (no slicing, or fewer than two replicas).
    async fn wanted_slices(&self, overseer: &LatticeMember) -> SliceMask {
        if !overseer.hex.slicing {
            return ALL_SLICES;
        }
        let me = self.engine.id.to_string();
        let mut replicas: Vec<String> = {
            let peers = self.engine.peers.lock().await;
            peers.iter().filter(|p| p.status == "active" && p.hex.role != ROLE_OVERSEER && p.hex.id != me).map(|p| p.hex.id.clone()).collect()
        };
        replicas.push(me.clone());
        if replicas.len() < 2 {
            return ALL_SLICES;
        }
        slicing::placement(&replicas, overseer.hex.slice_copies.max(1)).get(&me).copied().unwrap_or(ALL_SLICES)
    }

    /// Copy slices this hex gains from the Overseer; give up the ones it loses
    /// once enough other replicas hold them.
    async fn rebalance(&mut self, wanted: SliceMask, overseer: &LatticeMember) -> Result<()> {
        let Some(session) = self.session.as_ref() else { return Ok(()) };
        let (base, held, cursor, source) = (session.base.clone(), session.held, session.cursor, session.source_id.clone());
        let engine = self.engine.clone();
        let tessellations: Vec<String> = engine.tessellation_details().into_iter().map(|(n, _)| n).filter(|n| engine.is_sliced_tessellation(n)).collect();

        let gained = wanted & !held;
        if gained != 0 {
            info!("📥 Copying {} slice(s) from the Overseer.", slicing::count(gained));
            // Anything left over from an earlier time with these slices is stale.
            self.delete_slices(&tessellations, gained).await?;
            let mut guard = HashMap::new();
            for name in &tessellations {
                let mut after: Option<Ulid> = None;
                loop {
                    let mut url = format!("{}/lattice/snapshot/{}?limit={}&slices={}", base, name, PAGE, gained);
                    if let Some(a) = after {
                        url.push_str(&format!("&after={}", a));
                    }
                    let page: SnapshotPage = match self.get(&url).await? {
                        Ok(page) => page,
                        Err(status) if status == reqwest::StatusCode::NOT_FOUND => break,
                        Err(status) => bail!("GET {} returned {}", url, status),
                    };
                    let mut writes = Vec::new();
                    for (doc, seq) in page.documents {
                        if !slicing::holds(gained, slicing::slice_of(name, doc.id)) {
                            continue;
                        }
                        // The feed (from `cursor`) skips changes this copy already has.
                        if seq > cursor {
                            guard.insert(DocKey::new(name, doc.id), seq);
                        }
                        writes.push(ReplicaWrite::Put(doc));
                    }
                    engine.apply_replicated(writes, None).await?;
                    match page.next {
                        Some(next) => after = Some(next),
                        None => break,
                    }
                }
            }
            let held = held | gained;
            engine.apply_replicated(Vec::new(), Some(&ReplicaCursor { source_id: source.clone(), applied_seq: cursor, held_slices: held })).await?;
            if let Some(s) = self.session.as_mut() {
                s.held = held;
                s.guard.extend(guard);
            }
            self.set_status(|s| s.slices_held = held);
            engine.request_chunk_catch_up();
            info!("✅ Now keeping {} of {} slices.", slicing::count(held), slicing::SLICES);
        }

        let held = self.session.as_ref().map_or(held, |s| s.held);
        let lost = held & !wanted;
        if lost != 0 {
            // Give up a slice only when enough other live replicas hold it.
            let copies = overseer.hex.slice_copies.max(1);
            let me = engine.id.to_string();
            let others: Vec<SliceMask> = {
                let peers = engine.peers.lock().await;
                peers
                    .iter()
                    .filter(|p| p.status == "active" && p.hex.role != ROLE_OVERSEER && p.hex.id != me && p.hex.replication_source == source)
                    .map(|p| p.hex.slices_held)
                    .collect()
            };
            let safe: SliceMask = slicing::list(lost)
                .into_iter()
                .filter(|slice| others.iter().filter(|m| slicing::holds(**m, *slice)).count() >= copies)
                .fold(0, |mask, slice| mask | (1u64 << slice));
            if safe != 0 {
                let held = held & !safe;
                // Record the smaller set first: a crash in between leaves extra documents, never missing ones.
                engine.apply_replicated(Vec::new(), Some(&ReplicaCursor { source_id: source.clone(), applied_seq: cursor, held_slices: held })).await?;
                if let Some(s) = self.session.as_mut() {
                    s.held = held;
                }
                self.set_status(|s| s.slices_held = held);
                self.delete_slices(&tessellations, safe).await?;
                info!("📤 Gave up {} slice(s) other replicas now keep; keeping {} of {}.", slicing::count(safe), slicing::count(held), slicing::SLICES);
            }
        }
        Ok(())
    }

    /// Delete this hex's documents in some slices.
    async fn delete_slices(&self, tessellations: &[String], slices: SliceMask) -> Result<()> {
        for name in tessellations {
            let doomed: Vec<ReplicaWrite> = self
                .engine
                .visible_ids(name)
                .await
                .into_iter()
                .filter(|id| slicing::holds(slices, slicing::slice_of(name, *id)))
                .map(|id| ReplicaWrite::Delete { tessellation: name.clone(), id })
                .collect();
            for chunk in doomed.chunks(PAGE) {
                self.engine.apply_replicated(chunk.to_vec(), None).await?;
            }
        }
        Ok(())
    }

    /// A newly elected Overseer that kept only some slices collects the rest
    /// from the replicas that kept them (from the same history), and brings
    /// each to its own position with its recent changes. Returns true when done.
    async fn rebuild(&mut self) -> Result<bool> {
        let engine = self.engine.clone();
        let Some(cursor) = engine.load_replica_cursor().await else {
            // Nothing was ever replicated here: there's nothing to rebuild.
            self.finish_rebuild(None).await?;
            return Ok(true);
        };
        let mut held = cursor.held_slices;
        if held == ALL_SLICES {
            self.finish_rebuild(Some(&cursor)).await?;
            return Ok(true);
        }
        self.set_status(|s| s.state = "rebuilding".into());
        let me = engine.id.to_string();
        let holders: Vec<crate::network::discovery::PeerHex> = {
            let peers = engine.peers.lock().await;
            peers
                .iter()
                .filter(|p| p.status == "active" && p.hex.id != me && p.hex.replication_source == cursor.source_id)
                .map(|p| p.hex.clone())
                .collect()
        };
        let tessellations: Vec<String> = engine.tessellation_details().into_iter().map(|(n, _)| n).filter(|n| engine.is_sliced_tessellation(n)).collect();
        // For each missing slice, the holder furthest along.
        let mut plan: HashMap<String, (crate::network::discovery::PeerHex, SliceMask)> = HashMap::new();
        for slice in slicing::list(!held) {
            if let Some(best) = holders.iter().filter(|h| slicing::holds(h.slices_held, slice)).max_by_key(|h| h.applied_seq) {
                plan.entry(best.id.clone()).or_insert_with(|| (best.clone(), 0)).1 |= 1u64 << slice;
            }
        }
        for (holder, slices) in plan.into_values() {
            info!("📥 Collecting {} slice(s) from '{}'.", slicing::count(slices), holder.name);
            let base = peer_base_url(&holder);
            let mut position: Option<u64> = None;
            for name in &tessellations {
                let mut after: Option<Ulid> = None;
                loop {
                    let mut url = format!("{}/lattice/slice-snapshot/{}?limit={}&slices={}", base, name, PAGE, slices);
                    if let Some(a) = after {
                        url.push_str(&format!("&after={}", a));
                    }
                    let page: SlicePage = self.get_ok(&url).await?;
                    if page.source_id != cursor.source_id {
                        bail!("'{}' follows another history now", holder.name);
                    }
                    if *position.get_or_insert(page.applied_seq) != page.applied_seq {
                        bail!("'{}' moved on while its slices were being copied", holder.name);
                    }
                    let writes: Vec<ReplicaWrite> = page.documents.into_iter().map(ReplicaWrite::Put).collect();
                    engine.apply_replicated(writes, None).await?;
                    match page.next {
                        Some(next) => after = Some(next),
                        None => break,
                    }
                }
            }
            // Bring these slices from the holder's position to this hex's.
            let from = position.unwrap_or(holder.applied_seq);
            let (changes, covered) = {
                let recent = engine.recent_changes.lock().unwrap();
                let same = recent.source == cursor.source_id;
                let oldest = recent.entries.front().map(|c| c.seq);
                let covered = from >= cursor.applied_seq || (same && oldest.is_some_and(|o| o <= from + 1));
                let changes: Vec<Change> = if same {
                    recent
                        .entries
                        .iter()
                        .filter(|c| c.seq > from && c.seq <= cursor.applied_seq)
                        .filter(|c| c.id.is_some_and(|id| slicing::holds(slices, slicing::slice_of(&c.tessellation, id))))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                (changes, covered)
            };
            if !covered {
                warn!(
                    "⚠️ '{}' was further behind ({}) than this hex's recent changes reach; its slices may miss changes up to {}.",
                    holder.name, from, cursor.applied_seq
                );
                engine
                    .audit(
                        "system",
                        "lattice.slices_incomplete",
                        &holder.name,
                        serde_json::json!({ "slices": slicing::list(slices), "holder_seq": from, "overseer_seq": cursor.applied_seq }),
                    )
                    .await;
            }
            let writes: Vec<ReplicaWrite> = changes
                .iter()
                .filter_map(|c| match c.kind {
                    ChangeKind::Put => c.document.clone().map(ReplicaWrite::Put),
                    ChangeKind::Delete => c.id.map(|id| ReplicaWrite::Delete { tessellation: c.tessellation.clone(), id }),
                    ChangeKind::DropTessellation => None,
                })
                .collect();
            engine.apply_replicated(writes, None).await?;
            held |= slices;
            engine
                .apply_replicated(Vec::new(), Some(&ReplicaCursor { source_id: cursor.source_id.clone(), applied_seq: cursor.applied_seq, held_slices: held }))
                .await?;
            self.set_status(|s| s.slices_held = held);
        }
        if held != ALL_SLICES {
            let missing = slicing::count(!held);
            // Who has them: hexes discovery knows but can't reach now, holding a missing slice.
            let waiting_for: Vec<String> = {
                let peers = engine.peers.lock().await;
                let mut names: Vec<String> = peers
                    .iter()
                    .filter(|p| p.status != "active" && p.hex.replication_source == cursor.source_id && p.hex.slices_held & !held != 0)
                    .map(|p| p.hex.name.clone())
                    .collect();
                names.sort();
                names
            };
            let note = match waiting_for.as_slice() {
                [] => format!(
                    "This hex just became the Overseer and is collecting slices; {} slice(s) are on hexes that aren't reachable yet. Writes and queries resume when they're back.",
                    missing
                ),
                names => format!(
                    "This hex just became the Overseer and is collecting slices; {} slice(s) are only on {}, which {} reachable. Writes and queries resume when {} back.",
                    missing,
                    join_names(names),
                    if names.len() == 1 { "isn't" } else { "aren't" },
                    if names.len() == 1 { "it's" } else { "one of them is" }
                ),
            };
            if self.engine.replication.lock().unwrap().rebuilding.as_deref() != Some(note.as_str()) {
                warn!("⏳ {}", note);
            }
            self.set_status(|s| {
                s.rebuilding = Some(note);
                s.waiting_for = waiting_for;
            });
            return Ok(false);
        }
        let done = ReplicaCursor { held_slices: ALL_SLICES, ..cursor };
        self.finish_rebuild(Some(&done)).await?;
        Ok(true)
    }

    async fn finish_rebuild(&mut self, cursor: Option<&ReplicaCursor>) -> Result<()> {
        if let Some(cursor) = cursor {
            self.engine.apply_replicated(Vec::new(), Some(&ReplicaCursor { held_slices: ALL_SLICES, ..cursor.clone() })).await?;
        }
        self.engine.request_chunk_catch_up();
        self.set_status(|s| {
            s.slices_held = ALL_SLICES;
            s.rebuilding = None;
            s.waiting_for.clear();
        });
        info!("✅ This hex has every slice; it takes writes and queries as the Overseer.");
        Ok(())
    }

    /// Pick up tessellations and index definitions created on the Overseer.
    async fn sync_catalog(&mut self, base: &str) -> Result<()> {
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/catalog", base)).await?;
        self.engine.adopt_content_key(&meta.content_key)?;
        for (name, info) in &meta.tessellations {
            self.engine.ensure_replicated_tessellation(name)?;
            self.engine.set_replicated_kind(name, &info.kind)?;
            self.engine.reconcile_indexes(name, &info.indexes).await?;
            self.engine.set_schemas_unchecked(name, &info.schemas, info.pending_schema.as_ref())?;
        }
        if let Some(s) = self.session.as_mut() {
            s.last_catalog = std::time::Instant::now();
        }
        Ok(())
    }

    fn forget_cursor(&self) {
        self.set_status(|s| s.state = "syncing".into());
    }
}
