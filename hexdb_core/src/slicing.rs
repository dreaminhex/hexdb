// HexDB Core Slicing
//
// With `replication.slicing = true`, replicas keep only part of the data.
// Every document of a user tessellation falls in one of 64 slices (a hash of
// its tessellation and ID). The Overseer keeps every slice, and each slice is
// kept by `replication.slice_copies` replicas (1 by default), so any one hex
// can fail without losing data. With a single replica, it keeps everything.
// System tessellations (users, roles, the audit trail, ...) are never sliced.
//
// Every hex computes the same placement from the same live hexes, by
// rendezvous hashing: each slice goes to the replicas that score highest for
// it. When a hex joins or leaves, only the slices it gains or loses move.
//
// A replica gives up a slice only once another live replica holds it, so a
// slice never has fewer copies while a new replica catches up.

use std::collections::BTreeMap;

/// Number of slices in a lattice.
pub const SLICES: u32 = 64;

/// A set of slices, one bit each.
pub type SliceMask = u64;

/// Every slice.
pub const ALL_SLICES: SliceMask = u64::MAX;

fn hash64(parts: &[&[u8]]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().expect("8 bytes"))
}

/// The slice a document belongs to.
pub fn slice_of(tessellation: &str, id: ulid::Ulid) -> u32 {
    (hash64(&[tessellation.as_bytes(), &id.to_bytes()]) % SLICES as u64) as u32
}

/// True if `mask` holds `slice`.
pub fn holds(mask: SliceMask, slice: u32) -> bool {
    mask & (1u64 << slice) != 0
}

/// Number of slices in a mask.
pub fn count(mask: SliceMask) -> u32 {
    mask.count_ones()
}

/// The slices each replica should keep: `copies` replicas per slice (at most
/// all of them). Replica IDs in any order; the answer doesn't depend on it.
pub fn placement(replicas: &[String], copies: usize) -> BTreeMap<String, SliceMask> {
    let mut out: BTreeMap<String, SliceMask> = replicas.iter().map(|r| (r.clone(), 0)).collect();
    if replicas.is_empty() {
        return out;
    }
    let copies = copies.clamp(1, replicas.len());
    for slice in 0..SLICES {
        let mut scored: Vec<(u64, &String)> = replicas.iter().map(|r| (hash64(&[b"slice", &slice.to_le_bytes(), r.as_bytes()]), r)).collect();
        // Highest score first; the ID breaks (unlikely) ties.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        for (_, replica) in scored.into_iter().take(copies) {
            *out.get_mut(replica).expect("listed") |= 1u64 << slice;
        }
    }
    out
}

/// Slice numbers in a mask, for messages.
pub fn list(mask: SliceMask) -> Vec<u32> {
    (0..SLICES).filter(|s| holds(mask, *s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("01HEX{:021}", i)).collect()
    }

    #[test]
    fn slices_are_stable_and_spread() {
        let id = ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(slice_of("orders", id), slice_of("orders", id));
        let mut seen = [0u32; SLICES as usize];
        for i in 0..6400u128 {
            seen[slice_of("orders", ulid::Ulid::from(i)) as usize] += 1;
        }
        // Each slice gets a fair share (100 expected).
        assert!(seen.iter().all(|n| (50..=160).contains(n)), "{:?}", seen);
    }

    #[test]
    fn placement_covers_every_slice_with_the_copies_asked_for() {
        for replicas in 1..6 {
            for copies in 1..4 {
                let placed = placement(&ids(replicas), copies);
                for slice in 0..SLICES {
                    let holders = placed.values().filter(|m| holds(**m, slice)).count();
                    assert_eq!(holders, copies.min(replicas), "{} replicas, {} copies, slice {}", replicas, copies, slice);
                }
            }
        }
        // One replica keeps everything; two split the slices.
        assert_eq!(placement(&ids(1), 1).values().next(), Some(&ALL_SLICES));
        let two = placement(&ids(2), 1);
        assert!(two.values().all(|m| (20..=44).contains(&count(*m))), "{:?}", two);
    }

    #[test]
    fn placement_moves_little_when_a_replica_joins() {
        let before = placement(&ids(3), 1);
        let after = placement(&ids(4), 1);
        // Slices only move to the new replica, never between the old ones.
        for (id, mask) in &before {
            assert_eq!(after[id] & !mask, 0, "{} gained slices it didn't have", id);
        }
        // And the order the replicas are listed in doesn't matter.
        let mut reversed = ids(4);
        reversed.reverse();
        assert_eq!(placement(&reversed, 1), after);
    }
}
