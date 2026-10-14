// The chunk store's remote tier: sealed packs on a backend from
// `crate::remote`, with local copies dropped beyond a budget.
//
//   * Upload: every sealed pack (one that's no longer being appended to) is
//     uploaded to `<prefix>/<hex ID>/vertex-<n>/<number>.hxp`, after its record
//     headers are written to a local `<number>.hxi` index file.
//   * Drop: while the local packs take more than the budget, the oldest
//     uploaded packs lose their local file. The index file keeps them in the
//     chunk index at startup; reads fetch the byte ranges they need.
//   * Move: when the backend changes, packs kept only on the old backend are
//     downloaded (with the old backend's settings, from the configuration
//     version they came from) and uploaded to the new one; with no backend
//     (`local`), they're downloaded and kept here. Objects on the old backend
//     are left in place.
//   * Each vertex directory has a `remote.json` naming the packs uploaded (and
//     where) and the packs dropped. A crash between steps leaves a pack both
//     local and marked dropped; the local file wins.
//
// The steps run one pack at a time on the blocking pool, holding off sweeps
// and backups, from `HexDBEngine::run_remote_storage`.

use super::*;
use crate::remote::{BackendSpec, RemoteStore};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const INDEX_MAGIC: &[u8; 4] = b"HXI1";
const REMOTE_STATE: &str = "remote.json";
/// Packs are downloaded in pieces of this size.
const DOWNLOAD_PIECE: usize = 16 * 1024 * 1024;

/// The backend in use and the one packs are being moved away from.
pub type Tiers = (Option<Arc<Tier>>, Option<Arc<Tier>>);

/// A configured backend and where this hex's packs go on it.
pub struct Tier {
    store: Box<dyn RemoteStore>,
    spec: BackendSpec,
    hex: String,
    /// The configuration version the settings came from.
    config_version: u64,
}

impl Tier {
    pub fn new(store: Box<dyn RemoteStore>, spec: BackendSpec, hex: &str, config_version: u64) -> Tier {
        Tier { store, spec, hex: hex.to_string(), config_version }
    }

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn kind(&self) -> &str {
        &self.spec.kind
    }

    fn key(&self, v: usize, number: u32) -> String {
        let key = format!("{}/vertex-{}/{:08}.hxp", self.hex, v, number);
        if self.spec.prefix.is_empty() {
            key
        } else {
            format!("{}/{}", self.spec.prefix, key)
        }
    }

    /// Write, read back and delete a small object.
    pub fn check(&self) -> Result<()> {
        let key = if self.spec.prefix.is_empty() { format!("{}/probe", self.hex) } else { format!("{}/{}/probe", self.spec.prefix, self.hex) };
        self.store.check(&key)
    }
}

/// One vertex's remote state, kept in `remote.json` in its directory.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct VertexRemote {
    /// Pack number -> the backend (ID) it was uploaded to.
    #[serde(default)]
    pub(super) uploaded: BTreeMap<u32, String>,
    /// Packs with no local file.
    #[serde(default)]
    pub(super) evicted: BTreeSet<u32>,
    /// Backend ID -> the configuration version its settings came from.
    #[serde(default)]
    pub(super) versions: BTreeMap<String, u64>,
}

impl VertexRemote {
    pub(super) fn path(dir: &Path) -> PathBuf {
        dir.join(REMOTE_STATE)
    }

    pub(super) fn load(dir: &Path) -> VertexRemote {
        match fs::read(Self::path(dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!("⚠️ Ignoring an unreadable {}: {}", Self::path(dir).display(), e);
                VertexRemote::default()
            }),
            Err(_) => VertexRemote::default(),
        }
    }

    /// Replace the file atomically.
    pub(super) fn save(&self, dir: &Path) {
        let path = Self::path(dir);
        let tmp = dir.join(format!("{}.tmp", REMOTE_STATE));
        let written = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from).and_then(|bytes| {
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &path)?;
            crate::wal::sync_dir(dir);
            Ok(())
        });
        if let Err(e) = written {
            warn!("⚠️ Couldn't save {}: {:#}", path.display(), e);
        }
    }
}

pub(super) fn index_path(dir: &Path, number: u32) -> PathBuf {
    dir.join(format!("{:08}.hxi", number))
}

/// Write a sealed pack's record headers to its index file:
/// `HXI1 | pack size (u64) | count (u32) | (offset u64, header)*`.
fn write_index(pack: &Path, index: &Path) -> Result<u64> {
    let file = File::open(pack)?;
    let len = file.metadata()?.len();
    let mut entries = Vec::new();
    let mut offset = PACK_HEADER_LEN;
    let mut header = [0u8; RECORD_HEADER_LEN];
    while offset + RECORD_HEADER_LEN as u64 <= len {
        read_exact_at(&file, &mut header, offset)?;
        let Some(record) = RecordHeader::decode(&header) else { break };
        entries.push((offset, header));
        offset += (RECORD_HEADER_LEN + record.shard_len as usize) as u64;
    }
    let mut out = Vec::with_capacity(16 + entries.len() * (8 + RECORD_HEADER_LEN));
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (offset, header) in &entries {
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(header);
    }
    let tmp = index.with_extension("hxi.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&out)?;
    f.sync_all()?;
    fs::rename(&tmp, index)?;
    Ok(len)
}

/// Read a dropped pack's index file into `variants`, like `scan_pack`. Returns the pack's size and record count.
pub(super) fn scan_index(path: &Path, v: usize, number: u32, variants: &mut HashMap<ChunkId, Vec<Entry>>) -> Result<(u64, usize)> {
    let bytes = fs::read(path).with_context(|| format!("Failed to read {} (the index of a pack kept on the remote backend)", path.display()))?;
    if bytes.len() < 16 || &bytes[..4] != INDEX_MAGIC {
        bail!("{} is not a HexDB pack index", path.display());
    }
    let size = u64::from_be_bytes(bytes[4..12].try_into().unwrap());
    let count = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let mut at = 16;
    let mut found = 0;
    for _ in 0..count {
        let Some(slice) = bytes.get(at..at + 8 + RECORD_HEADER_LEN) else { break };
        let offset = u64::from_be_bytes(slice[..8].try_into().unwrap());
        at += 8 + RECORD_HEADER_LEN;
        let Some(record) = RecordHeader::decode(&slice[8..]) else { continue };
        if record.index as usize != v || record.shard_len as usize != shard_len(record.cipher_len as usize) {
            continue;
        }
        let list = variants.entry(record.id).or_default();
        let fresh = record.entry();
        let slot = match list.iter().position(|e| e.same_encoding(&fresh)) {
            Some(i) => i,
            None => {
                list.push(fresh);
                list.len() - 1
            }
        };
        list[slot].shards[v] = ShardLoc { pack: number, offset };
        found += 1;
    }
    Ok((size, found))
}

/// What the remote tier is doing, for `/status`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RemoteStatus {
    /// `s3`, `azure`, `gdrive` or `dropbox`; none: local disks only.
    pub backend: Option<String>,
    pub backend_id: Option<String>,
    /// Packs on the backend in use.
    pub uploaded_packs: usize,
    /// Sealed packs not on the backend in use yet.
    pub pending_packs: usize,
    /// Packs kept only remotely (no local file).
    pub remote_only_packs: usize,
    /// Packs still on a previous backend, waiting to be moved.
    pub moving_packs: usize,
    pub local_bytes: u64,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub last_upload_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl BlobStore {
    /// Use these backends: `current` for new uploads, `previous` to move packs
    /// away from. Returns the ones replaced; drop them on the blocking pool
    /// (their HTTP clients can't be dropped on an async thread).
    #[must_use]
    pub fn set_tiers(&self, current: Option<Arc<Tier>>, previous: Option<Arc<Tier>>) -> Tiers {
        std::mem::replace(&mut *self.tiers.write().unwrap(), (current, previous))
    }

    /// The backends in use: (current, previous).
    pub fn tiers_now(&self) -> Tiers {
        self.tiers.read().unwrap().clone()
    }

    /// The backend in use (its ID), if any.
    pub fn current_tier_id(&self) -> Option<String> {
        self.tiers.read().unwrap().0.as_ref().map(|t| t.id().to_string())
    }

    /// Backends packs are kept on, other than `current`, with the configuration
    /// version their settings came from (so they can be reached to move packs away).
    pub fn other_backends(&self, current: Option<&str>) -> BTreeMap<String, u64> {
        let vertices = self.vertices.lock().unwrap();
        let mut out = BTreeMap::new();
        for vertex in vertices.iter() {
            for backend in vertex.remote.evicted.iter().filter_map(|n| vertex.remote.uploaded.get(n)) {
                if Some(backend.as_str()) != current {
                    out.insert(backend.clone(), vertex.remote.versions.get(backend).copied().unwrap_or(0));
                }
            }
        }
        out
    }

    fn tier_for(&self, backend: &str) -> Option<Arc<Tier>> {
        let tiers = self.tiers.read().unwrap();
        let found = [tiers.0.as_ref(), tiers.1.as_ref()].into_iter().flatten().find(|t| t.id() == backend).cloned();
        found
    }

    pub(super) fn read_remote(&self, backend: &str, v: usize, pack: u32, offset: u64, len: usize) -> Result<Vec<u8>> {
        let tier = self.tier_for(backend).ok_or_else(|| anyhow!("pack {} of vertex {} is on a storage backend that isn't configured any more", pack, v))?;
        let data = tier.store.get_range(&tier.key(v, pack), offset, len);
        match &data {
            Ok(bytes) => self.remote_status.lock().unwrap().bytes_downloaded += bytes.len() as u64,
            Err(e) => self.note_remote_error(e),
        }
        data
    }

    pub(super) fn delete_remote(&self, backend: &str, v: usize, pack: u32) {
        match self.tier_for(backend) {
            Some(tier) => {
                if let Err(e) = tier.store.delete(&tier.key(v, pack)) {
                    warn!("⚠️ Couldn't delete pack {} of vertex {} from {}: {:#}", pack, v, tier.store.kind(), e);
                }
            }
            None => warn!("⚠️ Pack {} of vertex {} stays on a storage backend that isn't configured any more.", pack, v),
        }
    }

    pub(super) fn has_remote_shards(&self, entry: &Entry) -> bool {
        let vertices = self.vertices.lock().unwrap();
        entry.shards.iter().enumerate().any(|(v, loc)| loc.is_some() && vertices[v].remote.evicted.contains(&loc.pack))
    }

    /// Record a problem setting up the backend (shown in `/status`).
    pub fn note_remote_problem(&self, e: &anyhow::Error) {
        self.note_remote_error(e);
    }

    fn note_remote_error(&self, e: &anyhow::Error) {
        let mut status = self.remote_status.lock().unwrap();
        status.last_error = Some(format!("{:#}", e).chars().take(500).collect());
        status.last_error_at = Some(now());
    }

    pub fn remote_status(&self) -> RemoteStatus {
        let mut status = self.remote_status.lock().unwrap().clone();
        let tiers = self.tiers.read().unwrap();
        status.backend = tiers.0.as_ref().map(|t| t.kind().to_string());
        status.backend_id = tiers.0.as_ref().map(|t| t.id().to_string());
        let current = status.backend_id.clone();
        drop(tiers);
        let vertices = self.vertices.lock().unwrap();
        let (mut uploaded, mut pending, mut remote_only, mut moving, mut local) = (0, 0, 0, 0, 0u64);
        for vertex in vertices.iter() {
            let active = vertex.active.as_ref().map(|a| a.number);
            for (&n, &size) in &vertex.packs {
                let on = vertex.remote.uploaded.get(&n);
                let evicted = vertex.remote.evicted.contains(&n);
                if !evicted {
                    local += size;
                }
                if on.is_some() && on == current.as_ref() {
                    uploaded += 1;
                } else if evicted {
                    moving += 1;
                } else if current.is_some() && Some(n) != active {
                    pending += 1;
                }
                if evicted {
                    remote_only += 1;
                }
            }
        }
        status.uploaded_packs = uploaded;
        status.pending_packs = pending;
        status.remote_only_packs = remote_only;
        status.moving_packs = moving;
        status.local_bytes = local;
        status
    }

    /// One pass of the remote tier: move packs off a previous backend, upload
    /// sealed packs to the current one, then drop local copies beyond
    /// `local_budget` bytes. Holds off sweeps and backups while it runs.
    pub fn remote_step(&self, local_budget: u64, seal_after: std::time::Duration) -> Result<()> {
        let _maintenance = self.maintenance.lock().unwrap();
        let (current, _) = self.tiers.read().unwrap().clone();
        // With a backend, a pack that's quiet for a while is sealed so it can be uploaded.
        if current.is_some() {
            let mut vertices = self.vertices.lock().unwrap();
            for vertex in vertices.iter_mut() {
                let quiet = vertex.active.as_ref().is_some_and(|a| a.size > PACK_HEADER_LEN && a.written.elapsed() >= seal_after);
                if quiet {
                    if let Some(active) = vertex.active.take() {
                        active.file.sync_data().context("syncing a pack before sealing it")?;
                    }
                }
            }
        }
        let current_id = current.as_ref().map(|t| t.id().to_string());

        // 1. Packs kept only on another backend: bring them back here.
        let moving: Vec<(usize, u32, String, u64)> = {
            let vertices = self.vertices.lock().unwrap();
            let mut list = Vec::new();
            for (v, vertex) in vertices.iter().enumerate() {
                for &n in &vertex.remote.evicted {
                    if let Some(backend) = vertex.remote.uploaded.get(&n).filter(|b| Some(*b) != current_id.as_ref()) {
                        list.push((v, n, backend.clone(), vertex.packs.get(&n).copied().unwrap_or(0)));
                    }
                }
            }
            list
        };
        for (v, n, backend, size) in moving {
            if let Err(e) = self.download_pack(&backend, v, n, size) {
                self.note_remote_error(&e);
                warn!("⚠️ Couldn't move pack {} of vertex {} off its old storage backend: {:#}", n, v, e);
            }
        }

        let Some(tier) = current else { return Ok(()) };

        // 2. Upload sealed packs that aren't on the current backend.
        let pending: Vec<(usize, u32, PathBuf)> = {
            let vertices = self.vertices.lock().unwrap();
            let mut list = Vec::new();
            for (v, vertex) in vertices.iter().enumerate() {
                let active = vertex.active.as_ref().map(|a| a.number);
                for &n in vertex.packs.keys() {
                    if Some(n) != active && !vertex.remote.evicted.contains(&n) && vertex.remote.uploaded.get(&n).map(String::as_str) != Some(tier.id()) {
                        list.push((v, n, vertex.dir.clone()));
                    }
                }
            }
            list
        };
        for (v, n, dir) in pending {
            let pack = pack_path(&dir, n);
            let uploaded = write_index(&pack, &index_path(&dir, n)).and_then(|size| tier.store.put_file(&tier.key(v, n), &pack).map(|_| size));
            match uploaded {
                Ok(size) => {
                    let mut vertices = self.vertices.lock().unwrap();
                    let vertex = &mut vertices[v];
                    if vertex.packs.contains_key(&n) {
                        vertex.remote.uploaded.insert(n, tier.id().to_string());
                        vertex.remote.versions.insert(tier.id().to_string(), tier.config_version);
                        vertex.remote.save(&vertex.dir);
                    }
                    let mut status = self.remote_status.lock().unwrap();
                    status.bytes_uploaded += size;
                    status.last_upload_at = Some(now());
                }
                Err(e) => {
                    self.note_remote_error(&e);
                    warn!("⚠️ Couldn't upload pack {} of vertex {} to {}: {:#}", n, v, tier.store.kind(), e);
                    // Try again on the next pass; don't hammer a failing backend now.
                    break;
                }
            }
        }

        // 3. Drop the oldest uploaded packs' local files while over the budget.
        let tier_id = tier.id().to_string();
        loop {
            let pick = {
                let vertices = self.vertices.lock().unwrap();
                let local: u64 = vertices
                    .iter()
                    .flat_map(|vx| vx.packs.iter().filter(|(n, _)| !vx.remote.evicted.contains(n)).map(|(_, s)| *s))
                    .sum();
                if local <= local_budget {
                    None
                } else {
                    vertices
                        .iter()
                        .enumerate()
                        .flat_map(|(v, vx)| {
                            let active = vx.active.as_ref().map(|a| a.number);
                            let tier_id = tier_id.as_str();
                            vx.packs
                                .keys()
                                .filter(move |n| Some(**n) != active && !vx.remote.evicted.contains(n) && vx.remote.uploaded.get(n).map(String::as_str) == Some(tier_id))
                                .map(move |n| (*n, v))
                        })
                        .min()
                }
            };
            let Some((n, v)) = pick else { break };
            let mut vertices = self.vertices.lock().unwrap();
            let vertex = &mut vertices[v];
            vertex.remote.evicted.insert(n);
            vertex.remote.save(&vertex.dir);
            self.handles.lock().unwrap().remove(&(v, n));
            if let Err(e) = fs::remove_file(pack_path(&vertex.dir, n)) {
                warn!("⚠️ Couldn't delete the local copy of pack {} of vertex {}: {}", n, v, e);
            }
        }
        Ok(())
    }

    /// Fetch a whole pack from the backend it was uploaded to and keep it locally again.
    fn download_pack(&self, backend: &str, v: usize, n: u32, size: u64) -> Result<()> {
        let tier = self.tier_for(backend).ok_or_else(|| anyhow!("its backend isn't configured (its settings aren't in the configuration history)"))?;
        let dir = self.vertices.lock().unwrap()[v].dir.clone();
        let tmp = dir.join(format!("{:08}.hxp.part", n));
        {
            let mut file = File::create(&tmp)?;
            let mut offset = 0u64;
            while offset < size {
                let len = DOWNLOAD_PIECE.min((size - offset) as usize);
                let piece = tier.store.get_range(&tier.key(v, n), offset, len)?;
                file.write_all(&piece)?;
                offset += len as u64;
                self.remote_status.lock().unwrap().bytes_downloaded += len as u64;
            }
            file.sync_all()?;
        }
        fs::rename(&tmp, pack_path(&dir, n))?;
        crate::wal::sync_dir(&dir);
        let mut vertices = self.vertices.lock().unwrap();
        let vertex = &mut vertices[v];
        vertex.remote.evicted.remove(&n);
        vertex.remote.uploaded.remove(&n);
        vertex.remote.save(&vertex.dir);
        fs::remove_file(index_path(&dir, n)).ok();
        info!("📦 Moved pack {} of vertex {} back from {}.", n, v, tier.store.kind());
        Ok(())
    }
}
