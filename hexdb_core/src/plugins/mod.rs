// HexDB Core Plugins
//
// Plugins extend HexDB without changing it. Each one is a folder with a
// `plugin.toml` manifest, listed in the registry (`plugins.registry` in
// hexdb.toml, default `plugins.json`):
//
//   { "@streams/kafka": { "path": "./plugins/streams/kafka", "enabled": true } }
//
// What a plugin receives depends on its `type`:
//
//   stream   every committed change to user tessellations, in order (the
//            same JSON as `GET /changes`). `tessellations = [...]` narrows it;
//            `audit = true` adds the audit trail (`_audit`).
//   logs     server log records (`[logs] level = "info"`, `targets = [...]`).
//   metrics  a metrics sample every 15 seconds.
//   source   nothing; the plugin brings data in through the API (ingest).
//
// And how it runs:
//
//   command = ["node", "index.mjs"]   a process in the plugin folder that gets
//                                     each payload as one JSON line on stdin;
//                                     its output goes to the HexDB log; it is
//                                     restarted if it exits.
//   [webhook] url = "https://..."     batches POSTed as a JSON array.
//   builtin = "otlp"                  the OpenTelemetry exporter, for logs or
//                                     metrics (`[otlp] endpoint = ...`).
//
// `[access] role = "writer", tessellations = ["orders"]` gives a process
// plugin its own user with that role and an API key (HEXDB_API_KEY, with
// HEXDB_API as the base URL), so it can read and write through the API.
//
// Plugins run on the Overseer only, so a lattice delivers each payload once.
//
// Stream delivery is at-least-once: each plugin's position is saved in the
// `_plugin_cursors` system tessellation (about once a second, and when it
// stops), and a restarted plugin continues from there, reading older changes
// from the durable change history. A change may be delivered again after a
// crash, so consumers should be idempotent (use `seq`). A new plugin starts at
// the current end of the feed. After a failover the new Overseer has a
// different change history, so plugins start at its end (logged as a warning).
// Logs and metrics are delivered from the moment the plugin starts.
//
// Isolation: a process plugin runs with a clean environment (only PATH and the
// basic variables a runtime needs, its manifest's `env`, the server variables
// named in `pass_env`, and HEXDB_*), in its own folder. On Unix, HexDB refuses a registry or manifest that other users
// can write. Plugins still run with the server's privileges, so only install
// plugins you trust, and keep the registry writable by administrators only.
// Webhooks must be http(s) URLs and redirects aren't followed.

mod access;
mod feeds;
pub mod otlp;

pub use access::{login_for, AccessConfig};

use crate::engine::HexDBEngine;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

const RESTART_DELAY: Duration = Duration::from_secs(5);
/// System tessellation of plugin positions in the change feed.
pub const PLUGIN_CURSORS_TESSELLATION: &str = "_plugin_cursors";
/// How often a running plugin's position is saved.
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// Longest line read from a plugin's output.
const MAX_LINE: usize = 64 * 1024;
/// Environment variables passed through to process plugins.
const PASSED_ENV: &[&str] = &[
    "PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "TEMP", "TMP", "TMPDIR", "HOME", "USERPROFILE", "APPDATA",
    "LOCALAPPDATA", "LANG", "LC_ALL", "TZ",
];

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntry {
    pub path: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Command {
    Line(String),
    Args(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_batch")]
    pub batch_size: usize,
}

fn default_batch() -> usize {
    100
}

/// `plugin.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default = "default_type")]
    pub kind: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub command: Option<Command>,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    /// Only send changes to these tessellations (default: all user tessellations).
    #[serde(default)]
    pub tessellations: Vec<String>,
    /// Extra environment variables for a process plugin.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables passed through from the server's own environment (for
    /// secrets such as API keys, so they needn't be written in plugin.toml).
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Also deliver the audit trail.
    #[serde(default)]
    pub audit: bool,
    /// A runtime built into HexDB instead of a command or webhook ("otlp").
    #[serde(default)]
    pub builtin: Option<String>,
    #[serde(default)]
    pub otlp: Option<otlp::OtlpConfig>,
    /// Filters for `logs` plugins.
    #[serde(default)]
    pub logs: LogsConfig,
    /// API access for a process plugin.
    #[serde(default)]
    pub access: Option<AccessConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogsConfig {
    /// Least severe level sent (error, warn, info, debug, trace). Default: everything kept.
    #[serde(default)]
    pub level: Option<String>,
    /// Only records whose target starts with one of these.
    #[serde(default)]
    pub targets: Vec<String>,
}

/// What a plugin receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Stream,
    Logs,
    Metrics,
    Source,
}

impl Manifest {
    pub fn kind(&self) -> PluginKind {
        match self.kind.as_str() {
            "logs" => PluginKind::Logs,
            "metrics" => PluginKind::Metrics,
            "source" => PluginKind::Source,
            _ => PluginKind::Stream,
        }
    }

    fn runtime(&self) -> &'static str {
        if self.builtin.is_some() {
            "builtin"
        } else if self.webhook.is_some() {
            "webhook"
        } else {
            "process"
        }
    }
}

const KINDS: &[&str] = &["stream", "logs", "metrics", "source"];

fn default_type() -> String {
    "stream".into()
}

/// A plugin's state, for `GET /plugins` and the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct PluginStatus {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub version: String,
    pub description: String,
    /// "process", "webhook" or "builtin" (empty if the manifest couldn't be read).
    pub runtime: String,
    pub path: String,
    /// running, standby (this hex isn't the Overseer), disabled, invalid, or error.
    pub state: String,
    pub delivered: u64,
    pub last_seq: u64,
    pub skipped: u64,
    pub restarts: u64,
    pub last_error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
}

/// All plugins' statuses.
#[derive(Default)]
pub struct PluginRegistry {
    pub plugins: Mutex<Vec<PluginStatus>>,
}

impl PluginRegistry {
    pub fn list(&self) -> Vec<PluginStatus> {
        self.plugins.lock().unwrap().clone()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut PluginStatus)) {
        if let Some(p) = self.plugins.lock().unwrap().iter_mut().find(|p| p.id == id) {
            f(p);
        }
    }
}

/// Resolve the registry path: absolute, or relative to the config file's folder
/// (or the working directory when there's no config file).
pub fn registry_path(engine: &HexDBEngine) -> PathBuf {
    let path = PathBuf::from(&engine.config.plugins.registry);
    if path.is_absolute() {
        return path;
    }
    match engine.config.source.as_ref().and_then(|s| s.parent()) {
        Some(dir) => dir.join(path),
        None => path,
    }
}

/// Refuse files other users could change (they decide what the server runs).
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).with_context(|| format!("can't read {}", path.display()))?;
    if meta.mode() & 0o022 != 0 {
        bail!("{} is writable by other users; HexDB won't run plugins from it (chmod go-w)", path.display());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc_getuid() };
    if meta.uid() != uid && meta.uid() != 0 {
        bail!("{} is owned by another user; HexDB won't run plugins from it", path.display());
    }
    Ok(())
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    // Windows ACLs: keep the registry and plugin folders writable by
    // administrators only (documented); not checked here.
    Ok(())
}

fn load_manifest(dir: &Path) -> Result<Manifest> {
    let file = dir.join("plugin.toml");
    check_permissions(&file)?;
    let text = std::fs::read_to_string(&file).with_context(|| format!("can't read {}", file.display()))?;
    let manifest: Manifest = toml_from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
    if let Some(name) = manifest.env.keys().chain(manifest.pass_env.iter()).find(|k| k.to_ascii_uppercase().starts_with("HEXDB_")) {
        bail!("env and pass_env can't include {}; HEXDB_* variables are set by HexDB", name);
    }
    if let Some(webhook) = &manifest.webhook {
        let url = reqwest::Url::parse(&webhook.url).with_context(|| format!("webhook url '{}' is invalid", webhook.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("webhook url must be http or https, not {}", url.scheme());
        }
    }
    if !KINDS.contains(&manifest.kind.as_str()) {
        bail!("type must be one of {}, not '{}'", KINDS.join(", "), manifest.kind);
    }
    let runtimes = [manifest.command.is_some(), manifest.webhook.is_some(), manifest.builtin.is_some()].iter().filter(|r| **r).count();
    match runtimes {
        0 => bail!("declares no command, [webhook] or builtin; nothing to run"),
        1 => {}
        _ => bail!("declare only one of command, [webhook] and builtin"),
    }
    if let Some(builtin) = &manifest.builtin {
        if builtin != "otlp" {
            bail!("unknown builtin '{}' (available: otlp)", builtin);
        }
        if manifest.otlp.is_none() {
            bail!("builtin = \"otlp\" needs an [otlp] section with an endpoint");
        }
        if !matches!(manifest.kind(), PluginKind::Logs | PluginKind::Metrics) {
            bail!("the OTLP exporter sends logs or metrics; set type to \"logs\" or \"metrics\"");
        }
    }
    if manifest.kind() == PluginKind::Source && manifest.command.is_none() {
        bail!("a source plugin needs a command (it brings data in through the API)");
    }
    if manifest.access.is_some() && manifest.command.is_none() {
        bail!("[access] is for process plugins (the API key is passed in HEXDB_API_KEY)");
    }
    Ok(manifest)
}

fn toml_from_str(text: &str) -> Result<Manifest> {
    Ok(toml::from_str(text)?)
}

/// Load the registry and start every enabled plugin.
pub fn spawn_plugins(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    if !engine.config.plugins.enabled {
        info!("🧩 Plugins are disabled (plugins.enabled = false).");
        return;
    }
    let path = registry_path(&engine);
    if path.exists() {
        if let Err(e) = check_permissions(&path) {
            error!("❌ Plugins are not loaded: {:#}", e);
            return;
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("🧩 No plugin registry at {}; no plugins loaded.", path.display());
            return;
        }
        Err(e) => {
            error!("❌ Can't read the plugin registry {}: {}", path.display(), e);
            return;
        }
    };
    let entries: BTreeMap<String, RegistryEntry> = match serde_json::from_str(&text) {
        Ok(entries) => entries,
        Err(e) => {
            error!("❌ The plugin registry {} is invalid: {}", path.display(), e);
            return;
        }
    };
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();

    for (id, entry) in entries {
        let dir = base.join(&entry.path);
        let mut status = PluginStatus {
            id: id.clone(),
            name: id.clone(),
            kind: entry.kind.clone().unwrap_or_else(default_type),
            version: String::new(),
            description: String::new(),
            runtime: String::new(),
            path: dir.display().to_string(),
            state: "invalid".into(),
            delivered: 0,
            last_seq: 0,
            skipped: 0,
            restarts: 0,
            last_error: None,
            started_at: None,
        };
        let manifest = load_manifest(&dir).and_then(|m| {
            if m.id != id {
                bail!("the manifest's id '{}' doesn't match the registry key '{}'", m.id, id);
            }
            Ok(m)
        });
        match manifest {
            Ok(m) => {
                status.name = m.name.clone();
                status.kind = m.kind.clone();
                status.version = m.version.clone();
                status.description = m.description.clone();
                status.runtime = m.runtime().into();
                status.state = if entry.enabled { "standby".into() } else { "disabled".into() };
                engine.plugins.plugins.lock().unwrap().push(status);
                if entry.enabled {
                    info!("🧩 Loaded plugin '{}' ({} {}).", id, m.name, m.version);
                    tokio::spawn(run_plugin(engine.clone(), m, dir, shutdown_rx.clone()));
                } else {
                    info!("🧩 Plugin '{}' is disabled in the registry.", id);
                }
            }
            Err(e) => {
                warn!("⚠️ Plugin '{}' wasn't loaded: {:#}", id, e);
                status.last_error = Some(format!("{:#}", e));
                engine.plugins.plugins.lock().unwrap().push(status);
            }
        }
    }
}

/// Keep a plugin running while this hex is the Overseer.
async fn run_plugin(engine: Arc<HexDBEngine>, manifest: Manifest, dir: PathBuf, mut shutdown_rx: watch::Receiver<()>) {
    let id = manifest.id.clone();
    let mut first = true;
    loop {
        if !engine.is_writable() {
            engine.plugins.update(&id, |p| p.state = "standby".into());
            tokio::select! {
                _ = shutdown_rx.changed() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
            }
        }
        if !first {
            engine.plugins.update(&id, |p| p.restarts += 1);
        }
        first = false;
        engine.plugins.update(&id, |p| {
            p.state = "running".into();
            p.started_at = Some(Utc::now());
        });
        let result = tokio::select! {
            _ = shutdown_rx.changed() => return,
            result = run_once(&engine, &manifest, &dir) => result,
        };
        match result {
            Ok(()) => engine.plugins.update(&id, |p| p.state = "standby".into()),
            Err(e) => {
                warn!(plugin = %id, "⚠️ Plugin '{}' stopped: {:#}. Restarting in {}s.", id, e, RESTART_DELAY.as_secs());
                engine.plugins.update(&id, |p| {
                    p.state = "error".into();
                    p.last_error = Some(format!("{:#}", e));
                });
                tokio::select! {
                    _ = shutdown_rx.changed() => return,
                    _ = tokio::time::sleep(RESTART_DELAY) => {}
                }
            }
        }
    }
}

/// Start the plugin's process (if it has a command) with its environment.
async fn start_process(engine: &Arc<HexDBEngine>, manifest: &Manifest, dir: &Path, command: &Command) -> Result<tokio::process::Child> {
    let id = &manifest.id;
    let args: Vec<String> = match command {
        Command::Args(args) => args.clone(),
        Command::Line(line) => line.split_whitespace().map(String::from).collect(),
    };
    let (program, rest) = args.split_first().ok_or_else(|| anyhow!("command is empty"))?;
    let mut process = tokio::process::Command::new(program);
    process.args(rest).current_dir(dir).env_clear();
    for name in PASSED_ENV {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    let scheme = if engine.config.tls.enabled() { "https" } else { "http" };
    let api_host = engine.config.network.api_endpoint.replace("0.0.0.0", "127.0.0.1");
    for name in &manifest.pass_env {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    process
        .envs(&manifest.env)
        .env("HEXDB_PLUGIN_ID", id)
        .env("HEXDB_PLUGIN_TYPE", &manifest.kind)
        .env("HEXDB_API", format!("{}://{}", scheme, api_host));
    if !engine.config.tls.ca_file.is_empty() {
        // Also where Node.js and Python's requests look for extra CA certificates.
        process
            .env("HEXDB_CA_FILE", &engine.config.tls.ca_file)
            .env("NODE_EXTRA_CA_CERTS", &engine.config.tls.ca_file)
            .env("REQUESTS_CA_BUNDLE", &engine.config.tls.ca_file);
    }
    if let Some(access) = &manifest.access {
        let key = access::api_key(engine, manifest, access).await.context("creating the plugin's API key")?;
        process.env("HEXDB_API_KEY", key);
    }
    crate::process::contain(&mut process);
    let mut child = process
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("couldn't start {:?} in {}", args, dir.display()))?;
    crate::process::adopt(&child);
    info!(plugin = %id, "🧩 Started plugin '{}' (pid {}).", id, child.id().unwrap_or_default());
    for (stream, is_err) in [
        (child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), false),
        (child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), true),
    ] {
        if let Some(stream) = stream {
            let id = id.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    // Bounded, so a plugin can't exhaust memory with one endless line.
                    match (&mut reader).take(MAX_LINE as u64).read_until(b'\n', &mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if is_err {
                        warn!(target: "hexdb_core::plugins", plugin = %id, "[{}] {}", id, text);
                    } else {
                        info!(target: "hexdb_core::plugins", plugin = %id, "[{}] {}", id, text);
                    }
                }
            });
        }
    }
    Ok(child)
}

/// Saves a stream plugin's position at most once per SAVE_EVERY (and on the way out).
struct Checkpoint {
    saved: u64,
    at: std::time::Instant,
}

impl Checkpoint {
    async fn save(&mut self, engine: &HexDBEngine, id: &str, position: Option<u64>, force: bool) {
        let Some(position) = position else { return };
        if position != self.saved && (force || self.at.elapsed() >= SAVE_EVERY) {
            feeds::save_cursor(engine, id, position).await;
            self.saved = position;
            self.at = std::time::Instant::now();
        }
    }
}

fn note_skipped(engine: &HexDBEngine, id: &str, feed: &mut feeds::Feed) {
    let skipped = feed.take_skipped();
    if skipped > 0 {
        warn!(plugin = %id, "⚠️ Plugin '{}' skipped {} change positions that were no longer in the history.", id, skipped);
        engine.plugins.update(id, |p| p.skipped += skipped);
    }
}

fn note_delivered(engine: &HexDBEngine, id: &str, batch: &feeds::Batch) {
    let count = batch.payloads.len() as u64;
    if count == 0 {
        return;
    }
    let seq = batch.position;
    engine.plugins.update(id, |p| {
        p.delivered += count;
        if let Some(seq) = seq {
            p.last_seq = seq;
        }
    });
}

/// Deliver until the plugin fails or this hex stops being the Overseer.
async fn run_once(engine: &Arc<HexDBEngine>, manifest: &Manifest, dir: &Path) -> Result<()> {
    let id = manifest.id.clone();
    let mut feed = feeds::Feed::open(engine, manifest).await;
    let mut checkpoint = Checkpoint { saved: feed.position().unwrap_or_default(), at: std::time::Instant::now() };

    if let Some(command) = &manifest.command {
        let mut child = start_process(engine, manifest, dir, command).await?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        if manifest.kind() == PluginKind::Source {
            // Sources receive nothing; closing stdin tells them so.
            drop(stdin);
            loop {
                tokio::select! {
                    status = child.wait() => bail!("the process exited ({})", status?),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if !engine.is_writable() {
                            let _ = child.kill().await;
                            info!(plugin = %id, "🧩 Plugin '{}' paused: this hex is no longer the Overseer.", id);
                            return Ok(());
                        }
                    }
                }
            }
        }
        let result: Result<()> = loop {
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    break Err(anyhow!("the process exited ({})", status));
                }
                _ = tokio::time::sleep(SAVE_EVERY) => {
                    checkpoint.save(engine, &id, feed.position(), false).await;
                    if !engine.is_writable() {
                        let _ = child.kill().await;
                        info!(plugin = %id, "🧩 Plugin '{}' paused: this hex is no longer the Overseer.", id);
                        break Ok(());
                    }
                }
                batch = feed.next_batch(engine, manifest, 1000) => {
                    let Some(batch) = batch else { break Ok(()) };
                    if !engine.is_writable() {
                        let _ = child.kill().await;
                        info!(plugin = %id, "🧩 Plugin '{}' paused: this hex is no longer the Overseer.", id);
                        break Ok(());
                    }
                    let mut text = String::new();
                    for payload in &batch.payloads {
                        text.push_str(&payload.to_string());
                        text.push('\n');
                    }
                    if !text.is_empty() {
                        if let Err(e) = stdin.write_all(text.as_bytes()).await {
                            break Err(anyhow::Error::new(e).context("writing to the plugin's stdin"));
                        }
                        if let Err(e) = stdin.flush().await {
                            break Err(e.into());
                        }
                    }
                    note_delivered(engine, &id, &batch);
                    note_skipped(engine, &id, &mut feed);
                    checkpoint.save(engine, &id, batch.position, false).await;
                }
            }
        };
        checkpoint.save(engine, &id, feed.position(), true).await;
        return result;
    }

    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
    let batch_size = manifest.webhook.as_ref().map(|w| w.batch_size).unwrap_or(500).clamp(1, 10_000);
    loop {
        let Some(batch) = feed.next_batch(engine, manifest, batch_size).await else { return Ok(()) };
        if !engine.is_writable() {
            return Ok(());
        }
        note_skipped(engine, &id, &mut feed);
        if !batch.payloads.is_empty() {
            if let Some(webhook) = &manifest.webhook {
                let mut request = client.post(&webhook.url).json(&batch.payloads).header("x-hexdb-plugin", &id).header("x-hexdb-plugin-type", &manifest.kind);
                for (k, v) in &webhook.headers {
                    request = request.header(k, v);
                }
                let response = request.send().await.with_context(|| format!("POST {}", webhook.url))?;
                if !response.status().is_success() {
                    bail!("POST {} returned {}", webhook.url, response.status());
                }
            } else if let Some(config) = &manifest.otlp {
                match manifest.kind() {
                    PluginKind::Metrics => otlp::send(&client, config, "metrics", &otlp::metrics_body(engine, config, &batch.samples)).await?,
                    PluginKind::Logs => otlp::send(&client, config, "logs", &otlp::logs_body(engine, config, &batch.logs)).await?,
                    _ => {}
                }
            }
            debug!(plugin = %id, "🧩 Delivered {} payload(s).", batch.payloads.len());
            note_delivered(engine, &id, &batch);
        }
        // Delivered (or nothing to deliver): the position can move on.
        checkpoint.save(engine, &id, batch.position, batch.payloads.len() == batch_size).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifests() {
        let m = toml_from_str(
            r#"
            id = "@x/y"
            name = "Y"
            command = ["node", "y.mjs"]
            tessellations = ["orders"]
            [env]
            LEVEL = "debug"
            "#,
        )
        .unwrap();
        assert!(matches!(m.command, Some(Command::Args(ref a)) if a == &["node", "y.mjs"]));
        assert_eq!(m.kind, "stream");
        assert_eq!(m.env.get("LEVEL").map(String::as_str), Some("debug"));

        let m = toml_from_str(
            r#"
            id = "@x/hook"
            name = "Hook"
            [webhook]
            url = "http://localhost:9000"
            batch_size = 5
            "#,
        )
        .unwrap();
        assert_eq!(m.webhook.unwrap().batch_size, 5);
    }

    #[test]
    fn example_plugins_are_valid() {
        let registry = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins.json");
        let entries: BTreeMap<String, RegistryEntry> = serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
        assert!(entries.len() >= 10);
        for (id, entry) in entries {
            let dir = registry.parent().unwrap().join(&entry.path);
            let manifest = load_manifest(&dir).unwrap_or_else(|e| panic!("{}: {:#}", id, e));
            assert_eq!(manifest.id, id);
            assert_eq!(entry.kind.as_deref(), Some(manifest.kind.as_str()), "{}: the registry type matches", id);
        }
    }
}
