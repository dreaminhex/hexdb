// HexDB Core Engine: online backups
//
// A backup is a consistent copy of the data directory as of one sequence
// number, taken while the hex keeps serving reads and writes:
//
//   1. The flush lock is held for the whole backup, so no flush, compaction
//      swap or tessellation drop changes the set of SSTables and WAL segments.
//   2. Under the state lock, the backup's sequence number is fixed, the WAL is
//      rotated (every write up to that number lands in a closed segment), and
//      the catalog is captured.
//   3. The SSTables (immutable once written) are hard-linked into the backup,
//      or copied when the backup is on another file system; the closed WAL
//      segments and the encrypted metadata files are copied.
//
// Restoring is starting a hex on the backup folder (with the same encryption
// keys): recovery replays the copied WAL segments on top of the SSTables, as
// after a crash at the backup's sequence number. The backup's catalog gets a
// new history ID, so replicas and plugins never mistake a restored copy for
// the original's change history (they take a full sync / start at its end).
//
// Backups are written to `storage.backup_path` (default: `backups` next to the
// data directory) as `<name>/`, built in `<name>.partial/` and renamed when
// complete. Hard links share disk blocks with the live data: copy backups off
// the machine to protect against disk loss.

use super::HexDBEngine;
use crate::{catalog::Catalog, wal};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::info;

/// Files kept beside the data that a backup carries (all encrypted).
const METADATA_FILES: &[&str] = &["settings.hxe", "metrics-history.hxe", "query-stats.hxe"];
/// Describes a backup (plaintext; no names or data).
pub const BACKUP_MANIFEST: &str = "backup.json";

/// What a backup wrote.
#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub path: String,
    /// Every write up to this sequence number is in the backup.
    pub sequence: u64,
    pub created: i64,
    pub files: usize,
    /// Files hard-linked rather than copied (SSTables on the same file system).
    pub linked: usize,
    pub bytes: u64,
    pub millis: u64,
}

#[derive(Default)]
pub(crate) struct CopyStats {
    pub(crate) files: usize,
    pub(crate) linked: usize,
    pub(crate) bytes: u64,
}

impl CopyStats {
    /// Hard-link `from` to `to`, or copy it if linking isn't possible.
    fn link_or_copy(&mut self, from: &Path, to: &Path) -> Result<()> {
        let bytes = fs::metadata(from).with_context(|| format!("can't read {}", from.display()))?.len();
        if fs::hard_link(from, to).is_ok() {
            self.linked += 1;
        } else {
            fs::copy(from, to).with_context(|| format!("can't copy {}", from.display()))?;
        }
        self.files += 1;
        self.bytes += bytes;
        Ok(())
    }

    fn copy(&mut self, from: &Path, to: &Path) -> Result<()> {
        self.bytes += fs::copy(from, to).with_context(|| format!("can't copy {}", from.display()))?;
        self.files += 1;
        Ok(())
    }
}

fn invalid(message: String) -> anyhow::Error {
    crate::engine::EngineError::Invalid(message).into()
}

/// Everything a backup copies, owned so it can run on the blocking pool.
struct CopyJob {
    catalog: Catalog,
    keys: std::sync::Arc<crate::crypt::KeyRing>,
    storage_dir: PathBuf,
    wal_dir: PathBuf,
    new_segment: u64,
    tables: Vec<(String, PathBuf)>,
    partial: PathBuf,
    dest: PathBuf,
    root: PathBuf,
    manifest: serde_json::Value,
}

impl CopyJob {
    fn run(mut self) -> Result<CopyStats> {
        let mut stats = CopyStats::default();
        // A new history: the restored copy diverges from the original.
        self.catalog.save(&self.partial, &self.keys)?;
        stats.files += 1;

        // Closed WAL segments hold every write after the SSTables up to the backup's sequence.
        for (first, path) in wal::list_segments(&self.wal_dir)? {
            if first < self.new_segment {
                stats.copy(&path, &self.partial.join("wal").join(path.file_name().unwrap()))?;
            }
        }
        for (tess, path) in &self.tables {
            let dir = self.partial.join(tess);
            fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
            stats.link_or_copy(path, &dir.join(path.file_name().unwrap()))?;
        }

        // Index snapshots (written only by flushes, which are held off) and metadata.
        let indexes = self.storage_dir.join("indexes");
        if indexes.is_dir() {
            for tess in fs::read_dir(&indexes)? {
                let tess = tess?.path();
                if !tess.is_dir() {
                    continue;
                }
                let to = self.partial.join("indexes").join(tess.file_name().unwrap());
                fs::create_dir_all(&to)?;
                for file in fs::read_dir(&tess)? {
                    let file = file?.path();
                    if file.is_file() {
                        stats.copy(&file, &to.join(file.file_name().unwrap()))?;
                    }
                }
            }
        }
        for file in METADATA_FILES {
            let from = self.storage_dir.join(file);
            if from.is_file() {
                stats.copy(&from, &self.partial.join(file))?;
            }
        }

        self.manifest["files"] = serde_json::json!(stats.files);
        self.manifest["bytes"] = serde_json::json!(stats.bytes);
        fs::write(self.partial.join(BACKUP_MANIFEST), serde_json::to_vec_pretty(&self.manifest)?)?;
        wal::sync_dir(&self.partial);
        fs::rename(&self.partial, &self.dest).with_context(|| format!("can't move the backup into {}", self.dest.display()))?;
        wal::sync_dir(&self.root);
        Ok(stats)
    }
}

/// Backup names: letters, digits, `-` and `_`.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl HexDBEngine {
    /// The folder backups are written to.
    pub fn backup_dir(&self) -> PathBuf {
        self.config.backup_dir()
    }

    /// Write a consistent backup of this hex's data (see the module comment).
    pub async fn backup(&self, name: Option<&str>) -> Result<BackupInfo> {
        let started = std::time::Instant::now();
        let root = self.backup_dir();
        if root.starts_with(&self.storage_dir) {
            return Err(invalid(format!("storage.backup_path must be outside the data directory ({}).", self.storage_dir.display())));
        }
        let _flush = self.flush_lock.lock().await;

        // Fix the backup's position: rotate the WAL and capture the catalog.
        let (sequence, rotation, mut catalog) = {
            let state = self.state.lock().await;
            let rotation = self.wal.rotate(state.next_seq).await?;
            let catalog: Catalog = self.catalog.lock().unwrap().clone();
            (state.next_seq.saturating_sub(1), rotation, catalog)
        };
        let new_segment = rotation
            .await
            .map_err(|_| anyhow::anyhow!("WAL writer stopped during the backup"))?
            .map_err(|e| anyhow::anyhow!("WAL rotation failed: {}", e))?;

        let created = Utc::now().timestamp_millis();
        let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) if valid_name(n) => n.to_string(),
            Some(_) => return Err(invalid("A backup name may only contain letters, digits, '-' and '_' (up to 100).".into())),
            None => format!("hexdb-{}-{}", Utc::now().format("%Y%m%d-%H%M%S"), sequence),
        };
        let dest = root.join(&name);
        if dest.exists() {
            return Err(crate::engine::EngineError::Conflict(format!("A backup named '{}' already exists.", name)).into());
        }
        let partial = root.join(format!("{}.partial", name));
        if partial.exists() {
            fs::remove_dir_all(&partial).with_context(|| format!("can't clear {}", partial.display()))?;
        }
        fs::create_dir_all(partial.join("wal")).with_context(|| format!("can't create {}", partial.display()))?;

        // The copying runs on the blocking pool; compaction stays held off.
        let (_compaction, tables) = self.sst.hold_tables().await;
        catalog.history_id = ulid::Ulid::new().to_string();
        let job = CopyJob {
            catalog,
            keys: self.keys.clone(),
            storage_dir: self.storage_dir.clone(),
            wal_dir: self.wal_dir.clone(),
            new_segment,
            tables,
            partial: partial.clone(),
            dest: dest.clone(),
            root: root.clone(),
            manifest: serde_json::json!({
                "format": 1,
                "created": created,
                "sequence": sequence,
                "hex": self.id.to_string(),
                "version": self.version,
            }),
        };
        let result = tokio::task::spawn_blocking(move || job.run())
            .await
            .map_err(|e| anyhow::anyhow!("the backup task failed: {}", e))
            .and_then(|r| r)
            .map(|stats| BackupInfo {
                name: name.clone(),
                path: dest.display().to_string(),
                sequence,
                created,
                files: stats.files,
                linked: stats.linked,
                bytes: stats.bytes,
                millis: started.elapsed().as_millis() as u64,
            });

        match result {
            Ok(info) => {
                info!(
                    "📦 Backed up {} file(s) ({} linked, {} bytes) up to sequence {} to {} in {} ms.",
                    info.files, info.linked, info.bytes, info.sequence, info.path, info.millis
                );
                Ok(info)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&partial);
                Err(e.context("The backup failed"))
            }
        }
    }

    /// Backups in the backup folder, newest first (from their manifests).
    pub fn list_backups(&self) -> Result<Vec<serde_json::Value>> {
        let root = self.backup_dir();
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut list = Vec::new();
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let Ok(text) = fs::read_to_string(path.join(BACKUP_MANIFEST)) else { continue };
            let Ok(mut manifest) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            manifest["name"] = serde_json::json!(path.file_name().and_then(|n| n.to_str()).unwrap_or_default());
            manifest["path"] = serde_json::json!(path.display().to_string());
            list.push(manifest);
        }
        list.sort_by_key(|m| std::cmp::Reverse(m["created"].as_i64().unwrap_or(0)));
        Ok(list)
    }
}
