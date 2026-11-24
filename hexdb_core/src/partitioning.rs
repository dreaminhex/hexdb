// HexDB Core Partitioning: the partition map
//
// Partitioned writes (design/partitioning.md) split the 64 slices into groups,
// each with its own Overseer. This is stage 1: the map, its epoch, and routing
// through it, with one partition holding every slice. Nothing behaves
// differently yet; later stages add partitions and move slices between them.
//
// The map says which partition owns each slice and which hexes form each
// partition's group. Every change gets a higher epoch. Versions are documents
// in the `_partitions` system tessellation (partition 0 holds every system
// tessellation), so they replicate to every hex and survive failover like any
// other system data. A lattice that never wrote one has the implicit map:
// partition 0, every slice, epoch 1. Upgrading from 2.x therefore writes nothing.
//
// With `partitioning.enabled`, a hex that forwards a request to a partition's
// Overseer sends the epoch of the map it routed by, covered by the lattice
// signature. An Overseer whose map has another epoch refuses the request before
// doing anything (421 `partition_epoch`), so the forwarding hex can reload its
// map and send it again without risk of running it twice. Hexes only send the
// epoch once every live hex runs a version that understands it; older ones
// would reject the signature.

use crate::engine::{DocumentQuery, HexDBEngine};
use crate::filter::Filter;
use crate::slicing::{SliceMask, ALL_SLICES, SLICES};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, RwLock};
use tracing::{info, warn};

/// System tessellation holding the versions of the partition map.
pub const PARTITIONS_TESSELLATION: &str = "_partitions";
/// Header carrying the epoch a forwarded request was routed by.
pub const PARTITION_EPOCH_HEADER: &str = "x-hexdb-partition-epoch";
/// Error code of a request routed by another epoch than the receiving Overseer's.
pub const EPOCH_MISMATCH_CODE: &str = "partition_epoch";
/// The first version that understands partition epochs on forwarded requests.
pub const FIRST_VERSION: &str = "2.6.0";
/// The first version with partition groups: a map with several partitions needs every hex on it.
pub const STAGE_2_VERSION: &str = "2.7.0";

/// One partition: the slices it owns and the hexes of its group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub id: u32,
    /// The slices this partition owns, one bit each.
    pub slices: SliceMask,
    /// IDs of the hexes in this partition's group. Empty in the implicit map:
    /// every hex of the lattice.
    #[serde(default)]
    pub hexes: Vec<String>,
}

/// Which partition owns each slice, at an epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionMap {
    pub epoch: u64,
    pub partitions: Vec<Partition>,
}

impl Default for PartitionMap {
    /// The implicit map of every lattice that hasn't partitioned: one partition, every slice.
    fn default() -> Self {
        PartitionMap { epoch: 1, partitions: vec![Partition { id: 0, slices: ALL_SLICES, hexes: Vec::new() }] }
    }
}

impl PartitionMap {
    /// Checks that every slice belongs to exactly one partition, that partition 0
    /// exists (it holds the system tessellations), and that IDs and hexes are unique.
    pub fn validate(&self) -> Result<()> {
        if self.epoch == 0 {
            bail!("A partition map's epoch starts at 1.");
        }
        if !self.partitions.iter().any(|p| p.id == 0) {
            bail!("Partition 0 is missing; it holds the system tessellations.");
        }
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut seen_hexes = std::collections::BTreeSet::new();
        let mut covered: SliceMask = 0;
        for p in &self.partitions {
            if !seen_ids.insert(p.id) {
                bail!("Partition {} is listed twice.", p.id);
            }
            if p.slices == 0 {
                bail!("Partition {} owns no slices.", p.id);
            }
            if covered & p.slices != 0 {
                bail!("Slices {:?} belong to more than one partition.", crate::slicing::list(covered & p.slices));
            }
            covered |= p.slices;
            for hex in &p.hexes {
                // One partition per hex (design decision 1).
                if !seen_hexes.insert(hex.as_str()) {
                    bail!("Hex {} is in more than one partition.", hex);
                }
            }
        }
        if covered != ALL_SLICES {
            bail!("Slices {:?} belong to no partition.", crate::slicing::list(!covered));
        }
        Ok(())
    }

    /// The partition owning `slice`. A valid map always has one; partition 0 otherwise.
    pub fn partition_of_slice(&self, slice: u32) -> u32 {
        debug_assert!(slice < SLICES);
        self.partitions.iter().find(|p| crate::slicing::holds(p.slices, slice)).map_or(0, |p| p.id)
    }

    /// The partition a document of a user tessellation belongs to, by its slice. System
    /// tessellations aren't placed by hash: see [`HexDBEngine::placement`].
    pub fn partition_of(&self, tessellation: &str, id: ulid::Ulid) -> u32 {
        self.partition_of_slice(crate::slicing::slice_of(tessellation, id))
    }

    /// The partition a hex belongs to. In the implicit map (no hexes listed), every hex is in partition 0.
    pub fn partition_of_hex(&self, hex: &str) -> Option<u32> {
        if self.partitions.len() == 1 && self.partitions[0].hexes.is_empty() {
            return Some(self.partitions[0].id);
        }
        self.partitions.iter().find(|p| p.hexes.iter().any(|h| h == hex)).map(|p| p.id)
    }

    /// True while the whole lattice is one partition (every lattice until stage 2).
    pub fn is_single(&self) -> bool {
        self.partitions.len() == 1
    }

    /// The map as stored in `_partitions`: one document per epoch.
    pub fn to_document(&self) -> serde_json::Value {
        serde_json::json!({ "epoch": self.epoch, "partitions": self.partitions })
    }

    /// The newest valid map among stored versions; the implicit map if there are none.
    /// An invalid version is skipped (and named in the error list), never applied, and
    /// so is an epoch two different versions claim: no hex can know which one is meant.
    pub fn newest(versions: impl IntoIterator<Item = serde_json::Value>) -> (PartitionMap, Vec<String>) {
        let mut by_epoch: std::collections::BTreeMap<u64, Vec<PartitionMap>> = std::collections::BTreeMap::new();
        let mut errors = Vec::new();
        for doc in versions {
            match serde_json::from_value::<PartitionMap>(doc).map_err(anyhow::Error::from).and_then(|m| m.validate().map(|_| m)) {
                Ok(map) => by_epoch.entry(map.epoch).or_default().push(map),
                Err(e) => errors.push(format!("{:#}", e)),
            }
        }
        for (epoch, maps) in by_epoch.into_iter().rev() {
            if maps.iter().all(|m| *m == maps[0]) {
                return (maps.into_iter().next().unwrap_or_default(), errors);
            }
            errors.push(format!("epoch {} has {} different versions; none is applied", epoch, maps.len()));
        }
        (PartitionMap::default(), errors)
    }
}

/// The map this hex routes by, and what it last read.
#[derive(Default)]
pub struct PartitionState {
    map: RwLock<PartitionMap>,
    /// `_partitions`' generation when last read, so an unchanged map isn't read again.
    seen: Mutex<Option<u64>>,
    /// Set while a new map is being committed: user writes are refused meanwhile
    /// (checked under the commit lock), so none slips in between the emptiness check
    /// and the new map.
    pub(crate) changing: std::sync::atomic::AtomicBool,
    /// Stored versions that failed validation (never applied).
    rejected: Mutex<Vec<String>>,
}

/// System tessellations whose records are written in the same commit as a user
/// document (idempotency records, vault entries, the trash and delete history), or
/// that hold what runs per partition (plugin, trigger and stream source cursors).
/// Each record stays in the partition that wrote it, so a commit stays one commit in
/// one partition's WAL (design/partitioning-stage-2.md).
pub const PARTITION_LOCAL_TESSELLATIONS: &[&str] = &[
    crate::engine::writes::IDEMPOTENCY_TESSELLATION,
    crate::tokens::VAULT_TESSELLATION,
    crate::engine::deletes::TRASH_TESSELLATION,
    crate::engine::deletes::DELETES_TESSELLATION,
    crate::files::TRASH_TESSELLATION,
    crate::plugins::PLUGIN_CURSORS_TESSELLATION,
    crate::triggers::TRIGGER_CURSORS_TESSELLATION,
    crate::streams::STREAM_OFFSETS_TESSELLATION,
    crate::files::UPLOADS_TESSELLATION,
];

/// System tessellations each hex writes for itself in the stage-2 preview (its own
/// audit entries, sign-in failures and AI usage), rather than getting them from
/// partition 0: a hex outside partition 0 keeps its own.
pub const HEX_WRITTEN_TESSELLATIONS: &[&str] = &[crate::audit::AUDIT_TESSELLATION, crate::auth::THROTTLE_TESSELLATION, crate::ai::AI_USAGE_TESSELLATION];

/// Where a record belongs in a partitioned lattice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// In this partition: a user document by its slice, or a lattice-wide system
    /// record (users, roles, keys, the configuration...) in partition 0.
    Partition(u32),
    /// In whichever partition wrote it: see [`PARTITION_LOCAL_TESSELLATIONS`].
    WithItsWrite,
}

impl HexDBEngine {
    #[cfg(test)]
    pub(crate) fn set_partition_map_for_test(&self, map: PartitionMap) {
        *self.partitions.map.write().unwrap() = map;
    }

    /// This hex's partition: Some(0) on a one-partition lattice, None for a hex a
    /// multi-partition map doesn't place (it holds nothing and is never elected).
    pub fn my_partition(&self) -> Option<u32> {
        self.partitions.map.read().unwrap().partition_of_hex(&self.id.to_string())
    }

    /// True if `hex_id` is in this hex's group: every hex on a one-partition lattice;
    /// otherwise the hexes the map puts in this hex's partition. Elections, quorum,
    /// replication and handovers happen within a group (design/partitioning-stage-2.md).
    pub fn in_my_group(&self, hex_id: &str) -> bool {
        let map = self.partitions.map.read().unwrap();
        if map.is_single() {
            return true;
        }
        match map.partition_of_hex(&self.id.to_string()) {
            Some(mine) => map.partition_of_hex(hex_id) == Some(mine),
            None => false,
        }
    }

    /// The slices this hex's partition owns: every slice on a one-partition lattice
    /// (every lattice until stage 2), its partition's mask otherwise, and none for a hex
    /// the map doesn't place. "Holds all it should" means holding these, not all 64.
    pub fn owned_slices(&self) -> SliceMask {
        let map = self.partitions.map.read().unwrap();
        if map.is_single() {
            return ALL_SLICES;
        }
        map.partition_of_hex(&self.id.to_string()).and_then(|id| map.partitions.iter().find(|p| p.id == id)).map_or(0, |p| p.slices)
    }

    /// Lattice-wide system tessellations every hex gets from partition 0's Overseer
    /// (users, roles, API keys, the configuration, the partition map...): system ones,
    /// but not those that travel with their write, aren't replicated at all, or that
    /// each hex writes for itself in this preview.
    pub fn in_system_scope(&self, tessellation: &str) -> bool {
        self.is_system_tessellation(tessellation)
            && !PARTITION_LOCAL_TESSELLATIONS.contains(&tessellation)
            && !HEX_WRITTEN_TESSELLATIONS.contains(&tessellation)
            && !crate::engine::is_local_only(tessellation)
    }

    /// True when this hex gets `tessellation` from partition 0's system stream instead of
    /// its own group: on a multi-partition lattice, outside partition 0. Its group's feed
    /// and snapshot leave it out, and a group full sync never drops or thins it out.
    pub fn from_system_stream(&self, tessellation: &str) -> bool {
        let outside_zero = {
            let map = self.partitions.map.read().unwrap();
            !map.is_single() && map.partition_of_hex(&self.id.to_string()) != Some(0)
        };
        outside_zero && self.in_system_scope(tessellation)
    }

    /// Why a write can't be committed here, if one of its user documents belongs to a
    /// partition this hex isn't in. Always None on a one-partition lattice.
    pub(crate) fn misplaced_write(&self, items: &[crate::engine::writes::BatchItem]) -> Option<String> {
        let map = self.partitions.map.read().unwrap();
        if map.is_single() {
            return None;
        }
        let mine = map.partition_of_hex(&self.id.to_string());
        for item in items {
            let tess = &item.key.tessellation;
            // Lattice-wide system records are partition 0's: a hex outside it has them
            // only as copies (its system stream would remove one written here).
            if mine != Some(0) && self.in_system_scope(tess) {
                return Some(format!("'{}' is kept by partition 0, which this hex isn't in. Nothing was written; send it to a hex of partition 0.", tess));
            }
            if PARTITION_LOCAL_TESSELLATIONS.contains(&tess.as_str()) || self.is_system_tessellation(tess) {
                continue;
            }
            let owner = map.partition_of(tess, item.key.id);
            if mine != Some(owner) {
                return Some(format!("This document belongs to partition {}, which this hex isn't in, and no hex of it could take the write here. Nothing was written; try again.", owner));
            }
        }
        None
    }

    /// A new document ID for `tessellation`. On a partitioned lattice it's drawn until it
    /// falls in a slice this hex's partition owns, so an insert commits on the hex that
    /// received it (its group's Overseer) and never has to cross to another partition;
    /// placement stays by hash. Two partitions take about two draws.
    pub fn new_document_id(&self, tessellation: &str) -> ulid::Ulid {
        let owned = self.owned_slices();
        if owned == ALL_SLICES || owned == 0 || self.is_system_tessellation(tessellation) {
            return ulid::Ulid::generate();
        }
        loop {
            let id = ulid::Ulid::generate();
            if crate::slicing::holds(owned, crate::slicing::slice_of(tessellation, id)) {
                return id;
            }
        }
    }

    /// Where a record of `tessellation` with `id` belongs. System tessellations are
    /// recognized as the engine does (the `_` prefix, `users` and `roles`, or a catalog
    /// kind of `system`), never by hash.
    pub fn placement(&self, tessellation: &str, id: ulid::Ulid) -> Placement {
        if PARTITION_LOCAL_TESSELLATIONS.contains(&tessellation) {
            Placement::WithItsWrite
        } else if self.is_system_tessellation(tessellation) {
            Placement::Partition(0)
        } else {
            Placement::Partition(self.partitions.map.read().unwrap().partition_of(tessellation, id))
        }
    }

    /// The partition map this hex routes by.
    pub fn partition_map(&self) -> PartitionMap {
        self.partitions.map.read().unwrap().clone()
    }

    /// Stored map versions that failed validation, for `GET /status/partitions`.
    pub fn rejected_partition_maps(&self) -> Vec<String> {
        self.partitions.rejected.lock().unwrap().clone()
    }

    /// True when this hex sends partition epochs on forwarded requests:
    /// `partitioning.enabled`, and every live hex understands them.
    pub async fn sends_partition_epochs(&self) -> bool {
        if !self.config.partitioning.enabled {
            return false;
        }
        let peers = self.peers.lock().await;
        // A hex on this hex's own version runs this code (before the version number reaches FIRST_VERSION, in development).
        all_understand_epochs(peers.iter().filter(|p| p.status == "active" && p.hex.version != self.version).map(|p| p.hex.version.as_str()))
    }

    /// Reads the newest map version if `_partitions` changed since the last look.
    /// Cheap when nothing changed; called at startup and by the configuration task.
    /// The epoch never goes back: a version older than the one in use is ignored.
    /// Commits a new partition map (epoch + 1), on the Overseer of partition 0. A map with
    /// several partitions is accepted only while the lattice holds no user documents:
    /// stage 2 doesn't move data (decision 2026-11-23), so a slice that changed partitions
    /// would leave its documents behind. Every hex must understand partitions first.
    pub async fn set_partition_map(&self, partitions: Vec<Partition>) -> Result<PartitionMap> {
        use std::sync::atomic::Ordering;
        self.ensure_writable()?;
        if self.my_partition() != Some(0) {
            bail!(crate::engine::EngineError::Invalid("The partition map is changed on the Overseer of partition 0.".into()));
        }
        if !self.config.partitioning.enabled {
            bail!(crate::engine::EngineError::Invalid("Partitioning is off (partitioning.enabled); a preview in this version.".into()));
        }
        if !self.all_understand_partitions().await {
            bail!(crate::engine::EngineError::Conflict(format!("Every hex must run HexDB {} or later before the partition map can change.", STAGE_2_VERSION)));
        }
        let current = self.partition_map();
        let next = PartitionMap { epoch: current.epoch + 1, partitions };
        next.validate().map_err(|e| crate::engine::EngineError::Invalid(format!("{:#}", e)))?;
        if self.partitions.changing.swap(true, Ordering::AcqRel) {
            bail!(crate::engine::EngineError::Conflict("Another partition map change is in progress.".into()));
        }
        let result = async {
            // A barrier: a user write that passed the check before the flag was set has
            // finished (it held the lock); every later one is refused until the map is in.
            self.commit_barrier().await;
            let slices_move = next.partitions.len() > 1 || !current.is_single();
            if slices_move {
                for (name, _) in self.tessellation_details() {
                    if !self.is_system_tessellation(&name) && self.count_documents(&name).await? > 0 {
                        bail!(crate::engine::EngineError::Conflict(format!(
                            "The lattice holds documents (in '{}'): until partitions can move data (a later 2.x stage), a lattice is split only while it's empty.",
                            name
                        )));
                    }
                }
            }
            self.ensure_system_tessellation(PARTITIONS_TESSELLATION)?;
            self.put_system_document(PARTITIONS_TESSELLATION, ulid::Ulid::generate(), next.to_document(), None).await?;
            Ok(())
        }
        .await;
        self.partitions.changing.store(false, Ordering::Release);
        result?;
        self.refresh_partition_map().await;
        Ok(self.partition_map())
    }

    /// True when every active hex runs a version with partition groups (stage 2).
    async fn all_understand_partitions(&self) -> bool {
        let peers = self.peers.lock().await;
        // A hex on this hex's own version runs this code (before the version number reaches STAGE_2_VERSION, in development).
        peers.iter().filter(|p| p.status == "active" && p.hex.version != self.version).all(|p| !crate::updates::is_newer(STAGE_2_VERSION, &p.hex.version))
    }

    pub async fn refresh_partition_map(&self) {
        if !self.tessellation_exists(PARTITIONS_TESSELLATION) {
            return;
        }
        let generation = self.tessellation_generation(PARTITIONS_TESSELLATION);
        if *self.partitions.seen.lock().unwrap() == Some(generation) {
            return;
        }
        // Every version, page by page: the newest can be anywhere in ID order.
        let mut versions = Vec::new();
        let mut after = None;
        loop {
            let query = DocumentQuery { filter: Filter::all(), sort: Vec::new(), offset: 0, limit: 10_000, after, with_total: false };
            let page = match self.query_documents(PARTITIONS_TESSELLATION, &query).await {
                Ok(page) => page,
                Err(e) => {
                    warn!("⚠️ Couldn't read the partition map: {:#}", e);
                    return;
                }
            };
            versions.extend(page.documents.iter().map(|d| d.data_json()));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        *self.partitions.seen.lock().unwrap() = Some(generation);
        let (newest, errors) = PartitionMap::newest(versions);
        for e in &errors {
            warn!("⚠️ Ignoring an invalid partition map version: {}", e);
        }
        *self.partitions.rejected.lock().unwrap() = errors;
        let mut map = self.partitions.map.write().unwrap();
        if newest.epoch > map.epoch {
            info!("🗺️ Partition map epoch {} ({} partition{}).", newest.epoch, newest.partitions.len(), if newest.partitions.len() == 1 { "" } else { "s" });
            *map = newest;
        }
    }
}

/// True when every given hex version understands partition epochs, so it's safe to send them.
pub fn all_understand_epochs<'a>(versions: impl IntoIterator<Item = &'a str>) -> bool {
    versions.into_iter().all(|v| !crate::updates::is_newer(FIRST_VERSION, v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn system_tessellations_are_never_placed_by_hash() {
        let (engine, _dir) = crate::engine::test_engine().await;
        let low: SliceMask = (1u64 << 32) - 1;
        *engine.partitions.map.write().unwrap() = PartitionMap { epoch: 2, partitions: vec![Partition { id: 0, slices: low, hexes: vec![] }, Partition { id: 1, slices: !low, hexes: vec!["b".into()] }] };
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let id = ulid::Ulid::generate();
            // users and roles have no '_' prefix, but they're lattice-wide system tessellations.
            for system in ["users", "roles", "_api_keys", "_config", "_partitions"] {
                assert_eq!(engine.placement(system, id), Placement::Partition(0), "{}", system);
            }
            for local in PARTITION_LOCAL_TESSELLATIONS {
                assert_eq!(engine.placement(local, id), Placement::WithItsWrite, "{}", local);
            }
            if let Placement::Partition(p) = engine.placement("orders", id) {
                seen.insert(p);
            }
        }
        assert_eq!(seen.len(), 2, "user documents spread over both partitions");
        // This hex isn't in the map: it owns nothing. Placed in partition 1, it owns that mask.
        assert_eq!(engine.owned_slices(), 0);
        engine.partitions.map.write().unwrap().partitions[1].hexes.push(engine.id.to_string());
        assert_eq!(engine.owned_slices(), !low);
        *engine.partitions.map.write().unwrap() = PartitionMap::default();
        assert_eq!(engine.owned_slices(), ALL_SLICES);
    }

    #[test]
    fn the_implicit_map_puts_everything_in_partition_zero() {
        let map = PartitionMap::default();
        map.validate().unwrap();
        assert!(map.is_single());
        assert_eq!(map.epoch, 1);
        for _ in 0..100 {
            assert_eq!(map.partition_of("orders", ulid::Ulid::generate()), 0);
        }
        assert_eq!(map.partition_of_hex("any-hex"), Some(0));
    }

    #[test]
    fn routing_follows_the_slice() {
        let low: SliceMask = (1u64 << 32) - 1;
        let map = PartitionMap { epoch: 2, partitions: vec![Partition { id: 0, slices: low, hexes: vec!["a".into()] }, Partition { id: 1, slices: !low, hexes: vec!["b".into()] }] };
        map.validate().unwrap();
        for _ in 0..200 {
            let id = ulid::Ulid::generate();
            let slice = crate::slicing::slice_of("orders", id);
            assert_eq!(map.partition_of("orders", id), u32::from(slice >= 32));
        }
        assert_eq!(map.partition_of_hex("b"), Some(1));
        assert_eq!(map.partition_of_hex("c"), None);
    }

    #[test]
    fn invalid_maps_are_refused() {
        let half: SliceMask = (1u64 << 32) - 1;
        let gap = PartitionMap { epoch: 2, partitions: vec![Partition { id: 0, slices: half, hexes: vec![] }] };
        assert!(gap.validate().unwrap_err().to_string().contains("belong to no partition"));
        let overlap = PartitionMap { epoch: 2, partitions: vec![Partition { id: 0, slices: ALL_SLICES, hexes: vec![] }, Partition { id: 1, slices: 1, hexes: vec![] }] };
        assert!(overlap.validate().unwrap_err().to_string().contains("more than one partition"));
        let no_zero = PartitionMap { epoch: 2, partitions: vec![Partition { id: 1, slices: ALL_SLICES, hexes: vec![] }] };
        assert!(no_zero.validate().is_err());
        let shared_hex = PartitionMap { epoch: 2, partitions: vec![Partition { id: 0, slices: half, hexes: vec!["a".into()] }, Partition { id: 1, slices: !half, hexes: vec!["a".into()] }] };
        assert!(shared_hex.validate().unwrap_err().to_string().contains("more than one partition"));
        assert!(PartitionMap { epoch: 0, ..PartitionMap::default() }.validate().is_err());
    }

    #[test]
    fn the_newest_valid_version_wins() {
        let (map, errors) = PartitionMap::newest(Vec::new());
        assert_eq!(map, PartitionMap::default());
        assert!(errors.is_empty());
        let v2 = json!({ "epoch": 2, "partitions": [{ "id": 0, "slices": u64::MAX }] });
        let broken = json!({ "epoch": 9, "partitions": [{ "id": 0, "slices": 1 }] });
        let (map, errors) = PartitionMap::newest(vec![PartitionMap::default().to_document(), broken, v2.clone()]);
        assert_eq!(map.epoch, 2);
        assert_eq!(errors.len(), 1);

        // Two different versions at one epoch: that epoch is refused, the one below applies.
        let v3a = json!({ "epoch": 3, "partitions": [{ "id": 0, "slices": u64::MAX }] });
        let v3b = json!({ "epoch": 3, "partitions": [{ "id": 0, "slices": u64::MAX, "hexes": ["h1"] }] });
        let (map, errors) = PartitionMap::newest(vec![v2.clone(), v3a.clone(), v3b]);
        assert_eq!(map.epoch, 2);
        assert!(errors.iter().any(|e| e.contains("epoch 3")), "{:?}", errors);
        // The same version stored twice is no conflict.
        let (map, errors) = PartitionMap::newest(vec![v2, v3a.clone(), v3a]);
        assert_eq!((map.epoch, errors.len()), (3, 0));
    }

    #[test]
    fn epochs_are_sent_only_when_every_hex_understands_them() {
        assert!(all_understand_epochs(["2.6.0", "2.6.1", "3.0.0"]));
        assert!(!all_understand_epochs(["2.6.0", "2.5.9"]));
        assert!(all_understand_epochs(std::iter::empty()));
    }
}
