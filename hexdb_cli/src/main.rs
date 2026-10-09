use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use hexdb_core::{
    load_config_from, runtime::local_base_url_with, user_config_dir, HexConfig, RuntimeInfo, CONFIG_FILE_NAME, LOCAL_CONFIG_FILE_NAME,
    SHUTDOWN_TOKEN_HEADER,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

mod lattice;

#[derive(Parser)]
#[command(name = "hexdb", about = " ⌬  HexDB CLI", version)]
struct Cli {
    /// Path to hexdb.toml. Defaults to $HEXDB_CONFIG, ./hexdb.toml, or hexdb.toml next to the executable.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// API key (or session token) for commands that need credentials, such as
    /// `status`. Create one on the admin UI's Account page. Prefer the
    /// HEXDB_TOKEN environment variable to keep it out of shell history.
    #[arg(long, global = true, env = "HEXDB_TOKEN", hide_env_values = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a private configuration with a new encryption key (hexdb start does this on first use)
    Init {
        /// Folder for hexdb.toml, hexdb.local.toml and the data. Defaults to the
        /// user's HexDB folder (%LOCALAPPDATA%\HexDB, ~/Library/Application
        /// Support/HexDB or ~/.config/hexdb), or the folder of --config.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Start the HexDB server
    Start {
        /// Run in the background. Output goes to hexdb.log in the data directory.
        #[arg(short, long)]
        silent: bool,

        /// Path to the hexdb_api server binary. Defaults to the one next to this CLI, then PATH.
        #[arg(long, env = "HEXDB_SERVER_BIN")]
        server_bin: Option<PathBuf>,
    },
    /// Stop a running HexDB server gracefully
    Stop {
        /// Seconds to wait for the server to stop
        #[arg(long, default_value_t = 30)]
        timeout: u64,

        /// Kill the server if it can't be stopped gracefully. Unflushed writes may be lost.
        #[arg(long)]
        force: bool,
    },
    /// Check server health
    Health {
        /// Server address, e.g. 127.0.0.1:7700. Defaults to network.api_endpoint from the config.
        #[arg(short, long)]
        url: Option<String>,
    },
    /// Show server status and metrics
    Status {
        /// Server address, e.g. 127.0.0.1:7700. Defaults to network.api_endpoint from the config.
        #[arg(short, long)]
        url: Option<String>,
    },
    /// Manage plugins
    Plugins {
        #[command(subcommand)]
        sub: PluginCommand,
    },
    /// Run more hexes on this machine that join this lattice
    Lattice {
        #[command(subcommand)]
        sub: LatticeCommand,
    },
    /// Print a new random key for storage.encryption_key or network.lattice_secret
    Secret,
    /// Run a SQL SELECT and print the rows as a table (needs a token with read access)
    Sql {
        /// The statement, e.g. "SELECT customer, COUNT(*) FROM orders GROUP BY customer"
        statement: String,
        /// A value for the next ? placeholder: JSON (42, true, "text") or plain text
        #[arg(short, long = "param")]
        params: Vec<String>,
        /// Print the server's JSON answer (every page) instead of a table
        #[arg(long)]
        json: bool,
        /// Server address, e.g. 127.0.0.1:7700. Defaults to network.api_endpoint from the config.
        #[arg(short, long)]
        url: Option<String>,
    },
    /// Back up the running server's data (needs a token with the maintenance permission)
    Backup {
        /// Folder name for the backup (default: date, time and sequence number)
        #[arg(long)]
        name: Option<String>,
        /// List existing backups instead of making one
        #[arg(long)]
        list: bool,
        /// Server address, e.g. 127.0.0.1:7700. Defaults to network.api_endpoint from the config.
        #[arg(short, long)]
        url: Option<String>,
    },
}

#[derive(Subcommand)]
enum LatticeCommand {
    /// Start hexes that join the lattice of the hex this config describes
    Spawn {
        /// How many hexes to start
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Their role: harvester (read replica that can be elected), replicant (never elected), or auto
        #[arg(long, default_value = "harvester", value_parser = lattice::role)]
        role: String,
        /// Path to the hexdb_api server binary. Defaults to the one next to this CLI, then PATH.
        #[arg(long, env = "HEXDB_SERVER_BIN")]
        server_bin: Option<PathBuf>,
    },
    /// List the hexes started with `spawn`
    List,
    /// Stop the hexes started with `spawn`
    Stop {
        /// Also delete their folders and data
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Register a plugin from the plugins directory
    Add {
        id: String,
    },
    /// Unregister a plugin
    Remove {
        id: String,
    },
    /// List registered plugins
    List,
}

#[derive(Serialize, Deserialize)]
struct PluginEntry {
    path: String,
    #[serde(rename = "type", alias = "plugin_type")]
    plugin_type: String,
}

const REGISTRY_PATH: &str = "plugins.json";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.as_deref();
    let token = cli.token.as_deref();

    match cli.command {
        Commands::Init { dir } => {
            let target = init_target(config_path, dir)?;
            let created = init(&target)?;
            print_init(&target, &created);
            Ok(())
        }
        Commands::Start { silent, server_bin } => start(config_path, silent, server_bin).await,
        Commands::Stop { timeout, force } => stop(config_path, Duration::from_secs(timeout), force).await,
        Commands::Health { url } => {
            let (base, config) = base_url(config_path, url)?;
            println!("{}", pretty(&get_text(&config, &base, "/health", token).await?));
            Ok(())
        }
        Commands::Status { url } => {
            let (base, config) = base_url(config_path, url)?;
            if token.is_none() {
                bail!("`hexdb status` needs credentials: set HEXDB_TOKEN (or pass --token) to an admin's API key.");
            }
            println!("{}", pretty(&get_text(&config, &base, "/status", token).await?));
            Ok(())
        }
        Commands::Plugins { sub } => match sub {
            PluginCommand::Add { id } => add_plugin(&id),
            PluginCommand::Remove { id } => remove_plugin(&id),
            PluginCommand::List => list_plugins(),
        },
        Commands::Lattice { sub } => {
            let config = load(config_path)?;
            match sub {
                LatticeCommand::Spawn { count, role, server_bin } => lattice::spawn(&config, count.max(1), &role, &resolve_server_bin(server_bin)).await,
                LatticeCommand::List => lattice::list(&config),
                LatticeCommand::Stop { remove } => lattice::stop(&config, remove).await,
            }
        }
        Commands::Secret => {
            println!("{}", lattice::new_secret());
            Ok(())
        }
        Commands::Sql { statement, params, json, url } => {
            let (base, config) = base_url(config_path, url)?;
            if token.is_none() {
                bail!("`hexdb sql` needs credentials: set HEXDB_TOKEN (or pass --token) to an API key with read access.");
            }
            let params: Vec<serde_json::Value> =
                params.iter().map(|p| serde_json::from_str(p).unwrap_or_else(|_| serde_json::Value::String(p.clone()))).collect();
            let mut cursor: Option<String> = None;
            let mut columns: Vec<String> = Vec::new();
            let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
            loop {
                let body = serde_json::json!({ "sql": statement, "params": params, "cursor": cursor });
                let text = request_text(&config, &base, reqwest::Method::POST, "/sql", token, Some(&body)).await?;
                let page: serde_json::Value = serde_json::from_str(&text)?;
                if json {
                    println!("{}", pretty(&text));
                } else {
                    if columns.is_empty() {
                        columns = page["columns"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(String::from)).collect();
                    }
                    rows.extend(page["rows"].as_array().into_iter().flatten().filter_map(|r| r.as_array().cloned()));
                }
                match page["next"].as_str() {
                    Some(next) => cursor = Some(next.to_string()),
                    None => break,
                }
            }
            if !json {
                print_table(&columns, &rows);
            }
            Ok(())
        }
        Commands::Backup { name, list, url } => {
            let (base, config) = base_url(config_path, url)?;
            if token.is_none() {
                bail!("`hexdb backup` needs credentials: set HEXDB_TOKEN (or pass --token) to an API key with the maintenance permission.");
            }
            if list {
                println!("{}", pretty(&get_text(&config, &base, "/backups", token).await?));
                return Ok(());
            }
            let body = serde_json::json!({ "name": name });
            let text = request_text(&config, &base, reqwest::Method::POST, "/backup", token, Some(&body)).await?;
            let info: serde_json::Value = serde_json::from_str(&text)?;
            println!(
                "Backed up {} file(s), {} bytes, up to sequence {} to {}",
                info["files"], info["bytes"], info["sequence"], info["path"].as_str().unwrap_or_default()
            );
            println!("To restore: stop the server, point storage.path at that folder (same encryption keys), and start it.");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

async fn start(config_path: Option<&Path>, silent: bool, server_bin: Option<PathBuf>) -> Result<()> {
    let mut config = load(config_path)?;
    // First run of an installed HexDB: no config anywhere and no key in the
    // environment. Create the user's own configuration rather than fail.
    if config.source.is_none() && config.storage.encryption_key.is_empty() {
        if let Some(dir) = user_config_dir() {
            let target = dir.join(CONFIG_FILE_NAME);
            let created = init(&target)?;
            print_init(&target, &created);
            println!();
            config = load(Some(&target))?;
        }
    }
    let storage_dir = config.storage_dir();
    let base = local_base_url_with(&config.network.api_endpoint, config.tls.enabled());

    if let Some(info) = RuntimeInfo::read(&storage_dir)? {
        if is_healthy(&config, &base).await {
            bail!("HexDB is already running (PID {}) at {}.", info.pid, base);
        }
    }

    let server = resolve_server_bin(server_bin);
    let mut cmd = Command::new(&server);
    match &config.source {
        Some(source) => {
            cmd.arg("--config").arg(source);
        }
        None => eprintln!("⚠️ No hexdb.toml found. The server will use built-in defaults and HEXDB_* environment variables."),
    }

    if !silent {
        // Foreground: the server shares this console and handles Ctrl+C itself.
        // Ignore Ctrl+C here so the CLI waits for the server's graceful shutdown.
        let mut child = tokio::process::Command::from(cmd)
            .spawn()
            .with_context(|| spawn_hint(&server))?;
        tokio::spawn(async {
            while tokio::signal::ctrl_c().await.is_ok() {}
        });

        let status = child.wait().await?;
        if !status.success() {
            bail!("HexDB exited with {}.", status);
        }
        return Ok(());
    }

    // Background: detach from this console and log to a file.
    fs::create_dir_all(&storage_dir)
        .with_context(|| format!("Failed to create {}", storage_dir.display()))?;
    let log_path = storage_dir.join("hexdb.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open {}", log_path.display()))?;
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    platform::detach(&mut cmd);

    let mut child = cmd.spawn().with_context(|| spawn_hint(&server))?;
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("HexDB exited during startup ({}). See {}.", status, log_path.display());
        }
        if is_healthy(&config, &base).await {
            println!("⌬  HexDB started in the background (PID {}) at {}.", pid, base);
            println!("   Admin UI: {}/ui/", base);
            println!("   Logs: {}", log_path.display());
            let first_password = storage_dir.join("initial-admin-password.txt");
            if first_password.is_file() {
                println!(
                    "   First sign-in: {} with the password in {} (change it, then delete the file).",
                    config.security.admin_login,
                    first_password.display()
                );
            }
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "HexDB (PID {}) did not become healthy within 30 seconds. See {}.",
                pid,
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn stop(config_path: Option<&Path>, timeout: Duration, force: bool) -> Result<()> {
    let config = load(config_path)?;
    let storage_dir = config.storage_dir();

    let Some(info) = RuntimeInfo::read(&storage_dir)? else {
        bail!(
            "No running HexDB found (no runtime file at {}).",
            RuntimeInfo::path(&storage_dir).display()
        );
    };

    if !platform::process_alive(info.pid) || !is_same_server(&info) {
        RuntimeInfo::remove(&storage_dir);
        println!("HexDB (PID {}) is not running. Removed the stale runtime file.", info.pid);
        return Ok(());
    }

    let base = local_base_url_with(&info.api_endpoint, info.tls);
    match request_shutdown(&config, &base, &info.shutdown_token).await {
        Ok(()) => {
            println!("🛑 Shutdown requested. Waiting for HexDB (PID {}) to stop...", info.pid);
            if wait_for_exit(info.pid, timeout).await {
                RuntimeInfo::remove(&storage_dir);
                println!("✅ HexDB stopped.");
                return Ok(());
            }
            if !force {
                bail!(
                    "HexDB (PID {}) did not stop within {} seconds. Re-run with --force to kill it.",
                    info.pid,
                    timeout.as_secs()
                );
            }
        }
        Err(e) => {
            if !force {
                bail!(
                    "Could not ask HexDB to shut down at {}: {}. If PID {} is HexDB, re-run with --force to kill it.",
                    base,
                    e,
                    info.pid
                );
            }
            eprintln!("⚠️ Graceful shutdown failed: {}", e);
        }
    }

    // Kill only a process we can positively identify as this server: PIDs are
    // reused, and the runtime file could be stale.
    if !is_same_server(&info) {
        bail!(
            "Refusing to kill PID {}: it can't be confirmed to be the HexDB server that wrote {} (different executable or start time). Stop it by hand if it is.",
            info.pid,
            RuntimeInfo::path(&storage_dir).display()
        );
    }
    platform::force_kill(info.pid)?;
    wait_for_exit(info.pid, Duration::from_secs(5)).await;
    RuntimeInfo::remove(&storage_dir);
    println!("⚠️ HexDB (PID {}) was forcefully terminated. Unflushed writes may be lost.", info.pid);
    Ok(())
}

/// True if `info.pid` is still the process that wrote the runtime file:
/// same executable, started within a few seconds of the recorded time.
fn is_same_server(info: &RuntimeInfo) -> bool {
    if info.exe.is_empty() || info.started_at == 0 {
        // A runtime file from an older HexDB: identity can't be checked.
        return false;
    }
    match platform::process_identity(info.pid) {
        Some((exe, started_ms)) => {
            let same_exe = if cfg!(windows) { exe.eq_ignore_ascii_case(&info.exe) } else { exe == info.exe };
            // The server records its start time early in main(); allow for process start-up.
            let close_in_time = started_ms.is_none_or(|ms| (info.started_at - ms).abs() <= 10_000);
            same_exe && close_in_time
        }
        None => false,
    }
}

async fn request_shutdown(config: &HexConfig, base: &str, token: &str) -> Result<()> {
    let res = http_client(config)?
        .post(format!("{}/shutdown", base))
        .header(SHUTDOWN_TOKEN_HEADER, token)
        .send()
        .await?;

    match res.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED => Err(anyhow!(
            "the server rejected the shutdown token (the runtime file may belong to a different server)"
        )),
        s => Err(anyhow!("unexpected response {}", s)),
    }
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !platform::process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    !platform::process_alive(pid)
}

/// The server binary: an explicit path, else `hexdb_api` next to this CLI, else `hexdb_api` on PATH.
fn resolve_server_bin(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }

    let file = format!("hexdb_api{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(file)
}

// ---------------------------------------------------------------------------
// hexdb init
// ---------------------------------------------------------------------------

/// The config file `init` writes: `--dir`, else the `--config` path, else the user's HexDB folder.
fn init_target(config_path: Option<&Path>, dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = dir {
        return Ok(dir.join(CONFIG_FILE_NAME));
    }
    if let Some(path) = config_path {
        return Ok(path.to_path_buf());
    }
    user_config_dir()
        .map(|d| d.join(CONFIG_FILE_NAME))
        .ok_or_else(|| anyhow!("No home folder is known. Pass --dir <folder> or set HEXDB_HOME."))
}

/// Writes `config_file` and `hexdb.local.toml` beside it, each only if it is
/// missing, so running it twice never replaces a key. Returns what it created.
fn init(config_file: &Path) -> Result<Vec<PathBuf>> {
    let dir = config_file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.exists() {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        platform::restrict_dir(dir);
    }
    let mut created = Vec::new();
    if !config_file.exists() {
        fs::write(config_file, config_template(installed_ui_dir().as_deref()))
            .with_context(|| format!("Failed to write {}", config_file.display()))?;
        created.push(config_file.to_path_buf());
    }
    let local = dir.join(LOCAL_CONFIG_FILE_NAME);
    if !local.exists() {
        let text = format!(
            "# Secrets for hexdb.toml. Keep this file private and back it up: without the\n# encryption key the data can't be read.\n\n[storage]\nencryption_key = \"{}\"\n",
            lattice::new_secret()
        );
        write_private(&local, &text).with_context(|| format!("Failed to write {}", local.display()))?;
        created.push(local);
    }
    Ok(created)
}

fn print_init(config_file: &Path, created: &[PathBuf]) {
    let dir = config_file.parent().unwrap_or(Path::new("."));
    if created.is_empty() {
        println!("⌬  HexDB is already set up in {}. Nothing changed.", dir.display());
        return;
    }
    println!("⌬  Created your HexDB configuration in {}", dir.display());
    for path in created {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name == LOCAL_CONFIG_FILE_NAME {
            println!("   {}   your encryption key. Back it up: without it the data can't be read.", name);
        } else {
            println!("   {}         settings; the data goes in the data folder beside it", name);
        }
    }
    println!("   Start the server with `hexdb start`.");
}

/// The admin UI shipped with this copy of HexDB: next to the executable
/// (archives, MSI, macOS package) or in ../share/hexdb/ui (Debian, Homebrew,
/// the install script).
fn installed_ui_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?;
    [dir.join("ui"), dir.join("..").join("share").join("hexdb").join("ui")]
        .into_iter()
        .find(|p| p.join("index.html").is_file())
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .map(|p| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\")))
}

fn config_template(ui: Option<&Path>) -> String {
    let ui_line = match ui {
        Some(path) => format!("path = {}", toml::Value::String(path.to_string_lossy().into_owned())),
        None => "# path = \"/path/to/hexdb_admin/dist\"   # the built admin UI; not found next to hexdb".to_string(),
    };
    format!(
        "# HexDB configuration, created by `hexdb init`. Every setting is described in\n\
         # MANUAL.md. Secrets go in hexdb.local.toml beside this file.\n\
         \n\
         [network]\n\
         api_endpoint = \"127.0.0.1:7700\"\n\
         discovery_endpoint = \"127.0.0.1:7702\"\n\
         \n\
         [storage]\n\
         path = \"./data\"\n\
         \n\
         [ui]\n\
         {}\n",
        ui_line
    )
}

/// Writes a file only its owner can read (0600 on Unix; on Windows the user's
/// profile folders are already private).
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

fn spawn_hint(server: &Path) -> String {
    format!(
        "Failed to start {}. Build it with `cargo build -p hexdb_api`, install it with `cargo install --path hexdb_api`, or pass --server-bin.",
        server.display()
    )
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn load(config_path: Option<&Path>) -> Result<HexConfig> {
    load_config_from(config_path).map_err(|e| anyhow!("Failed to load configuration: {}", e))
}

/// The server's base URL (`https://` when the config enables TLS) and the config.
fn base_url(config_path: Option<&Path>, url: Option<String>) -> Result<(String, HexConfig)> {
    let config = load(config_path).unwrap_or_default();
    let endpoint = url.unwrap_or_else(|| config.network.api_endpoint.clone());
    Ok((local_base_url_with(&endpoint, config.tls.enabled()), config))
}

/// An HTTP client that also trusts `tls.ca_file` (for private certificates).
fn http_client(config: &HexConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(5));
    if !config.tls.ca_file.is_empty() {
        let pem = fs::read(&config.tls.ca_file).with_context(|| format!("Failed to read tls.ca_file {}", config.tls.ca_file))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem)? {
            builder = builder.add_root_certificate(cert);
        }
    }
    Ok(builder.build()?)
}

async fn get_text(config: &HexConfig, base: &str, path: &str, token: Option<&str>) -> Result<String> {
    request_text(config, base, reqwest::Method::GET, path, token, None).await
}

async fn request_text(
    config: &HexConfig,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<String> {
    let mut request = http_client(config)?.request(method, format!("{}{}", base, path));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let res = request
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                anyhow!("HexDB is not reachable at {}.", base)
            } else {
                anyhow!(e)
            }
        })?;

    let status = res.status();
    let body = res.text().await?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("{}{} needs valid credentials: set HEXDB_TOKEN (or --token) to an API key.", base, path);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        bail!("{}{} isn't allowed with this API key (check its user's roles).", base, path);
    }
    if !status.is_success() {
        bail!("{}{} returned {}: {}", base, path, status, body);
    }
    Ok(body)
}

async fn is_healthy(config: &HexConfig, base: &str) -> bool {
    get_text(config, base, "/health", None).await.is_ok()
}

/// Rows as an aligned text table; long values are cut at 60 characters.
fn print_table(columns: &[String], rows: &[Vec<serde_json::Value>]) {
    let cell = |v: &serde_json::Value| -> String {
        let text = match v {
            serde_json::Value::Null => "NULL".to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let text = text.replace(['\n', '\r', '\t'], " ");
        if text.chars().count() > 60 {
            format!("{}...", text.chars().take(57).collect::<String>())
        } else {
            text
        }
    };
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(cell).collect()).collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |values: &[String]| {
        let padded: Vec<String> = values.iter().zip(&widths).map(|(v, w)| format!("{:<width$}", v, width = *w)).collect();
        println!("{}", padded.join(" | ").trim_end());
    };
    line(columns);
    println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-"));
    for row in &cells {
        line(row);
    }
    println!("({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
}

fn pretty(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| body.to_string())
}

// ---------------------------------------------------------------------------
// Platform-specific process handling
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use anyhow::Result;
    use nix::{
        errno::Errno,
        sys::signal::{kill, Signal},
        unistd::Pid,
    };
    use std::process::Command;

    /// Put the server in its own process group so terminal signals don't reach it.
    pub fn detach(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    /// Make a folder `hexdb init` created private to its owner (0700).
    pub fn restrict_dir(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    pub fn process_alive(pid: u32) -> bool {
        match kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => true,
            Err(Errno::EPERM) => true, // exists, owned by someone else
            Err(_) => false,
        }
    }

    pub fn force_kill(pid: u32) -> Result<()> {
        kill(Pid::from_raw(pid as i32), Signal::SIGKILL)?;
        Ok(())
    }

    /// The executable path and start time in epoch ms of a process.
    #[cfg(target_os = "linux")]
    pub fn process_identity(pid: u32) -> Option<(String, Option<i64>)> {
        {
            let exe = std::fs::read_link(format!("/proc/{}/exe", pid)).ok()?.display().to_string();
            // Field 22 of /proc/<pid>/stat is the start time in clock ticks after boot.
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
            let after_name = stat.rsplit_once(')')?.1;
            let ticks: i64 = after_name.split_whitespace().nth(19)?.parse().ok()?;
            let boot: i64 = std::fs::read_to_string("/proc/stat")
                .ok()?
                .lines()
                .find_map(|l| l.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            // Linux reports USER_HZ, which is 100 on every mainstream build.
            Some((exe, Some(boot * 1000 + ticks * 10)))
        }
    }

    /// The executable path of a process (no start time outside Linux).
    #[cfg(not(target_os = "linux"))]
    pub fn process_identity(pid: u32) -> Option<(String, Option<i64>)> {
        {
            let output = Command::new("ps").args(["-o", "comm=", "-p", &pid.to_string()]).output().ok()?;
            let exe = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!exe.is_empty()).then_some((exe, None))
        }
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{bail, Result};
    use std::process::Command;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    /// Nothing to do: the default folder is under %LOCALAPPDATA%, which only
    /// the user (plus SYSTEM and administrators) can open.
    pub fn restrict_dir(_dir: &std::path::Path) {}

    /// The executable path and start time (epoch ms) of a process.
    pub fn process_identity(pid: u32) -> Option<(String, Option<i64>)> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut buffer = [0u16; 1024];
            let mut len = buffer.len() as u32;
            let named = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            let timed = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
            CloseHandle(handle);
            if named == 0 {
                return None;
            }
            let exe = String::from_utf16_lossy(&buffer[..len as usize]);
            // FILETIME counts 100 ns intervals since 1601-01-01.
            let started = (timed != 0).then(|| {
                let ticks = ((created.dwHighDateTime as i64) << 32) | created.dwLowDateTime as i64;
                ticks / 10_000 - 11_644_473_600_000
            });
            Some((exe, started))
        }
    }

    /// Run the server without a console window, in its own process group so Ctrl+C here doesn't reach it.
    pub fn detach(cmd: &mut Command) {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};

        // Windows children inherit every inheritable handle, not just their own
        // stdio. If this CLI's output is a pipe (e.g. `hexdb start -s | more` or a
        // CI log), the server would hold that pipe open and whoever reads it would
        // wait until the server exits. The server's stdio is redirected to the log
        // file explicitly, so stop our own handles from being inherited.
        unsafe {
            for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = GetStdHandle(std_handle);
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    pub fn process_alive(pid: u32) -> bool {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // Exists but owned by someone else.
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }

    pub fn force_kill(pid: u32) -> Result<()> {
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                bail!("Could not open process {} to terminate it.", pid);
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            if ok == 0 {
                bail!("Failed to terminate process {}.", pid);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

fn resolve_registry_path() -> PathBuf {
    shellexpand::tilde(REGISTRY_PATH).to_string().into()
}

fn load_registry() -> Result<std::collections::HashMap<String, PluginEntry>> {
    let path = resolve_registry_path();
    if !path.exists() {
        return Ok(Default::default());
    }
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

fn save_registry(registry: &std::collections::HashMap<String, PluginEntry>) -> Result<()> {
    let path = resolve_registry_path();
    let json = serde_json::to_string_pretty(registry)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)?;
    Ok(())
}

fn add_plugin(id: &str) -> Result<()> {
    let plugin_dir = format!("plugins/{}", id.replace('@', ""));
    let toml_path = format!("{}/plugin.toml", plugin_dir);

    let content = fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path))?;
    let parsed: toml::Value = toml::from_str(&content)?;

    let plugin_type = parsed
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut registry = load_registry()?;
    registry.insert(
        id.to_string(),
        PluginEntry {
            path: plugin_dir.clone(),
            plugin_type,
        },
    );
    save_registry(&registry)?;
    println!("📦 Plugin {} added from {}", id, plugin_dir);
    Ok(())
}

fn remove_plugin(id: &str) -> Result<()> {
    let mut registry = load_registry()?;
    if registry.remove(id).is_some() {
        save_registry(&registry)?;
        println!("🗑️ Removed plugin {}", id);
    } else {
        println!("❗ Plugin {} not found", id);
    }
    Ok(())
}

fn list_plugins() -> Result<()> {
    let registry = load_registry()?;
    if registry.is_empty() {
        println!("(no plugins installed)");
    } else {
        println!("📦 Installed Plugins:");
        for (id, entry) in registry {
            println!("  {} [{}] @ {}", id, entry.plugin_type, entry.path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hexdb-cli-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn init_creates_a_loadable_config_and_never_replaces_the_key() {
        let dir = temp_dir("init");
        let file = dir.join(CONFIG_FILE_NAME);

        let created = init(&file).unwrap();
        assert_eq!(created.len(), 2);
        let key = load_config_from(Some(&file)).unwrap().storage.encryption_key;
        assert!(key.starts_with("base64:"));

        // A second run leaves both files alone.
        assert!(init(&file).unwrap().is_empty());
        let config = load_config_from(Some(&file)).unwrap();
        assert_eq!(config.storage.encryption_key, key);
        assert_eq!(PathBuf::from(&config.storage.path), std::path::absolute(dir.join("data")).unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(LOCAL_CONFIG_FILE_NAME)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_escapes_windows_paths() {
        let text = config_template(Some(Path::new(r"C:\Program Files\HexDB\ui")));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["ui"]["path"].as_str(), Some(r"C:\Program Files\HexDB\ui"));
        assert!(toml::from_str::<toml::Value>(&config_template(None)).is_ok());
    }
}
