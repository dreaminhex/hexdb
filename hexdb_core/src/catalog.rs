// HexDB Core Catalog
// The catalog records which tessellations exist (including empty ones) and the
// sequence number at which each dropped tessellation was deleted, so WAL
// records written before the drop are ignored during recovery. It is stored as
// `catalog.json` in the storage directory and replaced atomically on change.

use crate::wal::sync_dir;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const CATALOG_FILE: &str = "catalog.json";

/// Names that can't be used for tessellations: API routes and storage folders.
const RESERVED_NAMES: &[&str] = &["health", "status", "flush", "shutdown", "tessellation", "tessellations", "ui", "wal", "graphql"];
const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TessellationInfo {
    /// "user" or "system".
    pub kind: String,
    /// Creation time in epoch milliseconds.
    pub created: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Catalog {
    #[serde(default)]
    pub tessellations: BTreeMap<String, TessellationInfo>,
    /// Dropped tessellation name -> sequence number of the drop.
    #[serde(default)]
    pub dropped: BTreeMap<String, u64>,
}

impl Catalog {
    pub fn path(storage_dir: &Path) -> PathBuf {
        storage_dir.join(CATALOG_FILE)
    }

    /// Load the catalog, or `None` if it doesn't exist yet.
    pub fn load(storage_dir: &Path) -> Result<Option<Catalog>> {
        let path = Self::path(storage_dir);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("{} is corrupt", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    /// Write the catalog atomically (temp file, fsync, rename).
    pub fn save(&self, storage_dir: &Path) -> Result<()> {
        let path = Self::path(storage_dir);
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp).with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path).with_context(|| format!("Failed to replace {}", path.display()))?;
        sync_dir(storage_dir);
        Ok(())
    }

    /// Find an existing tessellation whose name matches ignoring case.
    /// Tessellation names map to folders, which are case-insensitive on Windows and macOS.
    pub fn find_case_insensitive(&self, name: &str) -> Option<&str> {
        self.tessellations
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .map(String::as_str)
    }

    /// Highest drop sequence number recorded.
    pub fn max_dropped_seq(&self) -> u64 {
        self.dropped.values().copied().max().unwrap_or(0)
    }

    /// Default kind for a tessellation name.
    pub fn default_kind(name: &str) -> &'static str {
        if name == "users" || name == "roles" {
            "system"
        } else {
            "user"
        }
    }
}

/// Check that a tessellation name is safe to use as a folder name and doesn't
/// collide with an API route: 1-64 ASCII letters, digits, `_` or `-`.
pub fn validate_tessellation_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        bail!("Tessellation names must be 1-{} characters long.", MAX_NAME_LEN);
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("Tessellation names may only contain letters, digits, '_' and '-'.");
    }
    if name.starts_with('_') {
        bail!("Tessellation names starting with '_' are reserved for system use.");
    }
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(name)) {
        bail!("'{}' is reserved and can't be used as a tessellation name.", name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert!(validate_tessellation_name("articles").is_ok());
        assert!(validate_tessellation_name("blog_posts-2024").is_ok());
        for bad in ["", "../etc", "a/b", "a.b", "has space", "WAL", "health", "_system", &"x".repeat(65)] {
            assert!(validate_tessellation_name(bad).is_err(), "{:?} should be rejected", bad);
        }
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("hexdb-catalog-test-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        assert!(Catalog::load(&dir).unwrap().is_none());

        let mut catalog = Catalog::default();
        catalog.tessellations.insert("Articles".into(), TessellationInfo { kind: "user".into(), created: 1 });
        catalog.dropped.insert("old".into(), 42);
        catalog.save(&dir).unwrap();

        let loaded = Catalog::load(&dir).unwrap().unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.find_case_insensitive("articles"), Some("Articles"));
        assert_eq!(loaded.max_dropped_seq(), 42);
        fs::remove_dir_all(&dir).ok();
    }
}
