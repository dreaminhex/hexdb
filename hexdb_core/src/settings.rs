// HexDB Core Runtime Settings
//
// Most configuration comes from hexdb.toml (and hexdb.local.toml and HEXDB_*
// variables). The settings listed in `SETTINGS` can also be changed from the
// admin UI or `PUT /settings`. Those changes are saved, encrypted, as
// `settings.hxe` in the data directory and applied on top of the config file
// at every start, so they belong to this hex (each hex has its own).
//
// Settings marked `live` take effect immediately; the rest apply at the next
// restart, and `GET /settings` says which saved changes are still waiting.

use crate::{config::HexConfig, crypt::KeyRing};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Saved setting overrides in the data directory.
pub const SETTINGS_FILE: &str = "settings.hxe";

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    Integer,
    Boolean,
}

/// A setting that can be changed at runtime.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SettingDef {
    pub key: &'static str,
    pub kind: SettingKind,
    pub min: i64,
    pub max: i64,
    /// Takes effect without a restart.
    pub live: bool,
    pub unit: &'static str,
    pub description: &'static str,
}

const fn int(key: &'static str, min: i64, max: i64, live: bool, unit: &'static str, description: &'static str) -> SettingDef {
    SettingDef { key, kind: SettingKind::Integer, min, max, live, unit, description }
}

const fn boolean(key: &'static str, live: bool, description: &'static str) -> SettingDef {
    SettingDef { key, kind: SettingKind::Boolean, min: 0, max: 1, live, unit: "", description }
}

pub const SETTINGS: &[SettingDef] = &[
    int("limits.max_document_kb", 1, 65_536, true, "KB", "Largest document HexDB accepts (as JSON). Larger writes get 413."),
    int("limits.max_request_mb", 1, 1_024, false, "MB", "Largest request body for bulk writes, queries, transactions and GraphQL."),
    int("storage.disk_mb", 64, 1 << 30, true, "MB", "Disk space the data directory may use. Beyond it, writes that add data get 507; deletes still work."),
    int("memory.ram_mb", 64, 1 << 24, false, "MB", "Memory for documents before they are flushed to disk (also ranks hexes in elections)."),
    int("limits.max_connections", 1, 1_000_000, false, "", "Connections the API accepts at once."),
    int("limits.max_connections_per_client", 1, 1_000_000, false, "", "Connections one client address may hold open."),
    int("limits.request_timeout_seconds", 1, 3_600, false, "s", "How long a request may take (long polls and streams are exempt)."),
    int("limits.header_timeout_seconds", 1, 600, false, "s", "Time to send request headers and finish the TLS handshake; idle connections close after it."),
    int("security.session_hours", 1, 720, true, "h", "How long a sign-in lasts."),
    int("security.max_failed_logins", 1, 1_000, false, "", "Failed sign-ins (per login and per client address) before sign-in is paused."),
    int("security.lockout_minutes", 1, 10_080, false, "min", "How long sign-in stays paused after too many failures."),
    int("security.audit_retention_days", 0, 36_500, true, "days", "How long audit events are kept (0 keeps them forever)."),
    int("storage.change_history_hours", 0, 87_600, true, "h", "Change history kept on disk so replicas, plugins and /changes clients can resume (0 keeps none)."),
    int("storage.change_history_mb", 1, 1 << 30, true, "MB", "Most disk space the change history may use."),
    int("storage.compaction_frequency", 1, 86_400 * 7, false, "s", "Seconds between SSTable compactions."),
    int("storage.wal_flush_check_frequency", 1, 86_400, false, "s", "Seconds between checks for documents to flush to disk."),
    boolean("storage.wal_sync", false, "fsync the write-ahead log before acknowledging writes (off is faster but can lose recent writes on power loss)."),
    int("compression.compression_level", -7, 22, false, "", "Zstandard level for the WAL and SSTables (0 is the library default)."),
    int("memory.ttl_scan_frequency", 1, 86_400, false, "s", "Seconds between sweeps for expired documents."),
    int("replication.min_acks", 0, 64, true, "", "Replicas that must have a write before the Overseer acknowledges it (0: asynchronous)."),
    int("replication.ack_timeout_ms", 100, 600_000, true, "ms", "How long a write waits for min_acks before answering 503 replication_timeout."),
    boolean("replication.forward_writes", false, "Replicas pass writes on to the Overseer instead of refusing them."),
    int("replication.quorum", 0, 64, false, "", "Hexes the Overseer must see to accept writes (0 turns the check off)."),
    boolean("plugins.enabled", false, "Load plugins at startup."),
];

pub fn definition(key: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// Settings read on every use, changeable while running.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSettings {
    pub max_document_kb: u64,
    pub disk_mb: u64,
    pub session_hours: u64,
    pub audit_retention_days: u64,
    pub change_history_hours: u64,
    pub change_history_mb: u64,
    pub min_acks: usize,
    pub ack_timeout_ms: u64,
}

impl LiveSettings {
    pub fn from_config(config: &HexConfig) -> Self {
        LiveSettings {
            max_document_kb: config.limits.max_document_kb,
            disk_mb: config.storage.disk_mb,
            session_hours: config.security.session_hours,
            audit_retention_days: config.security.audit_retention_days,
            change_history_hours: config.storage.change_history_hours,
            change_history_mb: config.storage.change_history_mb,
            min_acks: config.replication.min_acks,
            ack_timeout_ms: config.replication.ack_timeout_ms,
        }
    }
}

/// The value at a dotted path of the config, as JSON.
pub fn get(config: &HexConfig, key: &str) -> Option<Value> {
    let json = serde_json::to_value(config).ok()?;
    key.split('.').try_fold(&json, |v, part| v.get(part)).cloned()
}

/// Check a value against a setting's definition; returns it normalized.
pub fn validate(key: &str, value: &Value) -> Result<Value> {
    let def = definition(key).ok_or_else(|| anyhow!("'{}' can't be changed at runtime; edit hexdb.toml instead.", key))?;
    match def.kind {
        SettingKind::Boolean => value.as_bool().map(Value::Bool).ok_or_else(|| anyhow!("{} must be true or false.", key)),
        SettingKind::Integer => {
            let n = value.as_i64().ok_or_else(|| anyhow!("{} must be a whole number.", key))?;
            if n < def.min || n > def.max {
                bail!("{} must be between {} and {}.", key, def.min, def.max);
            }
            Ok(Value::from(n))
        }
    }
}

/// Apply overrides to a config (at startup).
pub fn apply(config: &mut HexConfig, overrides: &BTreeMap<String, Value>) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }
    let source = config.source.clone();
    let mut json = serde_json::to_value(&*config)?;
    for (key, value) in overrides {
        let value = validate(key, value)?;
        let mut parts: Vec<&str> = key.split('.').collect();
        let last = parts.pop().ok_or_else(|| anyhow!("empty setting key"))?;
        let mut node = &mut json;
        for part in parts {
            node = node.as_object_mut().and_then(|o| o.get_mut(part)).ok_or_else(|| anyhow!("unknown setting {}", key))?;
        }
        node.as_object_mut().ok_or_else(|| anyhow!("unknown setting {}", key))?.insert(last.to_string(), value);
    }
    *config = serde_json::from_value(json)?;
    config.source = source;
    Ok(())
}

/// Saved overrides (empty if none).
pub fn load_overrides(storage_dir: &Path, keys: &KeyRing) -> Result<BTreeMap<String, Value>> {
    match crate::crypt::read_sealed_file(&storage_dir.join(SETTINGS_FILE), keys)? {
        Some(bytes) => {
            let map: Map<String, Value> = serde_json::from_slice(&bytes)?;
            Ok(map.into_iter().collect())
        }
        None => Ok(BTreeMap::new()),
    }
}

pub fn save_overrides(storage_dir: &Path, keys: &KeyRing, overrides: &BTreeMap<String, Value>) -> Result<()> {
    std::fs::create_dir_all(storage_dir)?;
    crate::crypt::write_sealed_file(&storage_dir.join(SETTINGS_FILE), keys, &serde_json::to_vec(overrides)?)
}

/// The configuration as JSON with secrets replaced, for display.
pub fn redacted(config: &HexConfig) -> Value {
    let mut json = serde_json::to_value(config).unwrap_or_default();
    for (section, field) in [
        ("storage", "encryption_key"),
        ("storage", "previous_encryption_keys"),
        ("network", "lattice_secret"),
        ("network", "previous_lattice_secrets"),
        ("security", "admin_password"),
    ] {
        if let Some(value) = json.get_mut(section).and_then(|s| s.get_mut(field)) {
            *value = match value {
                Value::String(s) if s.is_empty() => Value::String(String::new()),
                Value::Array(items) => Value::Array(items.iter().map(|_| Value::String("(set)".into())).collect()),
                _ => Value::String("(set)".into()),
            };
        }
    }
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_apply_validate_and_round_trip() {
        let mut config = HexConfig::default();
        let mut overrides = BTreeMap::new();
        overrides.insert("limits.max_document_kb".to_string(), Value::from(2048));
        overrides.insert("storage.wal_sync".to_string(), Value::Bool(false));
        apply(&mut config, &overrides).unwrap();
        assert_eq!(config.limits.max_document_kb, 2048);
        assert!(!config.storage.wal_sync);
        assert_eq!(get(&config, "limits.max_document_kb"), Some(Value::from(2048)));

        assert!(validate("limits.max_document_kb", &Value::from(0)).is_err(), "below the minimum");
        assert!(validate("storage.wal_sync", &Value::from(1)).is_err(), "not a boolean");
        assert!(validate("storage.encryption_key", &Value::from("x")).is_err(), "not a runtime setting");
        assert!(SETTINGS.iter().all(|s| get(&HexConfig::default(), s.key).is_some()), "every setting exists in the config");

        let dir = std::env::temp_dir().join(format!("hexdb-settings-{}", ulid::Ulid::new()));
        let keys = KeyRing::new(&[3u8; 32], &[]);
        save_overrides(&dir, &keys, &overrides).unwrap();
        assert_eq!(load_overrides(&dir, &keys).unwrap(), overrides);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn secrets_are_redacted() {
        let mut config = HexConfig::default();
        config.storage.encryption_key = "base64:secret".into();
        config.network.previous_lattice_secrets = vec!["base64:old".into()];
        let json = redacted(&config);
        assert_eq!(json["storage"]["encryption_key"], "(set)");
        assert_eq!(json["network"]["previous_lattice_secrets"][0], "(set)");
        assert_eq!(json["network"]["lattice_secret"], "");
        assert!(!json.to_string().contains("base64:"));
    }
}
