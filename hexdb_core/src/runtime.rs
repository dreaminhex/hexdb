// HexDB Runtime Information
// A running server writes a small JSON file into its data directory with its
// process ID, executable, start time, API endpoint, and a random one-time
// shutdown token. The CLI reads it to find the server and to ask it to shut
// down gracefully. The file is readable by the owner only (Unix mode 0600; on
// Windows, inherited permissions are removed and only the current user and
// SYSTEM keep access), because the token can stop the server.

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
    /// The server's executable, so `hexdb stop --force` can check the PID
    /// still belongs to HexDB and not to an unrelated process that reused it.
    #[serde(default)]
    pub exe: String,
    /// When the server process started (epoch milliseconds).
    #[serde(default)]
    pub started_at: i64,
    /// True when the API is served over HTTPS.
    #[serde(default)]
    pub tls: bool,
}

impl RuntimeInfo {
    /// Create runtime info for the current process with a fresh random
    /// shutdown token. `started_at` is when the process started (epoch ms).
    pub fn for_current_process(api_endpoint: &str, started_at: i64, tls: bool) -> Result<Self> {
        let mut token = [0u8; 32];
        getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("Failed to generate shutdown token: {}", e))?;

        Ok(Self {
            pid: std::process::id(),
            api_endpoint: api_endpoint.to_string(),
            shutdown_token: hex::encode(token),
            exe: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            started_at,
            tls,
        })
    }

    /// Path of the runtime file for a storage directory.
    pub fn path(storage_dir: &Path) -> PathBuf {
        storage_dir.join(PID_FILE_NAME)
    }

    /// Write the runtime file, readable by the owner only.
    pub fn write(&self, storage_dir: &Path) -> Result<PathBuf> {
        fs::create_dir_all(storage_dir)
            .with_context(|| format!("Failed to create {}", storage_dir.display()))?;
        let path = Self::path(storage_dir);
        write_private_file(&path, serde_json::to_string_pretty(self)?.as_bytes())?;
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

/// Write a file that only the current user can read: mode 0600 on Unix; on
/// Windows, inheritance is removed and only the current user and SYSTEM are
/// granted access. The file is created empty and restricted before the
/// contents are written, so they are never readable by others.
pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` only applies to new files; tighten an existing one too.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) if !domain.is_empty() => format!("{}\\{}", domain, user),
        (_, Ok(user)) => user,
        _ => anyhow::bail!("can't determine the current user to restrict {}", path.display()),
    };
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let icacls = Path::new(&system_root).join("System32").join("icacls.exe");
    let output = std::process::Command::new(icacls)
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{}:F", user))
        // SYSTEM by well-known SID, so this works on non-English Windows.
        .args(["/grant:r", "*S-1-5-18:F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("Failed to run icacls on {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "icacls couldn't restrict {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

/// Turn a configured bind address into a URL a local client can connect to.
/// Wildcard hosts (0.0.0.0, [::]) are replaced with loopback addresses.
pub fn local_base_url(endpoint: &str) -> String {
    local_base_url_with(endpoint, false)
}

/// Like [`local_base_url`], with `https://` when `tls` is set.
pub fn local_base_url_with(endpoint: &str, tls: bool) -> String {
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

    format!("{}://{}", if tls { "https" } else { "http" }, endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_runtime_file() {
        let dir = std::env::temp_dir().join(format!("hexdb-runtime-test-{}", ulid::Ulid::new()));
        let info = RuntimeInfo::for_current_process("127.0.0.1:7700", 1, false).unwrap();
        assert_eq!(info.shutdown_token.len(), 64);

        info.write(&dir).unwrap();
        let read = RuntimeInfo::read(&dir).unwrap().unwrap();
        assert_eq!(read.pid, info.pid);
        assert_eq!(read.shutdown_token, info.shutdown_token);
        assert!(read.exe.contains("hexdb_core"), "{}", read.exe);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(RuntimeInfo::path(&dir)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

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
