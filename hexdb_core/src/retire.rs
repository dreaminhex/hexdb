// HexDB Core Retire: taking hexes out of a lattice for good, without losing data
//
// An administrator marks a hex as retiring (`POST /retire/{hex}`, on the
// Overseer; by ID, name or Fly.io machine ID) before its machine is removed: a hosted lattice moving to another
// region or size adds new hexes, then retires the old ones. The set is kept in
// the `_retiring_hexes` system tessellation, so every hex sees it, and each
// hex mirrors it in memory once per discovery round.
//
// A retiring hex:
// - leaves slice placement: the other replicas take its slices (copying them
//   from the Overseer) before it gives any up, and other replicas don't count
//   it as a holder when deciding whether a slice of theirs is safe to drop;
// - stops being a candidate for Overseer (it advertises the replicant
//   preference) once it isn't the Overseer;
// - if it's the Overseer, hands the role over first: a successor is chosen the
//   same way on every hex (a hex that prefers to lead, then the one holding
//   the most slices, then the lowest ID), copies every slice, and once it has
//   every change the Overseer steps down to it (`updates::hand_over`).
//
// `removable` (in `/status` and `GET /retire`) turns true once the hex is no
// longer the Overseer and every slice it holds is kept by enough other
// replicas that aren't retiring (`replication.slice_copies`, or all of them if
// fewer): then its machine and disk can go. A retirement can be cancelled
// (`DELETE /retire/{hex}`) until then.

use crate::network::discovery::{PeerHex, ROLE_ORACLE, ROLE_OVERSEER};
use crate::slicing::{self, SliceMask, ALL_SLICES};
use crate::HexDBEngine;
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::RwLock;
use tracing::{info, warn};
use ulid::Ulid;

/// Where the retiring hexes are listed (one document per hex, its ID as the document's).
pub const RETIRING_TESSELLATION: &str = "_retiring_hexes";

/// This hex's copy of the set, refreshed every discovery round.
static RETIRING: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// True if the hex with this ID is retiring.
pub fn is_retiring(id: &str) -> bool {
    RETIRING.read().unwrap().contains(id)
}

/// Read the set from the lattice's data into memory.
pub async fn refresh(engine: &HexDBEngine) {
    if !engine.tessellation_exists(RETIRING_TESSELLATION) {
        RETIRING.write().unwrap().clear();
        return;
    }
    match engine.matching_documents(RETIRING_TESSELLATION, &crate::filter::Filter::all()).await {
        Ok(docs) => *RETIRING.write().unwrap() = docs.iter().map(|d| d.id.to_string()).collect(),
        Err(e) => warn!("⚠️ Couldn't read the retiring hexes: {:#}", e),
    }
}

/// One hex as retirement sees it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RetiringHex {
    pub id: String,
    pub name: String,
    pub role: String,
    /// "active", "lost", or "unknown" (not heard from since it was retired).
    pub status: String,
    /// Its machine and disk can go: it isn't the Overseer and its slices are kept elsewhere.
    pub removable: bool,
    /// Why not yet, when it isn't removable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

/// The hexes of the lattice with their status: this one, then its peers.
async fn members(engine: &HexDBEngine) -> Vec<(PeerHex, String)> {
    let mut out = vec![(crate::network::discovery::local_identity(engine).await, "active".to_string())];
    out.extend(engine.peers.lock().await.iter().map(|p| (p.hex.clone(), p.status.clone())));
    out
}

/// The hex that takes over from a retiring Overseer: among active hexes that
/// aren't the Overseer, the Oracle, retiring or unwilling to lead, one that
/// prefers to lead, then the one holding the most slices, then the lowest ID.
pub fn successor<'a>(hexes: impl IntoIterator<Item = (&'a PeerHex, &'a str)>, retiring: &BTreeSet<String>) -> Option<&'a PeerHex> {
    hexes
        .into_iter()
        .filter(|(h, status)| {
            *status == "active" && h.role != ROLE_OVERSEER && h.role != ROLE_ORACLE && !retiring.contains(&h.id) && matches!(h.preference.as_str(), "auto" | "overseer")
        })
        .map(|(h, _)| h)
        .min_by(|a, b| {
            (b.preference == "overseer")
                .cmp(&(a.preference == "overseer"))
                .then(slicing::count(b.slices_held).cmp(&slicing::count(a.slices_held)))
                .then(a.id.cmp(&b.id))
        })
}

/// Why a retiring hex can't go yet, or None once it can. `hexes` is every
/// known hex with its status; `copies` the lattice's `slice_copies`.
pub fn blocker(hex: &PeerHex, hexes: &[(PeerHex, String)], retiring: &BTreeSet<String>, copies: usize) -> Option<String> {
    if hex.role == ROLE_OVERSEER {
        return Some("it's the Overseer; it hands the role over once a successor has every change".into());
    }
    // The replicas that stay, and what they hold.
    let staying: Vec<&PeerHex> = hexes
        .iter()
        .filter(|(h, status)| status == "active" && h.id != hex.id && h.role != ROLE_OVERSEER && h.role != ROLE_ORACLE && !retiring.contains(&h.id))
        .map(|(h, _)| h)
        .collect();
    if !hexes.iter().any(|(h, status)| status == "active" && h.role == ROLE_OVERSEER) {
        return Some("the lattice has no Overseer right now".into());
    }
    if staying.is_empty() || hex.role == ROLE_ORACLE {
        // The Overseer keeps everything, and nothing else is left to hold copies (or it held no data).
        return None;
    }
    let needed = copies.clamp(1, staying.len());
    let held: SliceMask = if hex.slicing { hex.slices_held } else { ALL_SLICES };
    let short: Vec<u32> = slicing::list(held)
        .into_iter()
        .filter(|slice| staying.iter().filter(|h| slicing::holds(if h.slicing { h.slices_held } else { ALL_SLICES }, *slice)).count() < needed)
        .collect();
    match short.len() {
        0 => None,
        n => Some(format!("{} of its slices aren't kept by enough other replicas yet", n)),
    }
}

impl HexDBEngine {
    /// Every hex for a hosting control service: where it runs, what it runs,
    /// how far along it is, and whether it's retiring and removable.
    pub async fn control_view(&self) -> serde_json::Value {
        let hexes = members(self).await;
        let retiring = self.retiring_hexes().await;
        let head = hexes.iter().find(|(h, s)| s == "active" && h.role == ROLE_OVERSEER).map(|(h, _)| h.applied_seq);
        let list: Vec<serde_json::Value> = hexes
            .iter()
            .map(|(h, status)| {
                // On the Overseer, a replica's position is exact (its polls acknowledge it).
                let applied = if self.is_writable() && h.id != self.id.to_string() { self.replica_applied(&h.id).unwrap_or(h.applied_seq) } else { h.applied_seq };
                let (is_retiring, removable) = status_of(&h.id, &retiring);
                json!({
                    "id": h.id, "name": h.name, "machine_id": h.machine_id, "region": h.region, "role": h.role, "status": status,
                    "version": h.version, "preference": h.preference, "applied_seq": applied,
                    "lag": head.map(|head| head.saturating_sub(applied)),
                    "slicing": h.slicing, "slices_held": slicing::count(if h.slicing { h.slices_held } else { ALL_SLICES }),
                    "retiring": is_retiring, "removable": removable,
                })
            })
            .collect();
        // The answering hex's own disk use (the Overseer's is the lattice's largest: it keeps everything).
        let answered_by = json!({ "id": self.id.to_string(), "machine_id": crate::updates::machine_id(), "disk_used_bytes": self.disk_used_bytes() });
        json!({ "lattice": self.config.network.lattice_name, "hexes": list, "answered_by": answered_by })
    }

    /// Every retiring hex, and whether it can be removed.
    pub async fn retiring_hexes(&self) -> Vec<RetiringHex> {
        let retiring = RETIRING.read().unwrap().clone();
        let hexes = members(self).await;
        let copies = hexes.iter().find(|(h, _)| h.role == ROLE_OVERSEER).map_or(1, |(h, _)| h.slice_copies.max(1));
        retiring
            .iter()
            .map(|id| match hexes.iter().find(|(h, _)| &h.id == id) {
                Some((hex, status)) => {
                    let waiting_for = blocker(hex, &hexes, &retiring, copies);
                    RetiringHex { id: id.clone(), name: hex.name.clone(), role: hex.role.clone(), status: status.clone(), removable: waiting_for.is_none(), waiting_for }
                }
                // Gone from discovery altogether: nothing of it is counted on.
                None => RetiringHex { id: id.clone(), name: String::new(), role: String::new(), status: "unknown".into(), removable: true, waiting_for: None },
            })
            .collect()
    }

    /// Mark a hex (by ID or name) as retiring. On the Overseer.
    pub async fn retire_hex(&self, which: &str, by: &str) -> Result<RetiringHex> {
        self.ensure_writable()?;
        let hexes = members(self).await;
        let (hex, _) = hexes
            .iter()
            .find(|(h, _)| h.id == which || h.name == which || (!h.machine_id.is_empty() && h.machine_id == which))
            .ok_or_else(|| crate::engine::EngineError::NotFound(format!("No hex '{}' in this lattice.", which)))?;
        let id = Ulid::from_string(&hex.id).map_err(|_| anyhow!("hex '{}' has no valid ID", hex.name))?;
        let mut retiring = RETIRING.read().unwrap().clone();
        retiring.insert(hex.id.clone());
        // Someone must be left to lead.
        let leaders = hexes
            .iter()
            .filter(|(h, status)| status == "active" && h.role != ROLE_ORACLE && !retiring.contains(&h.id) && matches!(h.preference.as_str(), "auto" | "overseer"))
            .count();
        if leaders == 0 && hex.role != ROLE_ORACLE {
            bail!(crate::engine::EngineError::Invalid("Retiring it would leave no active hex that can be the Overseer: add a hex first.".into()));
        }
        let doc = json!({ "id": hex.id, "name": hex.name, "retired_by": by, "retired_at": chrono::Utc::now().timestamp_millis() });
        self.put_system_document(RETIRING_TESSELLATION, id, doc, None).await?;
        RETIRING.write().unwrap().insert(hex.id.clone());
        info!("🌅 '{}' is retiring: its slices move to the other hexes before it can be removed.", hex.name);
        self.audit(by, "lattice.retire", &hex.name, json!({ "id": hex.id })).await;
        crate::network::discovery::elect_everywhere(self).await;
        let view = self.retiring_hexes().await;
        view.into_iter().find(|r| r.id == hex.id).ok_or_else(|| anyhow!("the retirement wasn't recorded"))
    }

    /// Cancel a retirement (by ID or name). On the Overseer.
    pub async fn unretire_hex(&self, which: &str, by: &str) -> Result<()> {
        self.ensure_writable()?;
        let hexes = members(self).await;
        let id = hexes.iter().find(|(h, _)| h.name == which || (!h.machine_id.is_empty() && h.machine_id == which)).map(|(h, _)| h.id.clone()).unwrap_or_else(|| which.to_string());
        if !RETIRING.read().unwrap().contains(&id) {
            bail!(crate::engine::EngineError::NotFound(format!("Hex '{}' isn't retiring.", which)));
        }
        let ulid = Ulid::from_string(&id).map_err(|_| anyhow!("'{}' isn't a hex ID", id))?;
        self.delete_system_document(RETIRING_TESSELLATION, ulid).await?;
        RETIRING.write().unwrap().remove(&id);
        self.audit(by, "lattice.unretire", which, json!({ "id": id })).await;
        crate::network::discovery::elect_everywhere(self).await;
        Ok(())
    }
}

/// On a retiring Overseer: hand the role to the successor once it has every
/// change (and, with slicing, every slice). Called after each discovery round.
pub async fn hand_over_if_retiring(engine: &HexDBEngine) {
    let me = engine.id.to_string();
    if !engine.is_writable() || !is_retiring(&me) || crate::updates::stepping_down() {
        return;
    }
    let retiring = RETIRING.read().unwrap().clone();
    let head = engine.changes.published_seq();
    let target = {
        let peers = engine.peers.lock().await;
        successor(peers.iter().map(|p| (&p.hex, p.status.as_str())), &retiring)
            .filter(|h| !h.slicing || h.slices_held == ALL_SLICES)
            .filter(|h| engine.replica_applied(&h.id).is_some_and(|applied| applied >= head))
            .map(|h| h.name.clone())
    };
    let Some(target) = target else { return };
    info!("🌅 This Overseer is retiring and '{}' has every change; handing the role over...", target);
    if let Err(e) = crate::updates::hand_over(engine, &target, "retiring").await {
        warn!("⚠️ Couldn't hand the Overseer role to '{}' ({:#}); trying again next round.", target, e);
    }
}

/// True if this replica (`me`) is the successor of its retiring, sliced
/// Overseer: it then keeps every slice, to take over without collecting any.
pub fn is_successor(me: &PeerHex, overseer: &PeerHex, others: &[(PeerHex, String)]) -> bool {
    let retiring = RETIRING.read().unwrap().clone();
    if !overseer.slicing || !retiring.contains(&overseer.id) {
        return false;
    }
    let hexes = others.iter().filter(|(h, _)| h.id != me.id).map(|(h, s)| (h, s.as_str())).chain(std::iter::once((me, "active")));
    successor(hexes, &retiring).is_some_and(|s| s.id == me.id)
}

/// The status fields for one hex.
pub fn status_of(id: &str, view: &[RetiringHex]) -> (bool, Option<bool>) {
    match view.iter().find(|r| r.id == id) {
        Some(r) => (true, Some(r.removable)),
        None => (false, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(id: &str, role: &str, preference: &str, slices: SliceMask) -> PeerHex {
        let mut h: PeerHex = serde_json::from_value(json!({ "id": id, "name": format!("n{}", id), "role": role, "lattice": "l", "ip": "x" })).unwrap();
        h.preference = preference.into();
        h.slicing = true;
        h.slices_held = slices;
        h.slice_copies = 1;
        h
    }

    #[test]
    fn the_successor_prefers_to_lead_then_holds_the_most() {
        let a = hex("A", "replicant", "auto", 0b1111);
        let b = hex("B", "replicant", "auto", 0b11);
        let c = hex("C", "replicant", "overseer", 0b1);
        let o = hex("O", ROLE_OVERSEER, "auto", ALL_SLICES);
        let none = BTreeSet::new();
        let list = [(&a, "active"), (&b, "active"), (&c, "active"), (&o, "active")];
        assert_eq!(successor(list, &none).map(|h| h.id.as_str()), Some("C"));
        let retiring: BTreeSet<String> = ["C".to_string()].into();
        assert_eq!(successor(list, &retiring).map(|h| h.id.as_str()), Some("A"));
        assert_eq!(successor([(&a, "lost"), (&b, "active")], &none).map(|h| h.id.as_str()), Some("B"));
    }

    #[test]
    fn a_hex_is_removable_once_others_hold_its_slices() {
        let o = hex("O", ROLE_OVERSEER, "auto", ALL_SLICES);
        let r = hex("R", "replicant", "auto", 0b1100);
        let a = hex("A", "replicant", "auto", 0b0011);
        let b = hex("B", "replicant", "auto", 0b0100);
        let retiring: BTreeSet<String> = ["R".to_string()].into();
        let all = |hexes: Vec<PeerHex>| hexes.into_iter().map(|h| (h, "active".to_string())).collect::<Vec<_>>();
        let hexes = all(vec![o.clone(), r.clone(), a.clone(), b.clone()]);
        assert!(blocker(&r, &hexes, &retiring, 1).unwrap().contains("1 of its slices"), "slice 3 is only on R");
        let a2 = hex("A", "replicant", "auto", 0b1011);
        assert_eq!(blocker(&r, &all(vec![o.clone(), r.clone(), a2, b.clone()]), &retiring, 1), None);
        // The Overseer must hand over first; with no replica staying, the Overseer alone keeps everything.
        assert!(blocker(&o, &hexes, &["O".to_string()].into(), 1).unwrap().contains("Overseer"));
        assert_eq!(blocker(&r, &all(vec![o.clone(), r.clone()]), &retiring, 1), None);
        // A retiring holder doesn't count.
        let both: BTreeSet<String> = ["R".to_string(), "A".to_string()].into();
        let a3 = hex("A", "replicant", "auto", 0b1100);
        assert!(blocker(&r, &all(vec![o, r.clone(), a3, b]), &both, 1).is_some());
    }
}
