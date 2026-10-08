// HexDB Core Catalog
// The catalog records which tessellations exist (including empty ones) and the
// sequence number at which each dropped tessellation was deleted, so WAL
// records written before the drop are ignored during recovery. It is stored
// encrypted as `catalog.hxe` in the storage directory and replaced atomically
// on change.

use crate::crypt::{read_sealed_file, write_sealed_file, KeyRing};
use crate::wal::sync_dir;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// The catalog, encrypted with the storage key ring.
pub const CATALOG_FILE: &str = "catalog.hxe";
/// The plaintext catalog of earlier builds (migrated on first save).
const LEGACY_CATALOG_FILE: &str = "catalog.json";

/// Names that can't be used for tessellations: API routes and storage folders.
const RESERVED_NAMES: &[&str] = &[
    "health", "status", "flush", "shutdown", "tessellation", "tessellations", "ui", "wal", "graphql",
    "logs", "changes", "indexes", "auth", "transactions", "plugins", "lattice", "replication", "compact",
    "audit", "settings", "join", "streams", "functions", "schedules", "analyzers", "schemas", "openapi", "backup", "backups", "triggers",
    "sql",
];
const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TessellationInfo {
    /// "user" or "system".
    pub kind: String,
    /// Creation time in epoch milliseconds.
    pub created: i64,
    /// Secondary index definitions (contents are rebuilt in memory at startup).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<crate::index::IndexDef>,
    /// Schema versions, oldest first (none: schemaless).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schemas: Vec<crate::schema::SchemaVersion>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Catalog {
    #[serde(default)]
    pub tessellations: BTreeMap<String, TessellationInfo>,
    /// Dropped tessellation name -> sequence number of the drop.
    #[serde(default)]
    pub dropped: BTreeMap<String, u64>,
    /// Names this data directory's sequence of changes. It survives restarts,
    /// so replicas can resume from their cursor; a different hex (or a wiped
    /// data directory) has a different history and replicas resynchronize.
    #[serde(default)]
    pub history_id: String,
    /// The lattice this data belongs to, when the config doesn't name one.
    #[serde(default)]
    pub lattice_name: String,
    /// This hex's ID, kept across restarts.
    #[serde(default)]
    pub hex_id: String,
    /// This hex's name, kept across restarts when the config doesn't set one.
    #[serde(default)]
    pub hex_name: String,
}

impl Catalog {
    pub fn path(storage_dir: &Path) -> PathBuf {
        storage_dir.join(CATALOG_FILE)
    }

    /// Load the catalog, or `None` if it doesn't exist yet. A plaintext
    /// `catalog.json` from an earlier build is read, and replaced by the
    /// encrypted file on the next save.
    pub fn load(storage_dir: &Path, keys: &KeyRing) -> Result<Option<Catalog>> {
        let path = Self::path(storage_dir);
        if let Some(bytes) = read_sealed_file(&path, keys)? {
            return Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("{} is corrupt", path.display()))?));
        }
        let legacy = storage_dir.join(LEGACY_CATALOG_FILE);
        match fs::read_to_string(&legacy) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("{} is corrupt", legacy.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", legacy.display())),
        }
    }

    /// True if the catalog needs rewriting: still plaintext, or encrypted with a previous key.
    pub fn needs_rewrite(storage_dir: &Path, keys: &KeyRing) -> bool {
        storage_dir.join(LEGACY_CATALOG_FILE).exists()
            || crate::crypt::sealed_file_key_id(&Self::path(storage_dir)).is_some_and(|id| id != keys.current_id())
    }

    /// Write the catalog encrypted and atomically (temp file, fsync, rename).
    pub fn save(&self, storage_dir: &Path, keys: &KeyRing) -> Result<()> {
        write_sealed_file(&Self::path(storage_dir), keys, &serde_json::to_vec(self)?)?;
        let legacy = storage_dir.join(LEGACY_CATALOG_FILE);
        if legacy.exists() {
            fs::remove_file(&legacy).with_context(|| format!("Failed to remove {}", legacy.display()))?;
        }
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
        let keys = KeyRing::new(&[1u8; 32], &[]);
        assert!(Catalog::load(&dir, &keys).unwrap().is_none());

        let mut catalog = Catalog::default();
        catalog.tessellations.insert("Articles".into(), TessellationInfo { kind: "user".into(), created: 1, indexes: Vec::new(), schemas: Vec::new() });
        catalog.dropped.insert("old".into(), 42);
        catalog.save(&dir, &keys).unwrap();
        assert!(!fs::read(Catalog::path(&dir)).unwrap().windows(8).any(|w| w == b"Articles"), "encrypted");

        let loaded = Catalog::load(&dir, &keys).unwrap().unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.find_case_insensitive("articles"), Some("Articles"));
        assert_eq!(loaded.max_dropped_seq(), 42);

        // A plaintext catalog from an earlier build is read, then replaced.
        fs::remove_file(Catalog::path(&dir)).unwrap();
        fs::write(dir.join(LEGACY_CATALOG_FILE), serde_json::to_string(&catalog).unwrap()).unwrap();
        assert!(Catalog::needs_rewrite(&dir, &keys));
        let migrated = Catalog::load(&dir, &keys).unwrap().unwrap();
        assert_eq!(migrated, catalog);
        migrated.save(&dir, &keys).unwrap();
        assert!(!dir.join(LEGACY_CATALOG_FILE).exists());
        assert!(!Catalog::needs_rewrite(&dir, &keys));
        assert!(Catalog::needs_rewrite(&dir, &KeyRing::new(&[2u8; 32], &[[1u8; 32]])), "a rotated key means a rewrite");
        fs::remove_dir_all(&dir).ok();
    }
}
