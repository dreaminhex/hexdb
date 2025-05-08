use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct HexConfig {
    /// Listening address for document engine
    pub engine_endpoint: String,

    /// Listening address for query interface (GraphQL, etc.)
    pub query_endpoint: String,

    /// Listening address for cluster discovery (TCP or WS)
    pub discovery_endpoint: String,

    /// How much RAM to allocate (in MiB) for hot document storage
    pub ram_mb: u32,

    /// How much disk to allocate (in MiB) for persistent storage
    pub disk_mb: u32,
}

impl Default for HexConfig {
    fn default() -> Self {
        Self {
            engine_endpoint: "127.0.0.1:7700".into(),
            query_endpoint: "127.0.0.1:7701".into(),
            discovery_endpoint: "127.0.0.1:7702".into(),
            ram_mb: 512,
            disk_mb: 8192,
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
