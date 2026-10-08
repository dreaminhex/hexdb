// `hexdb lattice ...`: run extra hexes on this machine, to try a lattice
// (replication, failover) without more servers.
//
// `spawn` creates a folder per hex under `.hexdb-local/` next to the main
// hex's hexdb.toml, with its own config and data directory, on free ports. The
// new hexes join the main hex's lattice: same lattice name, the main hex as a
// seed, and the same lattice secret (or, without one, the same storage key,
// from which the lattice key is derived). They start as Harvesters (read
// replicas that can be elected) unless `--role` says otherwise. `list` shows
// them and `stop` stops them (`--remove` also deletes their folders).

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hexdb_core::{runtime::local_base_url_with, HexConfig, RuntimeInfo};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// One spawned hex, as recorded in `.hexdb-local/lattice.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalHex {
    name: String,
    dir: PathBuf,
    api_endpoint: String,
    discovery_endpoint: String,
    pid: u32,
}

/// A new random 32-byte key, formatted for hexdb.toml.
pub fn new_secret() -> String {
    format!("base64:{}", STANDARD.encode(hexdb_core::random_bytes(32)))
}

fn local_dir(config: &HexConfig) -> PathBuf {
    config
        .source
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hexdb-local")
}

fn registry_path(base: &Path) -> PathBuf {
    base.join("lattice.json")
}

fn load_registry(base: &Path) -> Vec<LocalHex> {
    fs::read_to_string(registry_path(base)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_registry(base: &Path, hexes: &[LocalHex]) -> Result<()> {
    fs::create_dir_all(base)?;
    fs::write(registry_path(base), serde_json::to_string_pretty(hexes)?)?;
    Ok(())
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// A free discovery port, preferably in 7702-7709 (which hexes scan on their own machine).
fn discovery_port(taken: &[u16]) -> Result<u16> {
    for port in 7703..=7709 {
        if !taken.contains(&port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    free_port()
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Start `count` hexes that join this config's lattice.
pub async fn spawn(config: &HexConfig, count: usize, role: &str, server: &Path) -> Result<()> {
    if !matches!(role, "auto" | "harvester" | "replicant") {
        bail!("--role must be auto, harvester or replicant");
    }
    let keys = config.key_ring().context("The main hex's storage key is needed to join its lattice")?;
    let (lattice, _) = hexdb_core::network::discovery::resolve_lattice_name(config, &keys)?;
    if config.network.lattice_name.trim().is_empty() && hexdb_core::catalog::Catalog::load(&config.storage_dir(), &keys)?.is_none() {
        bail!("Start the main hex once first (hexdb start), so its lattice has a name.");
    }
    let base = local_dir(config);
    let mut hexes = load_registry(&base);
    let main_discovery = config.network.discovery_endpoint.replace("0.0.0.0", "127.0.0.1");
    let mut taken: Vec<u16> = hexes.iter().filter_map(|h| h.discovery_endpoint.rsplit(':').next()?.parse().ok()).collect();
    let ui = Path::new(&config.ui.path);

    for _ in 0..count {
        let n = (1..).find(|n| !base.join(format!("hex-{}", n)).exists()).unwrap_or(1);
        let name = format!("hex-{}", n);
        let dir = base.join(&name);
        fs::create_dir_all(&dir)?;
        let api = format!("127.0.0.1:{}", free_port()?);
        let disc_port = discovery_port(&taken)?;
        taken.push(disc_port);
        let discovery = format!("127.0.0.1:{}", disc_port);
        let mut peers = vec![main_discovery.clone()];
        peers.extend(hexes.iter().map(|h| h.discovery_endpoint.clone()));
        let peers = peers.iter().map(|p| toml_string(p)).collect::<Vec<_>>().join(", ");

        let mut main_toml = format!(
            "# Created by `hexdb lattice spawn`: a hex that joins '{lattice}' on this machine.\n\
             [network]\napi_endpoint = {api}\ndiscovery_endpoint = {discovery}\nlattice_name = {lattice_q}\npeers = [{peers}]\n\n\
             [identity]\nrole = {role}\n\n[storage]\npath = \"./data\"\n\n[ui]\npath = {ui}\n\n\
             [security]\nadmin_login = {login}\nadmin_email = {email}\n",
            lattice = lattice,
            api = toml_string(&api),
            discovery = toml_string(&discovery),
            lattice_q = toml_string(&lattice),
            peers = peers,
            role = toml_string(role),
            ui = toml_string(&ui.display().to_string().replace('\\', "/")),
            login = toml_string(&config.security.admin_login),
            email = toml_string(&config.security.admin_email),
        );
        if config.tls.enabled() {
            main_toml.push_str(&format!(
                "\n[tls]\ncert_file = {}\nkey_file = {}\nca_file = {}\n",
                toml_string(&config.tls.cert_file.replace('\\', "/")),
                toml_string(&config.tls.key_file.replace('\\', "/")),
                toml_string(&config.tls.ca_file.replace('\\', "/"))
            ));
        }
        // With a lattice secret each hex can have its own storage key; without
        // one, the lattice key comes from the storage key, so it is shared.
        let local_toml = if config.network.lattice_secret.trim().is_empty() {
            format!("[storage]\nencryption_key = {}\n", toml_string(&config.storage.encryption_key))
        } else {
            format!(
                "[network]\nlattice_secret = {}\n\n[storage]\nencryption_key = {}\n",
                toml_string(&config.network.lattice_secret),
                toml_string(&new_secret())
            )
        };
        fs::write(dir.join("hexdb.toml"), main_toml)?;
        fs::write(dir.join("hexdb.local.toml"), local_toml)?;
        hexdb_core::runtime::write_private_file(&dir.join("hexdb.local.toml"), fs::read(dir.join("hexdb.local.toml"))?.as_slice())?;

        let log = fs::OpenOptions::new().create(true).append(true).open(dir.join("hexdb.log"))?;
        let mut cmd = Command::new(server);
        cmd.arg("--config").arg(dir.join("hexdb.toml")).current_dir(&dir).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        crate::platform::detach(&mut cmd);
        let mut child = cmd.spawn().with_context(|| crate::spawn_hint(server))?;
        let pid = child.id();

        let url = local_base_url_with(&api, config.tls.enabled());
        let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).danger_accept_invalid_certs(config.tls.enabled()).build()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("{} exited during startup ({}). See {}.", name, status, dir.join("hexdb.log").display());
            }
            if client.get(format!("{}/health", url)).send().await.is_ok_and(|r| r.status().is_success()) {
                break;
            }
            if Instant::now() > deadline {
                bail!("{} didn't start within 30 seconds. See {}.", name, dir.join("hexdb.log").display());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        println!("⌬  {} joined '{}' (PID {}): API {}  discovery {}", name, lattice, pid, url, discovery);
        hexes.push(LocalHex { name, dir, api_endpoint: api, discovery_endpoint: discovery, pid });
        save_registry(&base, &hexes)?;
    }
    println!("   It copies the data from the Overseer, then follows it. See them in the admin UI's lattice card, or: hexdb lattice list");
    Ok(())
}

/// The spawned hexes and whether they are running.
pub fn list(config: &HexConfig) -> Result<()> {
    let base = local_dir(config);
    let hexes = load_registry(&base);
    if hexes.is_empty() {
        println!("No local hexes. Start some with: hexdb lattice spawn --count 2");
        return Ok(());
    }
    for hex in hexes {
        let running = RuntimeInfo::read(&hex.dir.join("data")).ok().flatten().is_some_and(|info| crate::platform::process_alive(info.pid));
        println!("{:<8} {:<8} API {:<22} discovery {:<22} {}", hex.name, if running { "running" } else { "stopped" }, hex.api_endpoint, hex.discovery_endpoint, hex.dir.display());
    }
    Ok(())
}

/// Stop every spawned hex (and with `remove`, delete their folders).
pub async fn stop(config: &HexConfig, remove: bool) -> Result<()> {
    let base = local_dir(config);
    let hexes = load_registry(&base);
    if hexes.is_empty() {
        println!("No local hexes.");
        return Ok(());
    }
    let mut failures = Vec::new();
    for hex in &hexes {
        let path = hex.dir.join("hexdb.toml");
        match crate::stop(Some(&path), Duration::from_secs(30), false).await {
            Ok(()) => {}
            Err(e) if e.to_string().contains("No running HexDB") => {}
            Err(e) => failures.push(format!("{}: {:#}", hex.name, e)),
        }
    }
    if !failures.is_empty() {
        bail!("Some hexes didn't stop:\n  {}", failures.join("\n  "));
    }
    if remove {
        fs::remove_dir_all(&base).with_context(|| format!("Failed to delete {}", base.display()))?;
        println!("🧹 Removed {}.", base.display());
    }
    Ok(())
}

/// Parse a role argument.
pub fn role(value: &str) -> Result<String> {
    let role = value.trim().to_ascii_lowercase();
    if matches!(role.as_str(), "auto" | "harvester" | "replicant") {
        Ok(role)
    } else {
        Err(anyhow!("role must be auto, harvester or replicant"))
    }
}
