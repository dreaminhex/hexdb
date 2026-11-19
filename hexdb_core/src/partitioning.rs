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

    /// The partition a document belongs to. System tessellations are all in partition 0.
    pub fn partition_of(&self, tessellation: &str, id: ulid::Ulid) -> u32 {
        if tessellation.starts_with('_') {
            return 0;
        }
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
    /// An invalid version is skipped (and named in the error list), never applied.
    pub fn newest(versions: impl IntoIterator<Item = serde_json::Value>) -> (PartitionMap, Vec<String>) {
        let mut best: Option<PartitionMap> = None;
        let mut errors = Vec::new();
        for doc in versions {
            match serde_json::from_value::<PartitionMap>(doc).map_err(anyhow::Error::from).and_then(|m| m.validate().map(|_| m)) {
                Ok(map) => {
                    if best.as_ref().is_none_or(|b| map.epoch > b.epoch) {
                        best = Some(map);
                    }
                }
                Err(e) => errors.push(format!("{:#}", e)),
            }
        }
        (best.unwrap_or_default(), errors)
    }
}

/// The map this hex routes by, and what it last read.
#[derive(Default)]
pub struct PartitionState {
    map: RwLock<PartitionMap>,
    /// `_partitions`' generation when last read, so an unchanged map isn't read again.
    seen: Mutex<Option<u64>>,
    /// Stored versions that failed validation (never applied).
    rejected: Mutex<Vec<String>>,
}

impl HexDBEngine {
    /// The partition map this hex routes by.
    pub fn partition_map(&self) -> PartitionMap {
        self.partitions.map.read().unwrap().clone()
    }

    /// Stored map versions that failed validation, for `GET /partitions`.
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
    pub async fn refresh_partition_map(&self) {
        if !self.tessellation_exists(PARTITIONS_TESSELLATION) {
            return;
        }
        let generation = self.tessellation_generation(PARTITIONS_TESSELLATION);
        if *self.partitions.seen.lock().unwrap() == Some(generation) {
            return;
        }
        let query = DocumentQuery { filter: Filter::all(), sort: Vec::new(), offset: 0, limit: 10_000, after: None, with_total: false };
        let page = match self.query_documents(PARTITIONS_TESSELLATION, &query).await {
            Ok(page) => page,
            Err(e) => {
                warn!("⚠️ Couldn't read the partition map: {:#}", e);
                return;
            }
        };
        *self.partitions.seen.lock().unwrap() = Some(generation);
        let (newest, errors) = PartitionMap::newest(page.documents.iter().map(|d| d.data_json()));
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
            // System tessellations stay in partition 0 whatever their hash.
            assert_eq!(map.partition_of("_users", id), 0);
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
        let (map, errors) = PartitionMap::newest(vec![PartitionMap::default().to_document(), broken, v2]);
        assert_eq!(map.epoch, 2);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn epochs_are_sent_only_when_every_hex_understands_them() {
        assert!(all_understand_epochs(["2.6.0", "2.6.1", "3.0.0"]));
        assert!(!all_understand_epochs(["2.6.0", "2.5.9"]));
        assert!(all_understand_epochs(std::iter::empty()));
    }
}
