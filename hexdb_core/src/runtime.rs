// HexDB Runtime Information
// A running server writes a small JSON file into its data directory with its
// process ID, API endpoint, and a random one-time shutdown token. The CLI reads
// it to find the server and to ask it to shut down gracefully.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// File name of the runtime file inside the storage directory.
pub const PID_FILE_NAME: &str = "hexdb.pid";

/// HTTP header that carries the shutdown token.
pub const SHUTDOWN_TOKEN_HEADER: &str = "x-hexdb-shutdown-token";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub api_endpoint: String,
    pub shutdown_token: String,
}

impl RuntimeInfo {
    /// Create runtime info for the current process with a fresh random shutdown token.
    pub fn for_current_process(api_endpoint: &str) -> Result<Self> {
        let mut token = [0u8; 32];
        getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("Failed to generate shutdown token: {}", e))?;

        Ok(Self {
            pid: std::process::id(),
            api_endpoint: api_endpoint.to_string(),
            shutdown_token: hex::encode(token),
        })
    }

    /// Path of the runtime file for a storage directory.
    pub fn path(storage_dir: &Path) -> PathBuf {
        storage_dir.join(PID_FILE_NAME)
    }

    /// Write the runtime file. On Unix it is readable by the owner only.
    pub fn write(&self, storage_dir: &Path) -> Result<PathBuf> {
        fs::create_dir_all(storage_dir)
            .with_context(|| format!("Failed to create {}", storage_dir.display()))?;

        let path = Self::path(storage_dir);
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options
            .open(&path)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(path)
    }

    /// Read the runtime file, if present.
    pub fn read(storage_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(storage_dir);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("Invalid runtime file {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    /// Remove the runtime file, ignoring a missing file.
    pub fn remove(storage_dir: &Path) {
        let _ = fs::remove_file(Self::path(storage_dir));
    }
}

/// Turn a configured bind address into a URL a local client can connect to.
/// Wildcard hosts (0.0.0.0, [::]) are replaced with loopback addresses.
pub fn local_base_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }

    let endpoint = if let Some(port) = endpoint.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{}", port)
    } else if let Some(port) = endpoint.strip_prefix("[::]:") {
        format!("[::1]:{}", port)
    } else {
        endpoint.to_string()
    };

    format!("http://{}", endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_runtime_file() {
        let dir = std::env::temp_dir().join(format!("hexdb-runtime-test-{}", ulid::Ulid::new()));
        let info = RuntimeInfo::for_current_process("127.0.0.1:7700").unwrap();
        assert_eq!(info.shutdown_token.len(), 64);

        info.write(&dir).unwrap();
        let read = RuntimeInfo::read(&dir).unwrap().unwrap();
        assert_eq!(read.pid, info.pid);
        assert_eq!(read.shutdown_token, info.shutdown_token);

        RuntimeInfo::remove(&dir);
        assert!(RuntimeInfo::read(&dir).unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn local_urls() {
        assert_eq!(local_base_url("127.0.0.1:7700"), "http://127.0.0.1:7700");
        assert_eq!(local_base_url("0.0.0.0:7700"), "http://127.0.0.1:7700");
        assert_eq!(local_base_url("[::]:7700"), "http://[::1]:7700");
        assert_eq!(local_base_url("http://db.local:7700/"), "http://db.local:7700");
    }
}
