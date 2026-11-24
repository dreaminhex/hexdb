// HexDB Core: point-in-time restore from an S3-compatible store
//
// The other half of `engine::continuous` (see design/point-in-time-restore.md):
// rebuild a data directory as the lattice was at a moment, from the newest
// base shipped at or before it, the WAL segments shipped after that base (the
// last one cut at the moment), and every pack file shipped in that history.
// The folder then starts as a hex, as a restored backup does: recovery replays
// the WAL on top of the base's SSTables.

use crate::crypt::KeyRing;
use crate::engine::continuous::{ShipState, STATE_AAD};
use crate::objstore::{ObjectStore, StoreConfig};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// What a restore rebuilt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreReport {
    pub history: String,
    pub base_sequence: u64,
    pub base_created: i64,
    pub segments: usize,
    /// True if the last segment was cut at `until` (later commits left out).
    pub cut: bool,
    pub packs: usize,
}

/// A pack's objects in the store: the whole sealed pack, or the pieces of an open one by offset.
type PackObjects = (Option<String>, Vec<(u64, String)>);

/// Rebuild `to` (an empty folder) as the lattice was at `until` (epoch milliseconds;
/// None: the newest moment shipped), from `prefix` in the store.
pub async fn restore(store: StoreConfig, prefix: &str, until: Option<i64>, to: &Path, keys: &KeyRing) -> Result<RestoreReport> {
    if to.exists() && std::fs::read_dir(to)?.next().is_some() {
        bail!("{} isn't empty: restore into a new folder", to.display());
    }
    let prefix = prefix.trim_matches('/');
    let store = ObjectStore::new(store)?;
    let until = until.unwrap_or(i64::MAX);

    // Every history's shipped state, and the newest base at or before `until` across them.
    let mut best: Option<(String, u64, i64)> = None;
    let mut oldest: Option<i64> = None;
    for object in store.list(&format!("{}/history/", prefix)).await? {
        let Some(rest) = object.key.strip_prefix(&format!("{}/history/", prefix)) else { continue };
        let Some((history, "state.hxe")) = rest.split_once('/') else { continue };
        let Some(sealed) = store.get(&object.key).await? else { continue };
        let plain = keys.decrypt_any(&sealed, STATE_AAD).context("the shipped state can't be read: is this the lattice's storage key?")?;
        let state: ShipState = serde_json::from_slice(&plain)?;
        for base in &state.bases {
            oldest = Some(oldest.map_or(base.created, |o| o.min(base.created)));
            if base.created <= until && best.as_ref().is_none_or(|(_, _, created)| base.created > *created) {
                best = Some((history.to_string(), base.sequence, base.created));
            }
        }
    }
    let Some((history, base_sequence, base_created)) = best else {
        return Err(match oldest {
            Some(o) => anyhow!("nothing shipped is old enough: the oldest base is from {} (epoch ms)", o),
            None => anyhow!("there's nothing shipped under {}", prefix),
        });
    };
    let root = format!("{}/history/{}", prefix, history);
    std::fs::create_dir_all(to)?;

    // The base.
    let base_prefix = format!("{}/base/{:020}/", root, base_sequence);
    let mut manifest = false;
    for object in store.list(&base_prefix).await? {
        let rel = object.key.trim_start_matches(&base_prefix);
        manifest |= rel == crate::engine::backup::BACKUP_MANIFEST;
        write(&to.join(rel), store.get(&object.key).await?.ok_or_else(|| anyhow!("{} vanished", object.key))?)?;
    }
    if !manifest {
        bail!("the base at sequence {} is incomplete (no manifest)", base_sequence);
    }

    // Segments after it, in order; the one holding `until` is cut there, later ones aren't needed.
    let mut segments: Vec<(u64, String)> = store
        .list(&format!("{}/wal/", root))
        .await?
        .into_iter()
        .filter_map(|o| o.key.rsplit('/').next()?.strip_suffix(".wal")?.parse::<u64>().ok().map(|s| (s, o.key)))
        .filter(|(s, _)| *s > base_sequence)
        .collect();
    segments.sort();
    let (mut count, mut cut) = (0, false);
    for (seq, key) in segments {
        let path = to.join("wal").join(format!("{:020}.wal", seq));
        write(&path, store.get(&key).await?.ok_or_else(|| anyhow!("{} vanished", key))?)?;
        count += 1;
        if crate::wal::cut_after_time(&path, keys, until)? {
            cut = true;
            break;
        }
    }

    // Every pack of the history: sealed ones whole, open ones from their pieces.
    let packs_prefix = format!("{}/packs/", root);
    let mut pieces: BTreeMap<String, PackObjects> = BTreeMap::new();
    for object in store.list(&packs_prefix).await? {
        let rel = object.key.trim_start_matches(&packs_prefix).to_string();
        match rel.rsplit_once('.').and_then(|(name, off)| (off.len() == 12).then(|| off.parse::<u64>().ok().map(|o| (name.to_string(), o))).flatten()) {
            Some((name, offset)) => pieces.entry(name).or_default().1.push((offset, object.key)),
            None => pieces.entry(rel).or_default().0 = Some(object.key),
        }
    }
    let packs = pieces.len();
    for (name, (whole, mut parts)) in pieces {
        let bytes = match whole {
            Some(key) => store.get(&key).await?.ok_or_else(|| anyhow!("{} vanished", key))?,
            None => {
                parts.sort();
                let mut bytes = Vec::new();
                for (offset, key) in parts {
                    if offset != bytes.len() as u64 {
                        bail!("pack {} is missing bytes {}..{} in the store", name, bytes.len(), offset);
                    }
                    bytes.extend(store.get(&key).await?.ok_or_else(|| anyhow!("{} vanished", key))?);
                }
                bytes
            }
        };
        write(&to.join("blobs").join(&name), bytes)?;
    }

    // A history of its own, so replicas and plugins don't take it for the original's.
    if let Some(mut catalog) = crate::catalog::Catalog::load(to, keys)? {
        catalog.history_id = ulid::Ulid::generate().to_string();
        catalog.save(to, keys)?;
    }
    Ok(RestoreReport { history, base_sequence, base_created, segments: count, cut, packs })
}

fn write(path: &Path, bytes: Vec<u8>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes).with_context(|| format!("couldn't write {}", path.display()))
}
