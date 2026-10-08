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
// Internal endpoints require a fresh, single-use request signature made with
// the lattice key (see `network::lattice_auth`), so only hexes configured with
// the same lattice secret can replicate from each other, and captured requests
// can't be replayed. With `[tls]` configured, replication uses HTTPS.

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

pub use crate::network::lattice_auth::LATTICE_SIGNATURE_HEADER;
/// Documents per snapshot page and per applied batch.
const PAGE: usize = 500;
/// How long a replica long-polls the Overseer for changes.
const POLL_SECONDS: u64 = 5;
/// How often a streaming replica re-checks the Overseer's catalog (new tessellations, indexes).
const CATALOG_EVERY: Duration = Duration::from_secs(15);

/// An HTTP client for talking to other hexes: HTTPS-capable, trusting the
/// system roots plus `tls.ca_file` if set.
pub fn lattice_client(config: &HexConfig, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().use_rustls_tls().timeout(timeout).https_only(config.tls.enabled());
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
            .header(LATTICE_SIGNATURE_HEADER, signature)
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
        return Ok(());
    }
    Ok(())
}

/// What replication is doing on this hex, for `/status` and the dashboard.
#[derive(Debug, Clone, Default, Serialize)]
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
}

impl HexDBEngine {
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
        }
    }
}

// ---------------------------------------------------------------------------
// Follower
// ---------------------------------------------------------------------------

/// Start following the Overseer whenever this hex isn't one.
pub fn spawn_replication_task(engine: Arc<HexDBEngine>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
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
}

struct Follower {
    engine: Arc<HexDBEngine>,
    client: reqwest::Client,
    session: Option<Session>,
    was_leading: Option<bool>,
    /// The lattice key the Overseer last accepted (0 is the current key).
    key_index: std::sync::atomic::AtomicUsize,
}

impl Follower {
    fn new(engine: Arc<HexDBEngine>) -> Self {
        let client = lattice_client(&engine.config, Duration::from_secs(POLL_SECONDS + 30)).unwrap_or_else(|e| {
            warn!("⚠️ Replication client: {:#}. Using system certificates only.", e);
            reqwest::Client::new()
        });
        Follower { engine, client, session: None, was_leading: None, key_index: std::sync::atomic::AtomicUsize::new(0) }
    }

    fn set_status(&self, update: impl FnOnce(&mut ReplicationStatus)) {
        let mut status = self.engine.replication.lock().unwrap();
        update(&mut status);
    }

    /// Do one unit of work; returns how long to wait before the next.
    async fn step(&mut self) -> Duration {
        if self.engine.role() == ROLE_OVERSEER {
            if self.was_leading != Some(true) {
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
            self.set_status(|s| s.state = "waiting".into());
            return Duration::from_secs(1);
        };
        *self.engine.overseer_endpoint.write().unwrap() = Some(overseer.hex.api_endpoint.clone());

        let result = match &self.session {
            Some(session) if session.overseer_id == overseer.hex.id => self.stream().await,
            _ => self.start(&overseer).await,
        };
        match result {
            Ok(pause) => pause,
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
                self.session = Some(Session {
                    source_id: cursor.source_id,
                    overseer_id: overseer.hex.id.clone(),
                    base,
                    cursor: cursor.applied_seq,
                    guard: HashMap::new(),
                    last_catalog: std::time::Instant::now() - CATALOG_EVERY,
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
            let response = self
                .client
                .get(url)
                .header(LATTICE_SIGNATURE_HEADER, keys.sign_request_with(index, "GET", &path_and_query(url), b""))
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
            return Ok(Ok(response.json().await.with_context(|| format!("GET {}: unreadable response", url))?));
        }
    }

    async fn get_ok<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        match self.get(url).await? {
            Ok(value) => Ok(value),
            Err(status) if status == reqwest::StatusCode::UNAUTHORIZED => {
                bail!("the Overseer refused this hex's lattice signature; every hex in a lattice needs the same network.lattice_secret (or, without one, the same storage.encryption_key), and clocks within a minute of each other")
            }
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
        let wanted: HashSet<&str> = meta.tessellations.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in engine.tessellation_details() {
            if name != REPLICATION_TESSELLATION && !wanted.contains(name.as_str()) {
                engine.drop_replicated_tessellation(&name).await?;
            }
        }

        let mut guard = HashMap::new();
        let mut copied = 0u64;
        for (name, _) in &meta.tessellations {
            engine.ensure_replicated_tessellation(name)?;
            let mut seen: HashSet<Ulid> = HashSet::new();
            let mut after: Option<Ulid> = None;
            loop {
                let mut url = format!("{}/lattice/snapshot/{}?limit={}", base, name, PAGE);
                if let Some(a) = after {
                    url.push_str(&format!("&after={}", a));
                }
                let page: SnapshotPage = self.get_ok(&url).await?;
                let mut writes = Vec::with_capacity(page.documents.len());
                for (doc, seq) in page.documents {
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
            engine.set_schemas_unchecked(name, &info.schemas)?;
        }

        let cursor = ReplicaCursor { source_id: meta.source_id.clone(), applied_seq: meta.seq };
        engine.apply_replicated(Vec::new(), Some(&cursor)).await?;
        info!("✅ Full sync from '{}' complete: {} documents in {} tessellations.", meta.source_name, copied, meta.tessellations.len());
        self.set_status(|s| {
            s.state = "streaming".into();
            s.applied_seq = meta.seq;
            s.source_seq = s.source_seq.max(meta.seq);
            s.synced_documents = copied;
            s.last_sync = Some(Utc::now());
        });
        self.session = Some(Session {
            source_id: meta.source_id,
            overseer_id: meta.hex_id,
            base,
            cursor: meta.seq,
            guard,
            last_catalog: std::time::Instant::now(),
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

        let engine = self.engine.clone();
        let mut writes = Vec::new();
        for change in &batch.changes {
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
                    let cursor = ReplicaCursor { source_id: session.source_id.clone(), applied_seq: change.seq.saturating_sub(1) };
                    engine.apply_replicated(std::mem::take(&mut writes), Some(&cursor)).await?;
                    engine.drop_replicated_tessellation(&change.tessellation).await?;
                }
            }
        }
        let cursor = ReplicaCursor { source_id: session.source_id.clone(), applied_seq: batch.last_seq };
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

    /// Pick up tessellations and index definitions created on the Overseer.
    async fn sync_catalog(&mut self, base: &str) -> Result<()> {
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/catalog", base)).await?;
        for (name, info) in &meta.tessellations {
            self.engine.ensure_replicated_tessellation(name)?;
            self.engine.reconcile_indexes(name, &info.indexes).await?;
            self.engine.set_schemas_unchecked(name, &info.schemas)?;
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
