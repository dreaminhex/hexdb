// HexDB Core IP Blocklist
//
// Addresses and CIDR ranges whose requests every hex refuses before
// authentication (see hexdb_api's blocklist middleware). Entries are
// documents in the `_blocklist` system tessellation, so they replicate to
// every hex like any write, and an entry with an expiry is a document with a
// TTL: it stops applying when it expires, with nothing to clean up.

use crate::engine::{EngineError, HexDBEngine};
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use ulid::Ulid;

/// The blocklist, one document per entry.
pub const BLOCKLIST_TESSELLATION: &str = "_blocklist";
/// Most entries the blocklist holds.
pub const MAX_ENTRIES: usize = 10_000;

/// One address range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// An address (`203.0.113.7`) or a range (`203.0.113.0/24`, `2001:db8::/32`).
    pub fn parse(text: &str) -> Option<Cidr> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (text.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Cidr { network: addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 address may arrive mapped into IPv6.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// A blocklist entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEntry {
    #[serde(default)]
    pub id: String,
    /// The address or range, as given.
    pub address: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub created_by: String,
    pub created_at: i64,
    /// Epoch milliseconds when it stops applying (none: until removed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// A new entry, as submitted.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBlock {
    pub address: String,
    #[serde(default)]
    pub note: String,
    /// Stop applying after this many hours (none: until removed).
    #[serde(default)]
    pub expires_in_hours: Option<f64>,
}

impl HexDBEngine {
    /// Every entry that still applies, newest first.
    pub async fn blocklist(&self) -> Result<Vec<BlockEntry>> {
        if !self.tessellation_exists(BLOCKLIST_TESSELLATION) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self.list_documents(BLOCKLIST_TESSELLATION, after, 1000).await?;
            for doc in &page.documents {
                if let Ok(mut entry) = serde_json::from_value::<BlockEntry>(doc.data_json()) {
                    entry.id = doc.id.to_string();
                    out.push(entry);
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        Ok(out)
    }

    /// Add an entry. `own` is the caller's address, which an entry may not cover.
    pub async fn add_block(&self, by: &str, input: NewBlock, own: Option<IpAddr>) -> Result<BlockEntry> {
        self.ensure_writable()?;
        let cidr = Cidr::parse(&input.address)
            .ok_or_else(|| EngineError::Invalid(format!("'{}' isn't an IP address or a CIDR range (like 203.0.113.0/24).", input.address)))?;
        if own.is_some_and(|ip| cidr.contains(ip)) {
            return Err(EngineError::Invalid(format!("{} covers your own address, so it would lock you out; block a narrower range.", input.address.trim())).into());
        }
        if self.blocklist().await?.len() >= MAX_ENTRIES {
            return Err(EngineError::Invalid(format!("The blocklist holds at most {} entries; remove some, or block a wider range.", MAX_ENTRIES)).into());
        }
        let now = Utc::now().timestamp_millis();
        let expires_at = match input.expires_in_hours {
            Some(h) if !(h > 0.0 && h.is_finite()) => return Err(EngineError::Invalid("expires_in_hours must be a positive number.".into()).into()),
            Some(h) => Some(now + (h * 3_600_000.0) as i64),
            None => None,
        };
        let id = Ulid::generate();
        let entry = BlockEntry { id: id.to_string(), address: input.address.trim().to_string(), note: input.note.trim().to_string(), created_by: by.to_string(), created_at: now, expires_at };
        self.ensure_system_tessellation(BLOCKLIST_TESSELLATION)?;
        self.put_system_document(BLOCKLIST_TESSELLATION, id, serde_json::to_value(&entry)?, expires_at).await?;
        Ok(entry)
    }

    /// Remove an entry. False if there's none with that ID.
    pub async fn remove_block(&self, id: &str) -> Result<Option<BlockEntry>> {
        self.ensure_writable()?;
        let Ok(ulid) = Ulid::from_string(id) else { return Ok(None) };
        if !self.tessellation_exists(BLOCKLIST_TESSELLATION) {
            return Ok(None);
        }
        let Some(doc) = self.get_system_document(BLOCKLIST_TESSELLATION, ulid).await? else { return Ok(None) };
        let mut entry: BlockEntry = serde_json::from_value(doc.data_json())?;
        entry.id = id.to_string();
        self.delete_system_document(BLOCKLIST_TESSELLATION, ulid).await?;
        Ok(Some(entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let net = Cidr::parse("203.0.113.0/24").unwrap();
        assert!(net.contains("203.0.113.9".parse().unwrap()));
        assert!(!net.contains("203.0.114.9".parse().unwrap()));
        assert!(net.contains("::ffff:203.0.113.9".parse().unwrap()), "IPv4 mapped into IPv6");
        assert!(Cidr::parse("2001:db8::/32").unwrap().contains("2001:db8::1".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_none() && Cidr::parse("nope").is_none());
        assert!(Cidr::parse("198.51.100.4").unwrap().contains("198.51.100.4".parse().unwrap()));
    }

    #[tokio::test]
    async fn entries_expire_and_never_cover_the_caller() {
        let (engine, dir) = crate::engine::test_engine().await;
        let own: IpAddr = "198.51.100.4".parse().unwrap();
        let refused = engine.add_block("ada", NewBlock { address: "198.51.100.0/24".into(), note: String::new(), expires_in_hours: None }, Some(own)).await;
        assert!(refused.unwrap_err().to_string().contains("your own address"));
        let kept = engine.add_block("ada", NewBlock { address: "203.0.113.0/24".into(), note: "scanner".into(), expires_in_hours: None }, Some(own)).await.unwrap();
        engine.add_block("ada", NewBlock { address: "192.0.2.1".into(), note: String::new(), expires_in_hours: Some(0.0001) }, Some(own)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let list = engine.blocklist().await.unwrap();
        assert_eq!(list.iter().map(|e| e.address.as_str()).collect::<Vec<_>>(), vec!["203.0.113.0/24"], "the expired entry is gone");
        assert_eq!(engine.remove_block(&kept.id).await.unwrap().unwrap().note, "scanner");
        assert!(engine.blocklist().await.unwrap().is_empty());
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
