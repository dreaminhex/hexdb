// HexDB Core Plugins
//
// Plugins consume the change feed: every committed write to a user
// tessellation, in order, as it happens. They run while this hex is the
// Overseer (so a lattice delivers each change once).
//
// The registry (`plugins.registry` in hexdb.toml, default `plugins.json`)
// maps plugin IDs to folders holding a `plugin.toml` manifest:
//
//   { "@streams/kafka": { "path": "./plugins/streams/kafka", "enabled": true } }
//
// A manifest declares one of two runtimes:
//
//   # Process: HexDB starts the command in the plugin folder and writes each
//   # change to its stdin as one JSON line. Its stdout/stderr go to the HexDB
//   # log. It is restarted if it exits.
//   id = "@examples/change-logger"
//   name = "Change logger"
//   type = "stream"
//   version = "0.1.0"
//   command = ["node", "change-logger.mjs"]
//
//   # Webhook: HexDB POSTs batches of changes (a JSON array) to a URL.
//   [webhook]
//   url = "http://localhost:9000/hexdb"
//   headers = { Authorization = "Bearer ..." }
//   batch_size = 100
//
// Optional: `tessellations = ["orders", "customers"]` limits the changes sent.
//
// A change is {"seq", "timestamp", "op": "put" | "delete" | "drop_tessellation",
// "tessellation", "id", "document"}, the same as `GET /changes`.
//
// Delivery is at-most-once across restarts: a plugin starts at the current end
// of the feed, and changes made while it is down or restarting are skipped
// (logged as a warning). Use `GET /changes` with a stored cursor when every
// change must be processed.

use crate::{
    changes::{Change, ChangeKind},
    engine::HexDBEngine,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast::error::RecvError, watch};
use tracing::{debug, error, info, warn};

const RESTART_DELAY: Duration = Duration::from_secs(5);

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
}

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
    /// "process" or "webhook" (empty if the manifest couldn't be read).
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

fn load_manifest(dir: &Path) -> Result<Manifest> {
    let file = dir.join("plugin.toml");
    let text = std::fs::read_to_string(&file).with_context(|| format!("can't read {}", file.display()))?;
    let manifest: Manifest = toml_from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
    match (&manifest.command, &manifest.webhook) {
        (Some(_), Some(_)) => bail!("declare either command or [webhook], not both"),
        (None, None) => bail!("declares neither a command nor a [webhook]; nothing to run"),
        _ => Ok(manifest),
    }
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
                status.runtime = if m.webhook.is_some() { "webhook".into() } else { "process".into() };
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

fn wanted(manifest: &Manifest, engine: &HexDBEngine, change: &Change) -> bool {
    if !manifest.tessellations.is_empty() && !manifest.tessellations.contains(&change.tessellation) {
        return false;
    }
    if change.kind == ChangeKind::DropTessellation {
        return !change.tessellation.starts_with('_');
    }
    !engine.is_system_tessellation(&change.tessellation)
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

/// Deliver changes until the plugin fails or this hex stops being the Overseer.
async fn run_once(engine: &Arc<HexDBEngine>, manifest: &Manifest, dir: &Path) -> Result<()> {
    let (_, mut receiver) = engine
        .changes
        .follow(engine.changes.published_seq())
        .map_err(|_| anyhow!("change feed unavailable"))?;
    let id = manifest.id.clone();

    match (&manifest.command, &manifest.webhook) {
        (Some(command), _) => {
            let args: Vec<String> = match command {
                Command::Args(args) => args.clone(),
                Command::Line(line) => line.split_whitespace().map(String::from).collect(),
            };
            let (program, rest) = args.split_first().ok_or_else(|| anyhow!("command is empty"))?;
            let mut child = tokio::process::Command::new(program)
                .args(rest)
                .current_dir(dir)
                .envs(&manifest.env)
                .env("HEXDB_PLUGIN_ID", &id)
                .env("HEXDB_API", format!("http://{}", engine.config.network.api_endpoint))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("couldn't start {:?} in {}", args, dir.display()))?;
            info!(plugin = %id, "🧩 Started plugin '{}' (pid {}).", id, child.id().unwrap_or_default());
            for (stream, is_err) in [
                (child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), false),
                (child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), true),
            ] {
                if let Some(stream) = stream {
                    let id = id.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stream).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if is_err {
                                warn!(target: "hexdb_core::plugins", plugin = %id, "[{}] {}", id, line);
                            } else {
                                info!(target: "hexdb_core::plugins", plugin = %id, "[{}] {}", id, line);
                            }
                        }
                    });
                }
            }
            let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            loop {
                tokio::select! {
                    status = child.wait() => {
                        let status = status?;
                        bail!("the process exited ({})", status);
                    }
                    next = receiver.recv() => {
                        if !engine.is_writable() {
                            let _ = child.kill().await;
                            info!(plugin = %id, "🧩 Plugin '{}' paused: this hex is no longer the Overseer.", id);
                            return Ok(());
                        }
                        match next {
                            Ok(change) => {
                                if !wanted(manifest, engine, &change) {
                                    continue;
                                }
                                let mut line = change.to_api_json().to_string();
                                line.push('\n');
                                stdin.write_all(line.as_bytes()).await.context("writing to the plugin's stdin")?;
                                stdin.flush().await?;
                                let seq = change.seq;
                                engine.plugins.update(&id, |p| { p.delivered += 1; p.last_seq = seq; });
                            }
                            Err(RecvError::Lagged(n)) => {
                                warn!(plugin = %id, "⚠️ Plugin '{}' fell behind and skipped {} changes.", id, n);
                                engine.plugins.update(&id, |p| p.skipped += n);
                            }
                            Err(RecvError::Closed) => return Ok(()),
                        }
                    }
                }
            }
        }
        (None, Some(webhook)) => {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
            let batch_size = webhook.batch_size.clamp(1, 10_000);
            loop {
                // Wait for one change, then collect whatever else is ready, up to the batch size.
                let mut batch = Vec::new();
                let first = receiver.recv().await;
                let mut next = Some(first);
                while let Some(result) = next.take() {
                    match result {
                        Ok(change) => {
                            if wanted(manifest, engine, &change) {
                                batch.push(change);
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            warn!(plugin = %id, "⚠️ Plugin '{}' fell behind and skipped {} changes.", id, n);
                            engine.plugins.update(&id, |p| p.skipped += n);
                        }
                        Err(RecvError::Closed) => return Ok(()),
                    }
                    if batch.len() < batch_size {
                        if let Ok(more) = receiver.try_recv() {
                            next = Some(Ok(more));
                        }
                    }
                }
                if !engine.is_writable() {
                    return Ok(());
                }
                if batch.is_empty() {
                    continue;
                }
                let body: Vec<serde_json::Value> = batch.iter().map(|c| c.to_api_json()).collect();
                let mut request = client.post(&webhook.url).json(&body).header("x-hexdb-plugin", &id);
                for (k, v) in &webhook.headers {
                    request = request.header(k, v);
                }
                let response = request.send().await.with_context(|| format!("POST {}", webhook.url))?;
                if !response.status().is_success() {
                    bail!("POST {} returned {}", webhook.url, response.status());
                }
                let (count, seq) = (batch.len() as u64, batch.last().map(|c| c.seq).unwrap_or_default());
                debug!(plugin = %id, "🧩 Delivered {} change(s) to {}.", count, webhook.url);
                engine.plugins.update(&id, |p| {
                    p.delivered += count;
                    p.last_seq = seq;
                });
            }
        }
        (None, None) => bail!("nothing to run"),
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
}
