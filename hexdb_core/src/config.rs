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

    /// The config file this configuration was loaded from, if any.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NetworkConfig {
    pub api_endpoint: String,
    pub query_endpoint: String,
    pub discovery_endpoint: String,
    pub lattice_name: String,
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
    /// AES-256 key for the WAL, formatted as `base64:<32 bytes base64-encoded>`.
    pub encryption_key: String,
    pub compaction_frequency: u64, // seconds
    pub wal_flush_check_frequency: u64, // seconds
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SecurityConfig {
    pub admin_login: String,
    pub admin_password: String,
    pub admin_email: String,
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
                query_endpoint: "127.0.0.1:7701".into(),
                discovery_endpoint: "127.0.0.1:7702".into(),
                lattice_name: "Nebula Prime".into(),
            },
            security: SecurityConfig {
                admin_login: "hexdbadmin".into(),
                admin_password: "hexdbadmin1234".into(),
                admin_email: "admin@hexdb".into(),
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
                compaction_frequency: 1800, // 30 minutes
                wal_flush_check_frequency: 60, // 1 minute
            },
            compression: CompressionConfig {
                compression_level: 0,
            },
            ui: UiConfig::default(),
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
    hex_config.source = file;

    Ok(hex_config)
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
    fn missing_explicit_file_is_an_error() {
        let missing = std::env::temp_dir().join("hexdb-definitely-missing.toml");
        assert!(load_config_from(Some(&missing)).is_err());
    }
}
