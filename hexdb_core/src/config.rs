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
}

#[derive(Debug, Deserialize, Clone)]
pub struct NetworkConfig {
    pub engine_endpoint: String,
    pub query_endpoint: String,
    pub discovery_endpoint: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    pub ram_mb: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub disk_mb: u32,
    pub encryption_key: String, // base64 encoded 256-bit AES key
}

#[derive(Debug, Deserialize, Clone)]
pub struct CompressionConfig {
    pub compression_level: u8,
}

impl Default for HexConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                engine_endpoint: "127.0.0.1:7700".into(),
                query_endpoint: "127.0.0.1:7701".into(),
                discovery_endpoint: "127.0.0.1:7702".into(),
            },
            memory: MemoryConfig {
                ram_mb: 512,
            },
            storage: StorageConfig {
                disk_mb: 8192,
                encryption_key: "base64:...".into(), // fallback
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
