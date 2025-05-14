// HexDB Core Configuration
// This module provides a configuration structure for the HexDB engine.
// allowing for easy customization of various parameters such as endpoints,
// memory allocation, and disk space.

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct HexConfig {
    pub network: NetworkConfig,
    pub memory: MemoryConfig,
    pub storage: StorageConfig,
    pub compression: CompressionConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NetworkConfig {
    pub api_endpoint: String,
    pub query_endpoint: String,
    pub discovery_endpoint: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    pub ram_mb: u64,
    pub ttl_scan_frequency: u64, // seconds
    pub vertex_integrity_check_frequency: u64, // seconds
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub path: String,
    pub disk_mb: u64,
    pub encryption_key: String,
    pub compaction_frequency: u64, // seconds
    pub wal_flush_check_frequency: u64, // seconds
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecurityConfig {
    pub admin_login: String,
    pub admin_password: String,
    pub admin_email: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CompressionConfig {
    pub compression_level: i32,
}

impl Default for HexConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                api_endpoint: "127.0.0.1:7700".into(),
                query_endpoint: "127.0.0.1:7701".into(),
                discovery_endpoint: "127.0.0.1:7702".into(),
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
                encryption_key: "base64:...".into(), // fallback
                compaction_frequency: 1800, // 30 minutes
                wal_flush_check_frequency: 60, // 1 minute
            },
            compression: CompressionConfig {
                compression_level: 0,
            },
        }
    }
}


pub fn load_config() -> Result<HexConfig, config::ConfigError> {
    let cfg = config::Config::builder()
        .add_source(config::File::with_name("hexdb").required(false))
        .add_source(config::Environment::with_prefix("HEXDB").separator("_"))
        .build()?;

    cfg.try_deserialize::<HexConfig>()
}
