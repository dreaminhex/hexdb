//! End-to-end test harness for HexDB.
//!
//! [`TestServer`] runs the real `hexdb_api` binary in its own temporary
//! directory with its own config, data folder, and free ports, so tests can
//! run in parallel and exercise restarts and crashes.
//!
//! The server binary is built once per test run with `cargo build -p hexdb_api`.
//! Set `HEXDB_SERVER_BIN` to use a prebuilt binary instead, and
//! `HEXDB_TEST_KEEP=1` to keep test directories for inspection.

use anyhow::{anyhow, bail, Context, Result};
use hexdb_core::{RuntimeInfo, SHUTDOWN_TOKEN_HEADER};
use reqwest::blocking::{Client, Response};
use reqwest::{header::HeaderMap, Method, StatusCode};
use serde_json::Value;
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// A fixed AES-256 key (bytes 0..32) used by every test server.
pub const TEST_ENCRYPTION_KEY: &str = "base64:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Server binary
// ---------------------------------------------------------------------------

static SERVER_BIN: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();

/// Path to an up-to-date `hexdb_api` binary, building it on first use.
pub fn server_bin() -> Result<PathBuf> {
    SERVER_BIN
        .get_or_init(|| build_server().map_err(|e| format!("{:#}", e)))
        .clone()
        .map_err(|e| anyhow!(e))
}

fn build_server() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HEXDB_SERVER_BIN").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    // Test binaries live in <target>/<profile>/deps; the server goes in <target>/<profile>.
    let exe = std::env::current_exe()?;
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("Unexpected test binary location: {}", exe.display()))?;
    let release = profile_dir.file_name().is_some_and(|n| n == "release");

    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow!("hexdb_tests must live inside the workspace"))?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--quiet", "-p", "hexdb_api"]).current_dir(workspace);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().context("Failed to run `cargo build -p hexdb_api`")?;
    if !status.success() {
        bail!("`cargo build -p hexdb_api` failed ({})", status);
    }

    let bin = profile_dir.join(format!("hexdb_api{}", std::env::consts::EXE_SUFFIX));
    if !bin.is_file() {
        bail!("Server binary not found at {}", bin.display());
    }
    Ok(bin)
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

/// A running `hexdb_api` process with its own temporary directory.
pub struct TestServer {
    dir: Option<TempDir>,
    child: Option<Child>,
    base_url: String,
    launches: u32,
    client: Client,
}

impl TestServer {
    /// Create a fresh directory and start a server in it.
    pub fn start() -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("hexdb-test-").tempdir()?;
        let mut server = Self {
            dir: Some(dir),
            child: None,
            base_url: String::new(),
            launches: 0,
            client: Client::builder().timeout(Duration::from_secs(10)).build()?,
        };
        server.launch()?;
        Ok(server)
    }

    /// The test's temporary directory (contains hexdb.toml, server.log, and data/).
    pub fn dir(&self) -> &Path {
        self.dir.as_ref().expect("test dir").path()
    }

    /// The server's storage directory.
    pub fn data_dir(&self) -> PathBuf {
        self.dir().join("data")
    }

    /// The newest write-ahead log segment, if any.
    pub fn newest_wal_segment(&self) -> Option<PathBuf> {
        let mut segments: Vec<PathBuf> = fs::read_dir(self.data_dir().join("wal"))
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "wal"))
            .collect();
        segments.sort();
        segments.pop()
    }

    /// Combined server output across all launches.
    pub fn log_path(&self) -> PathBuf {
        self.dir().join("server.log")
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Start the server process (again) on fresh ports, reusing the same data directory.
    /// Retries if a port was taken between choosing and binding it.
    pub fn launch(&mut self) -> Result<()> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.try_launch() {
                Ok(()) => return Ok(()),
                Err(e) if attempt < 3 && e.to_string().contains("Failed to bind") => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn try_launch(&mut self) -> Result<()> {
        if self.child.is_some() {
            bail!("Server is already running");
        }

        let (api, discovery) = (free_port()?, free_port()?);
        let lattice = self
            .dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Background tasks run hourly so they don't interfere unless a test triggers them.
        let config = format!(
            r#"[network]
api_endpoint = "127.0.0.1:{api}"
discovery_endpoint = "127.0.0.1:{discovery}"
lattice_name = "{lattice}"

[memory]
ram_mb = 1024
ttl_scan_frequency = 3600
vertex_integrity_check_frequency = 3600

[storage]
path = "./data"
disk_mb = 1024
encryption_key = "{key}"
compaction_frequency = 3600
wal_flush_check_frequency = 3600

[ui]
path = "./no-ui"
"#,
            key = TEST_ENCRYPTION_KEY
        );
        let config_path = self.dir().join("hexdb.toml");
        fs::write(&config_path, config)?;

        self.launches += 1;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())?;
        use std::io::Write;
        writeln!(log, "\n===== launch {} =====", self.launches)?;

        let mut cmd = Command::new(server_bin()?);
        cmd.arg("--config")
            .arg(&config_path)
            .current_dir(self.dir())
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Keep the developer's HEXDB_* settings out of the test server.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HEXDB_") {
                cmd.env_remove(&key);
            }
        }

        self.child = Some(cmd.spawn().context("Failed to start hexdb_api")?);
        self.base_url = format!("http://127.0.0.1:{}", api);

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                self.child = None;
                bail!(
                    "Server exited during startup ({}). Log tail:\n{}",
                    status,
                    self.log_tail(30)
                );
            }
            if self.health_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                self.kill();
                bail!("Server did not become healthy within {:?}. Log tail:\n{}", STARTUP_TIMEOUT, self.log_tail(30));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn health_ok(&self) -> bool {
        self.client
            .get(self.url("/health"))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Stop the server gracefully through the token-protected shutdown endpoint.
    pub fn stop(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };

        let info = RuntimeInfo::read(&self.data_dir())?
            .ok_or_else(|| anyhow!("Runtime file missing; cannot request shutdown"))?;
        self.client
            .post(self.url("/shutdown"))
            .header(SHUTDOWN_TOKEN_HEADER, &info.shutdown_token)
            .send()?
            .error_for_status()?;

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Server did not stop within {:?}", SHUTDOWN_TIMEOUT);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kill the server immediately, simulating a crash or power loss.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Graceful stop followed by a new launch on the same data.
    pub fn restart(&mut self) -> Result<()> {
        self.stop()?;
        self.launch()
    }

    /// Hard kill followed by a new launch on the same data.
    pub fn crash_and_restart(&mut self) -> Result<()> {
        self.kill();
        self.launch()
    }

    /// The last `lines` lines of the server log.
    pub fn log_tail(&self, lines: usize) -> String {
        let text = fs::read_to_string(self.log_path()).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    pub fn get(&self, path: &str) -> Result<Response> {
        Ok(self.client.get(self.url(path)).send()?)
    }

    /// Send any request and return the status, headers and JSON body
    /// (`Value::Null` for empty bodies). Errors are not treated as failures.
    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<ApiResponse> {
        let mut builder = self.client.request(method, self.url(path));
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let res = builder.send()?;
        let status = res.status();
        let headers = res.headers().clone();
        let text = res.text()?;
        let body = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        Ok(ApiResponse { status, headers, body })
    }

    /// Insert a document and return its ID.
    pub fn insert(&self, tessellation: &str, doc: &Value) -> Result<String> {
        self.insert_with_query(tessellation, "", doc)
    }

    /// Insert a document with a query string (e.g. `"?ttl=60"`) and return its ID.
    pub fn insert_with_query(&self, tessellation: &str, query: &str, doc: &Value) -> Result<String> {
        let res = self.request(Method::POST, &format!("/{}{}", tessellation, query), Some(doc), &[])?;
        if res.status != StatusCode::CREATED {
            bail!("Insert returned {}: {}", res.status, res.body);
        }
        res.body
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("Insert response has no id: {}", res.body))
    }

    /// Insert a document without reading its ID. Safe to call from several threads.
    pub fn post_document(&self, tessellation: &str, doc: &Value) -> Result<()> {
        self.client
            .post(self.url(&format!("/{}", tessellation)))
            .json(doc)
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// Fetch a document. Returns `None` when it doesn't exist.
    pub fn get_doc(&self, tessellation: &str, id: &str) -> Result<Option<Value>> {
        let res = self.get(&format!("/{}/{}", tessellation, id))?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(res.error_for_status()?.json()?))
    }

    /// Partially update a document. `doc` must include `"id"`.
    pub fn patch(&self, tessellation: &str, doc: &Value) -> Result<()> {
        let id = doc.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("patch needs an id"))?;
        self.client
            .patch(self.url(&format!("/{}/{}", tessellation, id)))
            .json(doc)
            .send()?
            .error_for_status()?;
        Ok(())
    }

    pub fn delete(&self, tessellation: &str, id: &str) -> Result<()> {
        self.client
            .delete(self.url(&format!("/{}/{}", tessellation, id)))
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// Number of documents in a tessellation (errors if it doesn't exist).
    pub fn count(&self, tessellation: &str) -> Result<usize> {
        let body: Value = self.get(&format!("/{}/count", tessellation))?.error_for_status()?.json()?;
        body.get("count")
            .and_then(Value::as_u64)
            .map(|c| c as usize)
            .with_context(|| format!("Unexpected count response: {}", body))
    }

    /// POST a JSON body and return the status code without treating errors as failures.
    pub fn post_status(&self, path: &str, body: &Value) -> Result<StatusCode> {
        Ok(self.client.post(self.url(path)).json(body).send()?.status())
    }

    /// Delete a tessellation and all of its documents.
    pub fn delete_tessellation(&self, name: &str) -> Result<()> {
        self.client
            .delete(self.url(&format!("/tessellations/{}", name)))
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// Flush in-memory documents to SSTables.
    pub fn flush(&self) -> Result<()> {
        self.client.post(self.url("/flush")).send()?.error_for_status()?;
        Ok(())
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let failed = thread::panicking();
        if failed {
            eprintln!("---- server log tail ({}) ----\n{}", self.log_path().display(), self.log_tail(60));
        }
        self.kill();

        if failed || std::env::var_os("HEXDB_TEST_KEEP").is_some() {
            if let Some(dir) = self.dir.take() {
                eprintln!("Keeping test directory {}", dir.keep().display());
            }
        }
    }
}

/// Read a document field. Accepts both the current tagged format
/// (`{"type": "...", "value": ...}` under `data`) and plain JSON, so tests
/// keep working when the API starts returning plain documents.
/// A response from [`TestServer::request`].
#[derive(Debug)]
pub struct ApiResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl ApiResponse {
    /// True if the response was replayed from an idempotency record.
    pub fn replayed(&self) -> bool {
        self.headers.get("idempotent-replayed").is_some_and(|v| v == "true")
    }

    /// The error code from an error response.
    pub fn error_code(&self) -> Option<&str> {
        self.body.pointer("/error/code").and_then(Value::as_str)
    }
}

pub fn field(doc: &Value, name: &str) -> Option<Value> {
    let data = doc.get("data").unwrap_or(doc);
    data.get(name).map(untag)
}

/// Recursively convert tagged values (`{"type": "...", "value": ...}`) to plain JSON.
pub fn untag(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() <= 2 && map.get("type").is_some_and(Value::is_string) => {
            match map.get("value") {
                Some(inner) => untag(inner),
                None => Value::Null, // {"type": "Null"}
            }
        }
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), untag(v))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(untag).collect()),
        other => other.clone(),
    }
}
