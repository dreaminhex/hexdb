// HexDB Core Engine: continuous shipping for point-in-time restore
//
// See design/point-in-time-restore.md. The Overseer ships, to an S3-compatible
// store under `<prefix>/history/<history id>/`:
//
//   base/<sequence>/...   a lattice backup without its file content, every
//                         `backup.continuous.base_hours`, its manifest last;
//   packs/vertex-<n>/<p>  each sealed pack file once, and of the open one the
//                         bytes appended since the last pass (`<p>.<offset>`);
//   wal/<segment>         each closed WAL segment, the WAL being rotated when
//                         records have waited `max_lag_seconds`;
//   state.hxe             what has been shipped (sealed with the storage key).
//
// One pass runs every few seconds. Packs go before segments: a chunk is
// durable before the commit that refers to it, so every shipped segment's
// chunks are shipped already. Until a segment is in the store it isn't pruned
// (`keep_unshipped`). Everything shipped is encrypted with the storage key.

use super::{backup::BackupScope, HexDBEngine};
use crate::objstore::{ObjectStore, StoreConfig};
use crate::wal;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Associated data of the sealed state, so another object can't stand in for it.
pub const STATE_AAD: &[u8] = b"hexdb-continuous-state";
const STATE_FILE: &str = "state.hxe";

/// Shipping settings from the lattice configuration; None when it's off or incomplete.
#[derive(Debug, Clone)]
pub struct ContinuousConfig {
    pub store: StoreConfig,
    pub prefix: String,
    pub retention_days: u64,
    pub base_hours: u64,
    pub max_lag: Duration,
}

/// What a history has shipped (kept in the store, sealed).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShipState {
    pub history: String,
    /// Segments whose first sequence number is at most this are shipped (or covered by a base).
    pub segments_through: u64,
    /// "vertex-<n>/<file>" -> bytes shipped, and whether the pack was sealed when shipped whole.
    pub packs: BTreeMap<String, PackShipped>,
    /// Packs the lattice has since deleted, with when it noticed (epoch milliseconds).
    pub deleted: BTreeMap<String, i64>,
    /// Bases shipped, oldest first.
    pub bases: Vec<Base>,
    /// When a pass last finished (epoch milliseconds).
    pub last_pass: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PackShipped {
    pub len: u64,
    pub sealed: bool,
    /// Offsets of the pieces shipped while the pack was open (their objects are `<file>.<offset>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pieces: Vec<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Base {
    pub sequence: u64,
    pub created: i64,
}

/// Shipping's state on this hex.
#[derive(Default)]
pub struct Runtime {
    state: Mutex<Option<ShipState>>,
    last_rotation: Mutex<Option<Instant>>,
    last_error: Mutex<Option<(i64, String)>>,
    /// The first sequence number of the oldest segment not yet in the store (0 until a pass knows).
    unshipped_from: AtomicU64,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn seg_name(seq: u64) -> String {
    format!("{:020}.wal", seq)
}

impl HexDBEngine {
    /// The shipping settings, if shipping is on and complete.
    pub fn continuous_config(&self) -> Option<ContinuousConfig> {
        if self.config_value("backup.continuous.enabled").as_bool() != Some(true) {
            return None;
        }
        let text = |key: &str| self.config_value(key).as_str().map(str::trim).unwrap_or_default().to_string();
        let number = |key: &str, default: u64| self.config_value(key).as_u64().unwrap_or(default);
        let (endpoint, bucket) = (text("backup.continuous.endpoint"), text("backup.continuous.bucket"));
        let secret = self.config_secret("backup.continuous.secret_access_key")?;
        if endpoint.is_empty() || bucket.is_empty() {
            return None;
        }
        let region = Some(text("backup.continuous.region")).filter(|r| !r.is_empty()).unwrap_or_else(|| "us-east-1".into());
        let prefix = Some(text("backup.continuous.prefix")).filter(|p| !p.is_empty()).unwrap_or_else(|| self.config.network.lattice_name.clone());
        Some(ContinuousConfig {
            store: StoreConfig { endpoint, bucket, region, access_key_id: text("backup.continuous.access_key_id"), secret_access_key: secret.expose().to_string() },
            prefix: prefix.trim_matches('/').to_string(),
            retention_days: number("backup.continuous.retention_days", 7).max(1),
            base_hours: number("backup.continuous.base_hours", 24).max(1),
            max_lag: Duration::from_secs(number("backup.continuous.max_lag_seconds", 60).max(10)),
        })
    }

    /// While shipping is on: the first sequence number of the oldest WAL segment not yet
    /// in the store, which pruning must keep (0 until a pass has run: keep everything).
    pub(crate) fn keep_unshipped(&self) -> Option<u64> {
        self.continuous_config().map(|_| self.continuous.unshipped_from.load(Ordering::Acquire))
    }

    /// Shipping's state, for `GET /backups/continuous`.
    pub fn continuous_status(&self) -> Value {
        let Some(config) = self.continuous_config() else {
            return json!({ "enabled": false });
        };
        let state = self.continuous.state.lock().unwrap().clone();
        let error = self.continuous.last_error.lock().unwrap().clone();
        json!({
            "enabled": true,
            "endpoint": config.store.endpoint,
            "bucket": config.store.bucket,
            "prefix": config.prefix,
            "retention_days": config.retention_days,
            "history": state.as_ref().map(|s| s.history.clone()),
            "shipped_through": state.as_ref().map(|s| s.segments_through),
            "last_pass": state.as_ref().map(|s| s.last_pass).filter(|t| *t > 0),
            "oldest_restorable": state.as_ref().and_then(|s| s.bases.first().map(|b| b.created)),
            "last_base": state.as_ref().and_then(|s| s.bases.last().map(|b| json!({ "sequence": b.sequence, "created": b.created }))),
            "last_error": error.map(|(at, message)| json!({ "at": at, "message": message })),
        })
    }

    /// One shipping pass, on the Overseer while it holds the whole lattice. Errors are
    /// kept for the status and the next pass tries again.
    pub(crate) async fn ship_continuous(&self) {
        let Some(config) = self.continuous_config() else { return };
        if !self.is_writable() || !self.backup_coverage().1 {
            return;
        }
        match self.ship_pass(&config).await {
            Ok(()) => *self.continuous.last_error.lock().unwrap() = None,
            Err(e) => {
                let message = format!("{:#}", e);
                let first = self.continuous.last_error.lock().unwrap().as_ref().map(|(_, m)| m != &message).unwrap_or(true);
                if first {
                    warn!("⚠️ Shipping for point-in-time restore failed: {}", message);
                }
                *self.continuous.last_error.lock().unwrap() = Some((now_ms(), message));
            }
        }
    }

    async fn ship_pass(&self, config: &ContinuousConfig) -> Result<()> {
        let store = ObjectStore::new(config.store.clone())?;
        let history = self.catalog.lock().unwrap().history_id.clone();
        let root = format!("{}/history/{}", config.prefix, history);
        let cached = self.continuous.state.lock().unwrap().clone().filter(|s| s.history == history);
        let mut state = match cached {
            Some(s) => s,
            None => self.load_state(&store, &root, &history).await?,
        };

        // A base when there's none for this history, or the newest is too old.
        let base_due = state.bases.last().is_none_or(|b| now_ms() - b.created > config.base_hours as i64 * 3_600_000);
        if base_due {
            self.ship_base(&store, &root, &mut state).await?;
        }
        self.ship_packs(&store, &root, &mut state).await?;
        self.maybe_rotate(config).await?;
        self.ship_segments(&store, &root, &mut state).await?;
        self.prune_shipped(&store, &root, &mut state, config).await?;
        state.last_pass = now_ms();
        self.save_state(&store, &root, &state).await?;
        *self.continuous.state.lock().unwrap() = Some(state);
        Ok(())
    }

    async fn load_state(&self, store: &ObjectStore, root: &str, history: &str) -> Result<ShipState> {
        match store.get(&format!("{}/{}", root, STATE_FILE)).await? {
            Some(sealed) => {
                let plain = self.keys.decrypt_any(&sealed, STATE_AAD).context("the shipped state can't be read with this lattice's storage key")?;
                Ok(serde_json::from_slice(&plain)?)
            }
            None => Ok(ShipState { history: history.to_string(), ..Default::default() }),
        }
    }

    async fn save_state(&self, store: &ObjectStore, root: &str, state: &ShipState) -> Result<()> {
        let sealed = self.keys.encrypt(&serde_json::to_vec(state)?, STATE_AAD)?;
        store.put(&format!("{}/{}", root, STATE_FILE), sealed).await
    }

    /// A lattice backup, uploaded without its file content (packs are shipped on their own),
    /// its manifest last; the local copy is removed afterwards.
    async fn ship_base(&self, store: &ObjectStore, root: &str, state: &mut ShipState) -> Result<()> {
        let name = format!("continuous-{}", ulid::Ulid::generate().to_string().to_ascii_lowercase());
        let info = self.backup(Some(&name), BackupScope::Lattice).await?;
        let dir = std::path::PathBuf::from(&info.path);
        let result = async {
            let mut files = Vec::new();
            collect_files(&dir, &dir, &mut files)?;
            let prefix = format!("{}/base/{:020}", root, info.sequence);
            let manifest = super::backup::BACKUP_MANIFEST;
            for rel in files.iter().filter(|r| !r.starts_with("blobs/") && r.as_str() != manifest) {
                store.put(&format!("{}/{}", prefix, rel), tokio::fs::read(dir.join(rel)).await?).await?;
            }
            store.put(&format!("{}/{}", prefix, manifest), tokio::fs::read(dir.join(manifest)).await?).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        result?;
        state.bases.push(Base { sequence: info.sequence, created: info.created });
        // Segments up to the base's sequence are in it.
        state.segments_through = state.segments_through.max(info.sequence);
        info!("📤 Shipped a base backup (sequence {}) for point-in-time restore.", info.sequence);
        Ok(())
    }

    /// Sealed packs whole (once), and the open packs' new bytes.
    async fn ship_packs(&self, store: &ObjectStore, root: &str, state: &mut ShipState) -> Result<()> {
        let packs: Vec<(String, std::path::PathBuf, u64, bool)> = self.blobs.with_snapshot(|packs| {
            packs.iter().map(|(v, path, len, active)| (format!("vertex-{}/{}", v, path.file_name().and_then(|n| n.to_str()).unwrap_or_default()), path.clone(), *len, *active)).collect()
        });
        let live: std::collections::HashSet<&String> = packs.iter().map(|(k, ..)| k).collect();
        let gone: Vec<String> = state.packs.keys().filter(|k| !live.contains(k) && !state.deleted.contains_key(*k)).cloned().collect();
        for key in gone {
            state.deleted.insert(key, now_ms());
        }
        for (key, path, len, active) in &packs {
            let shipped = state.packs.get(key).cloned().unwrap_or_default();
            if shipped.sealed || (*active && *len <= shipped.len) {
                continue;
            }
            let object = format!("{}/packs/{}", root, key);
            if !*active {
                // Sealed: the whole pack, once (it may have been deleted since the snapshot: next pass).
                let Ok(bytes) = tokio::fs::read(path).await else { continue };
                store.put(&object, bytes).await?;
                state.packs.insert(key.clone(), PackShipped { len: *len, sealed: true, pieces: shipped.pieces });
            } else {
                // Open: the bytes appended since the last piece (synced by the snapshot).
                let bytes = read_range(path, shipped.len, *len)?;
                store.put(&format!("{}.{:012}", object, shipped.len), bytes).await?;
                let mut pieces = shipped.pieces;
                pieces.push(shipped.len);
                state.packs.insert(key.clone(), PackShipped { len: *len, sealed: false, pieces });
            }
        }
        Ok(())
    }

    /// Close the open WAL segment when it holds records and the last rotation is
    /// `max_lag` ago, so they can be shipped.
    async fn maybe_rotate(&self, config: &ContinuousConfig) -> Result<()> {
        let due = self.continuous.last_rotation.lock().unwrap().is_none_or(|at| at.elapsed() >= config.max_lag);
        if !due {
            return Ok(());
        }
        let active = wal::list_segments(&self.wal_dir)?.last().map(|(seq, _)| *seq).unwrap_or(0);
        let _flush = self.flush_lock.lock().await;
        let rotation = {
            let state = self.state.lock().await;
            if state.next_seq <= active {
                // Nothing in the open segment yet.
                return Ok(());
            }
            self.wal.rotate(state.next_seq).await?
        };
        rotation.await.map_err(|_| anyhow!("the WAL writer stopped"))?.map_err(|e| anyhow!("WAL rotation failed: {}", e))?;
        *self.continuous.last_rotation.lock().unwrap() = Some(Instant::now());
        Ok(())
    }

    /// Closed segments not shipped yet, oldest first; then the bound pruning keeps.
    async fn ship_segments(&self, store: &ObjectStore, root: &str, state: &mut ShipState) -> Result<()> {
        let mut segments = wal::list_segments(&self.wal_dir.join(wal::ARCHIVE_DIR))?;
        let live = wal::list_segments(&self.wal_dir)?;
        let active = live.last().map(|(seq, _)| *seq);
        segments.extend(live);
        segments.sort_by_key(|(seq, _)| *seq);
        for (seq, path) in &segments {
            if Some(*seq) == active || *seq <= state.segments_through {
                continue;
            }
            store.put(&format!("{}/wal/{}", root, seg_name(*seq)), tokio::fs::read(path).await?).await?;
            state.segments_through = *seq;
        }
        // Everything before the open segment is in the store now.
        self.continuous.unshipped_from.store(active.unwrap_or(0), Ordering::Release);
        Ok(())
    }

    /// Bases past the window (but the newest of those), the segments only they needed, packs
    /// deleted before the oldest kept base, and histories of earlier Overseers left idle past it.
    async fn prune_shipped(&self, store: &ObjectStore, root: &str, state: &mut ShipState, config: &ContinuousConfig) -> Result<()> {
        let window_start = now_ms() - config.retention_days as i64 * 86_400_000;
        let old: Vec<usize> = state.bases.iter().enumerate().filter(|(_, b)| b.created < window_start).map(|(i, _)| i).collect();
        if old.len() > 1 {
            let drop: Vec<Base> = old[..old.len() - 1].iter().map(|i| state.bases[*i].clone()).collect();
            for base in &drop {
                for object in store.list(&format!("{}/base/{:020}/", root, base.sequence)).await? {
                    store.delete(&object.key).await?;
                }
            }
            state.bases.retain(|b| !drop.contains(b));
        }
        let Some(oldest) = state.bases.first().cloned() else { return Ok(()) };
        // Segments wholly before the oldest kept base: each covers up to the next one's start.
        let shipped = store.list(&format!("{}/wal/", root)).await?;
        let starts: Vec<(u64, String)> = shipped.iter().filter_map(|o| o.key.rsplit('/').next()?.strip_suffix(".wal")?.parse().ok().map(|s| (s, o.key.clone()))).collect();
        for pair in starts.windows(2) {
            if pair[1].0 <= oldest.sequence + 1 {
                store.delete(&pair[0].1).await?;
            }
        }
        let expired: Vec<String> = state.deleted.iter().filter(|(_, at)| **at < oldest.created).map(|(k, _)| k.clone()).collect();
        for key in expired {
            let object = format!("{}/packs/{}", root, key);
            for piece in store.list(&object).await? {
                if piece.key == object || piece.key.starts_with(&format!("{}.", object)) {
                    store.delete(&piece.key).await?;
                }
            }
            state.deleted.remove(&key);
            state.packs.remove(&key);
        }
        // Earlier Overseers' histories, once their last pass is older than the window.
        for object in store.list(&format!("{}/history/", config.prefix)).await? {
            let Some(rest) = object.key.strip_prefix(&format!("{}/history/", config.prefix)) else { continue };
            let Some((other, file)) = rest.split_once('/') else { continue };
            if file != STATE_FILE || other == state.history {
                continue;
            }
            let Some(sealed) = store.get(&object.key).await? else { continue };
            let Ok(plain) = self.keys.decrypt_any(&sealed, STATE_AAD) else { continue };
            let Ok(theirs) = serde_json::from_slice::<ShipState>(&plain) else { continue };
            if theirs.last_pass < window_start {
                for o in store.list(&format!("{}/history/{}/", config.prefix, other)).await? {
                    store.delete(&o.key).await?;
                }
                info!("🗑️ Removed the shipped history {} of an earlier Overseer (past the retention window).", other);
            }
        }
        Ok(())
    }
}

/// Every file under `dir`, as paths relative to `root` with '/' separators.
fn collect_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    Ok(())
}

/// Bytes `from..to` of a file.
fn read_range(path: &std::path::Path, from: u64, to: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = vec![0u8; (to - from) as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
