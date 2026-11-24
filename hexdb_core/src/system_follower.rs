// HexDB Core: the system stream from partition 0
//
// On a lattice with several partitions (stage 2, a preview), every hex outside
// partition 0 still needs the lattice-wide system tessellations to authenticate
// requests and check schemas: users, roles, API keys, the configuration, the
// partition map. They're partition 0's. This follower copies them from partition
// 0's Overseer (`/lattice/*?scope=system`) and keeps them current, with its own
// cursor, next to the hex's ordinary replication from its own group's Overseer.
// It also copies every tessellation's definition (indexes, schemas, quotas) from
// partition 0, which owns the catalog.
//
// It runs only while this hex is in a partition other than 0 of a multi-partition
// map; on every other lattice it does nothing. A hex outside partition 0 never
// writes these tessellations itself (`misplaced_write`), so a full sync here, which
// removes what partition 0 no longer has, can't remove anything written locally.
// See design/partitioning-stage-2.md.

use crate::engine::{HexDBEngine, ReplicaCursor, ReplicaWrite};
use crate::network::discovery::ROLE_OVERSEER;
use crate::replication::{lattice_client, path_and_query, peer_base_url, verified_body, ChangeBatch, SnapshotMeta, SnapshotPage, PAGE, POLL_SECONDS};
use crate::LATTICE_SIGNATURE_HEADER;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// Where the stream is: partition 0's Overseer and history, and the last change applied.
struct Position {
    overseer_id: String,
    base: String,
    source_id: String,
    cursor: u64,
}

pub struct SystemFollower {
    engine: Arc<HexDBEngine>,
    client: reqwest::Client,
    position: Option<Position>,
    last_catalog: std::time::Instant,
}

/// Start the system follower (it idles unless this hex is outside partition 0).
pub fn spawn_system_follower(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("system stream", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let mut follower = SystemFollower::new(engine.clone());
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
            debug!("🛑 The system stream is shutting down...");
        }
    });
}

const CATALOG_EVERY: Duration = Duration::from_secs(10);

impl SystemFollower {
    fn new(engine: Arc<HexDBEngine>) -> Self {
        let client = lattice_client(&engine.config, Duration::from_secs(POLL_SECONDS + 30)).unwrap_or_else(|e| {
            warn!("⚠️ System stream client: {:#}. Using system certificates only.", e);
            crate::crypt::http_client().build().unwrap_or_default()
        });
        SystemFollower { engine, client, position: None, last_catalog: std::time::Instant::now() - CATALOG_EVERY }
    }

    /// True while this hex needs the system stream: in a partition other than 0.
    fn needed(&self) -> bool {
        let map = self.engine.partition_map();
        !map.is_single() && map.partition_of_hex(&self.engine.id.to_string()) != Some(0)
    }

    async fn step(&mut self) -> Duration {
        if !self.needed() {
            self.position = None;
            return Duration::from_secs(2);
        }
        let map = self.engine.partition_map();
        let overseer = {
            let peers = self.engine.peers.lock().await;
            peers
                .iter()
                .find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER && map.partition_of_hex(&p.hex.id) == Some(0))
                .map(|p| p.hex.clone())
        };
        let Some(overseer) = overseer else {
            return Duration::from_secs(1);
        };
        let base = peer_base_url(&overseer);
        let same = self.position.as_ref().is_some_and(|p| p.overseer_id == overseer.id && p.base == base);
        let result = if same { self.stream().await } else { self.start(&overseer.id, base).await };
        match result {
            Ok(pause) => pause,
            Err(e) => {
                warn!("⚠️ The system stream from partition 0 ('{}') failed: {:#}", overseer.name, e);
                self.position = None;
                Duration::from_secs(2)
            }
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<std::result::Result<T, reqwest::StatusCode>> {
        let keys = &self.engine.lattice_keys;
        let mut tried = 0;
        loop {
            let signature = keys.sign_request_with(tried, "GET", &path_and_query(url), b"");
            let response = self.client.get(url).header(LATTICE_SIGNATURE_HEADER, &signature).send().await.with_context(|| format!("GET {}", url))?;
            let status = response.status();
            tried += 1;
            if status == reqwest::StatusCode::UNAUTHORIZED && tried < keys.len() {
                continue;
            }
            if !status.is_success() {
                return Ok(Err(status));
            }
            let body = verified_body(&self.engine, url, &signature, response).await?;
            return Ok(Ok(serde_json::from_slice(&body).with_context(|| format!("GET {}: unreadable response", url))?));
        }
    }

    async fn get_ok<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        self.get(url).await?.map_err(|status| anyhow!("GET {} returned {}", url, status))
    }

    /// Resume from the saved cursor when partition 0's Overseer still has the changes
    /// after it; otherwise copy the system tessellations whole.
    async fn start(&mut self, overseer_id: &str, base: String) -> Result<Duration> {
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/catalog?scope=system", base)).await?;
        if let Some(cursor) = self.engine.load_system_cursor().await {
            if cursor.source_id == meta.source_id && cursor.applied_seq >= meta.available_after && cursor.applied_seq <= meta.seq {
                info!("🔁 Resuming the system stream from partition 0 after sequence {}.", cursor.applied_seq);
                self.position = Some(Position { overseer_id: overseer_id.to_string(), base, source_id: cursor.source_id, cursor: cursor.applied_seq });
                return Ok(Duration::ZERO);
            }
        }
        self.full_sync(overseer_id, base).await?;
        Ok(Duration::ZERO)
    }

    async fn full_sync(&mut self, overseer_id: &str, base: String) -> Result<()> {
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/snapshot?scope=system", base)).await?;
        if meta.hex_id != overseer_id {
            return Err(anyhow!("expected partition 0's Overseer {} but {} answered", overseer_id, meta.hex_id));
        }
        info!("📥 Copying the system tessellations from partition 0 ('{}') at sequence {}...", meta.source_name, meta.seq);
        let engine = self.engine.clone();
        engine.adopt_content_key(&meta.content_key)?;
        engine.adopt_token_key(&meta.token_key)?;
        let mut copied = 0u64;
        for (name, info) in &meta.tessellations {
            // Only lattice-wide system tessellations: never anything this hex's group owns.
            if !engine.in_system_scope(name) {
                continue;
            }
            engine.ensure_replicated_tessellation(name)?;
            engine.set_replicated_kind(name, &info.kind)?;
            let mut seen: HashSet<Ulid> = HashSet::new();
            let mut after: Option<Ulid> = None;
            loop {
                let mut url = format!("{}/lattice/snapshot/{}?limit={}", base, name, PAGE);
                if let Some(a) = after {
                    url.push_str(&format!("&after={}", a));
                }
                let page: SnapshotPage = self.get_ok(&url).await?;
                let writes: Vec<ReplicaWrite> = page.documents.into_iter().map(|(doc, _)| {
                    seen.insert(doc.id);
                    ReplicaWrite::Put(doc)
                }).collect();
                copied += writes.len() as u64;
                engine.apply_system_replicated(writes, None).await?;
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            // Partition 0 no longer has these (a deleted user, a revoked key).
            let stale: Vec<ReplicaWrite> = engine.visible_ids(name).await.into_iter().filter(|id| !seen.contains(id)).map(|id| ReplicaWrite::Delete { tessellation: name.clone(), id }).collect();
            for chunk in stale.chunks(PAGE) {
                engine.apply_system_replicated(chunk.to_vec(), None).await?;
            }
        }
        // A document changed between the snapshot's pages is newer than its sequence:
        // the feed brings it again from there, which is harmless (puts are whole documents).
        engine.apply_system_replicated(Vec::new(), Some(&ReplicaCursor { source_id: meta.source_id.clone(), applied_seq: meta.seq, held_slices: 0 })).await?;
        self.sync_catalog(&base).await?;
        info!("✅ System tessellations from partition 0 copied: {} documents.", copied);
        self.position = Some(Position { overseer_id: overseer_id.to_string(), base, source_id: meta.source_id, cursor: meta.seq });
        Ok(())
    }

    /// Every tessellation's definition (indexes, schemas, quotas) from partition 0.
    async fn sync_catalog(&mut self, base: &str) -> Result<()> {
        let meta: SnapshotMeta = self.get_ok(&format!("{}/lattice/catalog?scope=catalog", base)).await?;
        for (name, info) in &meta.tessellations {
            self.engine.ensure_replicated_tessellation(name)?;
            self.engine.set_replicated_kind(name, &info.kind)?;
            self.engine.reconcile_indexes(name, &info.indexes).await?;
            self.engine.set_schemas_unchecked(name, &info.schemas, info.pending_schema.as_ref())?;
            self.engine.set_quota_unchecked(name, info.quota.clone())?;
        }
        self.last_catalog = std::time::Instant::now();
        Ok(())
    }

    async fn stream(&mut self) -> Result<Duration> {
        let (base, cursor, source_id) = {
            let p = self.position.as_ref().unwrap();
            (p.base.clone(), p.cursor, p.source_id.clone())
        };
        if self.last_catalog.elapsed() >= CATALOG_EVERY {
            self.sync_catalog(&base).await?;
        }
        let url = format!("{}/lattice/changes?after={}&wait={}&limit=1000&scope=system", base, cursor, POLL_SECONDS);
        let batch: ChangeBatch = match self.get(&url).await? {
            Ok(batch) => batch,
            Err(status) if status == reqwest::StatusCode::GONE => {
                info!("📥 Partition 0 no longer has system changes after {}; copying them again.", cursor);
                self.position = None;
                return Ok(Duration::ZERO);
            }
            Err(status) => return Err(anyhow!("GET {} returned {}", url, status)),
        };
        if batch.source_id != source_id {
            // Partition 0 failed over to another history: copy again from the new Overseer.
            self.position = None;
            return Ok(Duration::ZERO);
        }
        let engine = self.engine.clone();
        let mut writes = Vec::new();
        for change in &batch.changes {
            if !engine.in_system_scope(&change.tessellation) {
                continue;
            }
            match change.kind {
                crate::changes::ChangeKind::Put => {
                    if let Some(doc) = &change.document {
                        writes.push(ReplicaWrite::Put(doc.clone()));
                    }
                }
                crate::changes::ChangeKind::Delete => {
                    if let Some(id) = change.id {
                        writes.push(ReplicaWrite::Delete { tessellation: change.tessellation.clone(), id });
                    }
                }
                crate::changes::ChangeKind::DropTessellation => {}
            }
        }
        if !writes.is_empty() || batch.last_seq != cursor {
            engine.apply_system_replicated(writes, Some(&ReplicaCursor { source_id: source_id.clone(), applied_seq: batch.last_seq, held_slices: 0 })).await?;
        }
        if let Some(p) = self.position.as_mut() {
            p.cursor = batch.last_seq;
        }
        Ok(Duration::ZERO)
    }
}
