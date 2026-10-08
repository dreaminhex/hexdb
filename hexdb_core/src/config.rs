// HexDB Core Configuration
// This module provides a configuration structure for the HexDB engine.
// allowing for easy customization of various parameters such as endpoints,
// memory allocation, and disk space.
//
// Configuration is layered: built-in defaults, then the config file, then
// environment variables (HEXDB_<SECTION>__<FIELD>, e.g. HEXDB_NETWORK__API_ENDPOINT).
// Relative paths inside the config file are resolved against the directory
// that contains the config file, so the server can be started from anywhere.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default config file name searched for in the working directory and next to the executable.
pub const CONFIG_FILE_NAME: &str = "hexdb.toml";

/// Environment variable that points at an explicit config file.
pub const CONFIG_ENV_VAR: &str = "HEXDB_CONFIG";

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HexConfig {
    pub network: NetworkConfig,
    pub memory: MemoryConfig,
    pub storage: StorageConfig,
    pub compression: CompressionConfig,
    pub security: SecurityConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub tls: TlsConfig,

    /// The config file this configuration was loaded from, if any.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NetworkConfig {
    pub api_endpoint: String,
    /// No longer used: GraphQL is served at /graphql on `api_endpoint`. Accepted so
    /// older config files still load; a warning is logged if it is set.
    #[serde(default, skip_serializing)]
    pub query_endpoint: Option<String>,
    pub discovery_endpoint: String,
    pub lattice_name: String,
    /// Discovery endpoints (`host:port`) of other hexes to probe, e.g. on other machines.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Also probe the local discovery ports 7702-7709 (several hexes on one machine).
    #[serde(default = "default_true")]
    pub scan_local_ports: bool,
    /// Seconds between discovery rounds.
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_seconds: u64,
    /// Shared secret (`base64:<32 bytes>`) that authenticates hexes to each
    /// other (discovery and replication). Every hex in a lattice needs the same
    /// value. When empty, it is derived from `storage.encryption_key`.
    #[serde(default)]
    pub lattice_secret: String,
    /// Host other hexes should use to reach this one, when the endpoints bind 0.0.0.0.
    #[serde(default)]
    pub advertise_host: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MemoryConfig {
    pub ram_mb: u64,
    pub ttl_scan_frequency: u64, // seconds
    pub vertex_integrity_check_frequency: u64, // seconds
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StorageConfig {
    /// Data directory. Relative paths are resolved against the config file's directory.
    pub path: String,
    pub disk_mb: u64,
    /// AES-256 key for data at rest (WAL and SSTables), formatted as
    /// `base64:<32 bytes base64-encoded>`. Keep it out of version control: put
    /// it in `hexdb.local.toml` or the `HEXDB_STORAGE__ENCRYPTION_KEY` variable.
    pub encryption_key: String,
    /// Keys used before the current one. Data written with them stays readable;
    /// compaction re-encrypts it with the current key, after which they can be removed.
    #[serde(default)]
    pub previous_encryption_keys: Vec<String>,
    pub compaction_frequency: u64, // seconds
    pub wal_flush_check_frequency: u64, // seconds
    /// fsync the WAL before acknowledging writes. Disabling it is faster, but a
    /// power loss or OS crash can lose recently acknowledged writes.
    #[serde(default = "default_true")]
    pub wal_sync: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SecurityConfig {
    /// The first administrator, created when there are no users.
    pub admin_login: String,
    /// Password for the first administrator. Leave empty to have HexDB
    /// generate one and print it once at first start. Ignored once users exist.
    #[serde(default)]
    pub admin_password: String,
    pub admin_email: String,
    /// How long a sign-in session lasts.
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
    /// Failed sign-ins (per login, and per client address) before sign-in is
    /// refused for `lockout_minutes`.
    #[serde(default = "default_max_failed_logins")]
    pub max_failed_logins: u32,
    #[serde(default = "default_lockout_minutes")]
    pub lockout_minutes: u64,
}

fn default_session_hours() -> u64 {
    12
}

fn default_max_failed_logins() -> u32 {
    5
}

fn default_lockout_minutes() -> u64 {
    15
}

/// HTTPS for the API (and for replication between hexes).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct TlsConfig {
    /// PEM certificate chain. HTTPS is on when this and `key_file` are set.
    #[serde(default)]
    pub cert_file: String,
    /// PEM private key.
    #[serde(default)]
    pub key_file: String,
    /// Extra PEM CA certificate(s) to trust when connecting to other hexes
    /// (for private or self-signed certificates).
    #[serde(default)]
    pub ca_file: String,
}

impl TlsConfig {
    pub fn enabled(&self) -> bool {
        !self.cert_file.is_empty() && !self.key_file.is_empty()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CompressionConfig {
    pub compression_level: i32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UiConfig {
    /// Built admin UI (Vite `dist`) directory. Relative paths are resolved against the config file's directory.
    pub path: String,
}

fn default_discovery_interval() -> u64 {
    10
}

/// Which lattice role this hex may take.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IdentityConfig {
    /// `auto` (elected), `overseer` (prefer leading), `harvester` or `replicant` (never lead).
    #[serde(default = "default_role")]
    pub role: String,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        IdentityConfig { role: default_role() }
    }
}

fn default_role() -> String {
    "auto".into()
}

/// Plugins that consume the change feed (see `hexdb_core::plugins`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PluginsConfig {
    /// Load plugins at startup.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The plugin registry: absolute, or relative to the config file's folder
    /// (the working directory when there's no config file).
    #[serde(default = "default_registry")]
    pub registry: String,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        PluginsConfig { enabled: true, registry: default_registry() }
    }
}

fn default_registry() -> String {
    "plugins.json".into()
}

fn default_true() -> bool {
    true
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            path: "../hexdb_admin/dist".into(),
        }
    }
}

impl Default for HexConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                api_endpoint: "127.0.0.1:7700".into(),
                query_endpoint: None,
                discovery_endpoint: "127.0.0.1:7702".into(),
                lattice_name: "Nebula Prime".into(),
                peers: Vec::new(),
                scan_local_ports: true,
                discovery_interval_seconds: 10,
                lattice_secret: String::new(),
                advertise_host: None,
            },
            security: SecurityConfig {
                admin_login: "hexdbadmin".into(),
                admin_password: String::new(),
                admin_email: "admin@hexdb".into(),
                session_hours: default_session_hours(),
                max_failed_logins: default_max_failed_logins(),
                lockout_minutes: default_lockout_minutes(),
            },
            memory: MemoryConfig {
                ram_mb: 1024,
                ttl_scan_frequency: 600, // 10 minutes
                vertex_integrity_check_frequency: 300, // 5 minutes
            },
            storage: StorageConfig {
                path: "./.hexdb".into(),
                disk_mb: 8192,
                encryption_key: String::new(), // must be supplied by config or environment
                previous_encryption_keys: Vec::new(),
                compaction_frequency: 1800, // 30 minutes
                wal_flush_check_frequency: 60, // 1 minute
                wal_sync: true,
            },
            compression: CompressionConfig {
                compression_level: 0,
            },
            ui: UiConfig::default(),
            identity: IdentityConfig::default(),
            plugins: PluginsConfig::default(),
            tls: TlsConfig::default(),
            source: None,
        }
    }
}

impl HexConfig {
    /// The resolved data directory.
    pub fn storage_dir(&self) -> PathBuf {
        PathBuf::from(&self.storage.path)
    }

    /// The resolved admin UI directory.
    pub fn ui_dir(&self) -> PathBuf {
        PathBuf::from(&self.ui.path)
    }

    /// The storage key ring: `storage.encryption_key` plus `previous_encryption_keys`.
    pub fn key_ring(&self) -> anyhow::Result<crate::crypt::KeyRing> {
        let current = crate::crypt::decode_encryption_key(&self.storage.encryption_key)?;
        let previous = self
            .storage
            .previous_encryption_keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                crate::crypt::decode_encryption_key(k)
                    .map_err(|e| anyhow::anyhow!("storage.previous_encryption_keys[{}]: {}", i, e))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(crate::crypt::KeyRing::new(&current, &previous))
    }

    /// The key hexes use to authenticate each other: `network.lattice_secret`,
    /// or a key derived from `storage.encryption_key` when that is empty.
    pub fn lattice_key(&self) -> anyhow::Result<[u8; 32]> {
        if !self.network.lattice_secret.trim().is_empty() {
            return crate::crypt::decode_encryption_key(&self.network.lattice_secret)
                .map_err(|e| anyhow::anyhow!("network.lattice_secret: {}", e.to_string().replace("storage.encryption_key", "network.lattice_secret")));
        }
        let storage = crate::crypt::decode_encryption_key(&self.storage.encryption_key)?;
        Ok(blake3::derive_key("HexDB 2026 lattice key v1", &storage))
    }

    /// The key that signs session tokens. Derived from the lattice key, so a
    /// session works on every hex of the lattice.
    pub fn session_key(&self) -> anyhow::Result<[u8; 32]> {
        Ok(blake3::derive_key("HexDB 2026 session signing key v1", &self.lattice_key()?))
    }
}

/// Load configuration using the default search order (see [`load_config_from`]).
pub fn load_config() -> Result<HexConfig, config::ConfigError> {
    load_config_from(None)
}

/// Load configuration.
///
/// The config file is chosen in this order:
/// 1. `explicit` (e.g. a `--config` argument)
/// 2. the `HEXDB_CONFIG` environment variable
/// 3. `./hexdb.toml` in the working directory
/// 4. `hexdb.toml` next to the running executable
///
/// If none is found, built-in defaults are used. An explicitly named file that
/// does not exist is an error. Environment variables override file values.
pub fn load_config_from(explicit: Option<&Path>) -> Result<HexConfig, config::ConfigError> {
    let file = find_config_file(explicit)?;

    let mut builder = config::Config::builder()
        .add_source(config::Config::try_from(&HexConfig::default())?);

    if let Some(path) = &file {
        builder = builder.add_source(config::File::from(path.as_path()).required(true));
        // Secrets live in an optional, git-ignored `hexdb.local.toml` beside the config file.
        if let Some(local) = local_config_path(path) {
            builder = builder.add_source(config::File::from(local.as_path()).required(true));
        }
    }

    let cfg = builder
        .add_source(
            config::Environment::with_prefix("HEXDB")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()?;

    let mut hex_config: HexConfig = cfg.try_deserialize()?;

    // Resolve relative paths against the config file's directory (or the working directory).
    let base = match &file {
        Some(path) => path.parent().map(Path::to_path_buf).unwrap_or_default(),
        None => std::env::current_dir().map_err(|e| config::ConfigError::Foreign(Box::new(e)))?,
    };
    hex_config.storage.path = resolve_path(&base, &hex_config.storage.path);
    hex_config.ui.path = resolve_path(&base, &hex_config.ui.path);
    for path in [&mut hex_config.tls.cert_file, &mut hex_config.tls.key_file, &mut hex_config.tls.ca_file] {
        if !path.is_empty() {
            *path = resolve_path(&base, path);
        }
    }
    hex_config.source = file;

    Ok(hex_config)
}

/// Name of the optional secrets file read after the main config file.
pub const LOCAL_CONFIG_FILE_NAME: &str = "hexdb.local.toml";

/// `hexdb.local.toml` next to a config file, if it exists.
pub fn local_config_path(config_file: &Path) -> Option<PathBuf> {
    let candidate = config_file.parent()?.join(LOCAL_CONFIG_FILE_NAME);
    candidate.is_file().then_some(candidate)
}

/// Locate the config file according to the search order documented on [`load_config_from`].
fn find_config_file(explicit: Option<&Path>) -> Result<Option<PathBuf>, config::ConfigError> {
    let named = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CONFIG_ENV_VAR).filter(|v| !v.is_empty()).map(PathBuf::from));

    if let Some(path) = named {
        if !path.is_file() {
            return Err(config::ConfigError::Message(format!(
                "Config file not found: {}",
                path.display()
            )));
        }
        return Ok(Some(absolute(&path)));
    }

    let cwd_candidate = PathBuf::from(CONFIG_FILE_NAME);
    if cwd_candidate.is_file() {
        return Ok(Some(absolute(&cwd_candidate)));
    }

    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let candidate = exe_dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_path(base: &Path, value: &str) -> String {
    let path = Path::new(value);
    let joined = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
    absolute(&joined).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn relative_paths_resolve_against_config_dir() {
        let dir = std::env::temp_dir().join(format!("hexdb-config-test-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hexdb.toml");
        fs::write(
            &file,
            "[storage]\npath = \"./data\"\nencryption_key = \"base64:abc\"\n[ui]\npath = \"../ui\"\n",
        )
        .unwrap();

        let cfg = load_config_from(Some(&file)).unwrap();

        assert_eq!(cfg.storage_dir(), absolute(&dir.join("./data")));
        assert_eq!(cfg.ui_dir(), absolute(&dir.join("../ui")));
        // Values missing from the file fall back to defaults.
        assert_eq!(cfg.network.api_endpoint, "127.0.0.1:7700");
        assert_eq!(cfg.storage.encryption_key, "base64:abc");
        assert_eq!(cfg.source.as_deref(), Some(absolute(&file).as_path()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn local_file_overrides_secrets() {
        let dir = std::env::temp_dir().join(format!("hexdb-config-test-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hexdb.toml");
        fs::write(&file, "[storage]
path = \"./data\"
encryption_key = \"\"
[security]
admin_login = \"root\"
admin_email = \"r@x\"
").unwrap();
        fs::write(dir.join(LOCAL_CONFIG_FILE_NAME), "[storage]
encryption_key = \"base64:secret\"
").unwrap();

        let cfg = load_config_from(Some(&file)).unwrap();
        assert_eq!(cfg.storage.encryption_key, "base64:secret");
        assert_eq!(cfg.security.admin_login, "root", "the main file still applies");
        assert!(cfg.security.admin_password.is_empty(), "no built-in default password");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retired_query_endpoint_still_loads() {
        let dir = std::env::temp_dir().join(format!("hexdb-config-test-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hexdb.toml");
        fs::write(&file, "[network]\nquery_endpoint = \"127.0.0.1:7701\"\n").unwrap();

        let cfg = load_config_from(Some(&file)).unwrap();
        assert_eq!(cfg.network.query_endpoint.as_deref(), Some("127.0.0.1:7701"));
        assert_eq!(cfg.network.api_endpoint, "127.0.0.1:7700");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let missing = std::env::temp_dir().join("hexdb-definitely-missing.toml");
        assert!(load_config_from(Some(&missing)).is_err());
    }
}
