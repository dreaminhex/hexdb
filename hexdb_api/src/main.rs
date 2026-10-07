/// HexDB API Module
/// This module provides the main entry point for the HexDB API server.
/// It initializes the server, sets up the necessary components, and starts
/// the server to listen for incoming requests.
use anyhow::{anyhow, bail};
use axum::{serve, Router};
use hexdb_api::{handlers::ShutdownHandle, init::init_security, routes::app_router};
use hexdb_core::{
    config::CONFIG_FILE_NAME, decode_encryption_key, discover_peers, init_logging, load_config_from,
    spawn_compaction_task, spawn_flush_task, spawn_ttl_sweep_task,
    spawn_vertex_monitoring_task, start_discovery_listener, HexConfig,
    HexDBEngine, HexIdentity, PeerHex, RuntimeInfo,
};
use std::{collections::HashSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use ulid::Ulid;

/// Names a hex can be given. Embedded so the server doesn't depend on its working directory.
const HEX_NAMES: &str = include_str!("../data/names.txt");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_arg = parse_args()?;

    // Initialize logging
    init_logging("hexdb");

    info!("▶️  HexDB is starting...");

    let config = load_config_from(config_arg.as_deref()).unwrap_or_else(|e| {
        error!("❌ Failed to load configuration: {}", e);
        std::process::exit(1);
    });

    match &config.source {
        Some(path) => info!("✅ HexDB configuration loaded from {}.", path.display()),
        None => warn!(
            "⚠️ No {} found (checked --config, HEXDB_CONFIG, the working directory, and next to the executable). Using built-in defaults and HEXDB_* environment variables.",
            CONFIG_FILE_NAME
        ),
    }
    if let Some(endpoint) = &config.network.query_endpoint {
        warn!(
            "⚠️ network.query_endpoint ({}) is no longer used; GraphQL is served at /graphql on {}. Remove the setting to silence this warning.",
            endpoint, config.network.api_endpoint
        );
    }
    info!(
        api_endpoint = %config.network.api_endpoint,
        storage = %config.storage.path,
        ui = %config.ui.path,
        "⚙️  Effective configuration"
    );

    // Ensure the WAL encryption key is set and that it can be decoded.
    let key = decode_encryption_key(&config.storage.encryption_key)
        .unwrap_or_else(|e| {
            error!("❌ {}", e);
            std::process::exit(1);
        })
        .to_vec();

    let storage_dir = config.storage_dir();

    // Decide who we are before building the engine: pick a name, discover
    // other hexes in our lattice, make sure the name is unique, and elect a role.
    let names: Vec<String> = HEX_NAMES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let id = Ulid::new();
    let mut name = HexDBEngine::pick_random_name(&names).unwrap_or_else(|| "Unnamed Hex".to_string());

    let peers = discover_peers(&config, &id.to_string(), &name).await;

    let peer_names: HashSet<&str> = peers.iter().map(|p| p.name.as_str()).collect();
    if peer_names.contains(name.as_str()) {
        if let Some(free) = names.iter().find(|n| !peer_names.contains(n.as_str())) {
            warn!("🎭 Hex name '{}' already in use. Reassigning to '{}'.", name, free);
            name = free.clone();
        }
    }

    let role = elect_role(&config, &id.to_string(), &name, &peers);

    // Open storage: load SSTable indexes, replay the WAL, and start the WAL writer.
    info!("🛠️  Opening storage at {}...", storage_dir.display());
    let identity = HexIdentity {
        id,
        name: name.clone(),
        hex_type: role.clone(),
    };
    let engine = match HexDBEngine::open(config.clone(), identity, &key).await {
        Ok(engine) => Arc::new(engine),
        Err(e) => {
            error!("❌ Failed to open storage: {:#}", e);
            std::process::exit(1);
        }
    };
    info!("✅ Hex '{}' (id: {}) initializing as {}...", engine.name, engine.id, engine.hex_type);

    // Set peers after discovering them
    {
        let mut known = engine.peers.lock().await;
        *known = peers.clone();
    }

    let self_hex = PeerHex {
        id: engine.id.to_string(),
        name: name.clone(),
        role: role.clone(),
        lattice: config.network.lattice_name.clone(),
        ip: config.network.discovery_endpoint.clone(),
    };

    // Start the discovery listener.
    tokio::spawn(start_discovery_listener(
        config.network.discovery_endpoint.clone(),
        self_hex,
    ));

    // Initialize security settings & create defaults if not present.
    info!("🔐 Initializing security settings...");
    init_security(engine.clone()).await?;

    // Create channels for shutdown signals
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let shutdown_tx = Arc::new(shutdown_tx);

    // Start the SST compaction task
    info!("🏃‍➡️  Starting the SST compaction task (1 of 4)...");
    spawn_compaction_task(
        engine.clone(),
        Duration::from_secs(config.storage.compaction_frequency.max(1)),
        shutdown_rx.clone(),
    );

    // Spawn a task to flush writes to SSTables
    info!("🏃‍➡️  Starting the flush task (2 of 4)...");
    spawn_flush_task(
        engine.clone(),
        Duration::from_secs(config.storage.wal_flush_check_frequency.max(1)),
        shutdown_rx.clone(),
    );

    // Spawn a task to sweep expired documents (ttl)
    info!("🏃‍➡️  Starting the TTL sweep task (3 of 4)...");
    let ttl_interval = Duration::from_secs(config.memory.ttl_scan_frequency.max(1));
    spawn_ttl_sweep_task(engine.clone(), ttl_interval, shutdown_rx.clone());

    // Spawn a task to monitor vertices
    info!("🏃‍➡️  Starting the vertex monitoring task (4 of 4)...");
    spawn_vertex_monitoring_task(
        engine.clone(),
        Duration::from_secs(config.memory.vertex_integrity_check_frequency.max(1)),
        shutdown_rx.clone(),
    );

    // The runtime file lets the CLI find this process and stop it gracefully.
    let runtime = RuntimeInfo::for_current_process(&config.network.api_endpoint)?;
    let shutdown_handle = ShutdownHandle {
        token: Arc::new(runtime.shutdown_token.clone()),
        trigger: shutdown_tx.clone(),
    };

    // Locate the admin UI.
    let ui_dir = config.ui_dir();
    let ui_dir = if ui_dir.join("index.html").is_file() {
        info!("🖥️  Serving the admin UI from {}.", ui_dir.display());
        Some(ui_dir)
    } else {
        warn!(
            "⚠️ Admin UI not found at {} (build it with `npm run build` in hexdb_admin, or set ui.path). The UI is disabled.",
            ui_dir.display()
        );
        None
    };

    // Start the HTTP server
    info!("🌐 Starting the HTTP server...");
    let schema = hexdb_query::build_schema(engine.clone());
    let app: Router = app_router(engine.clone(), schema, ui_dir, shutdown_handle);
    let addr: SocketAddr = config.network.api_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}.", config.network.api_endpoint);
        std::process::exit(1);
    });

    // Spawn a task to listen for shutdown signals and notify all tasks.
    let signal_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = signal_tx.send(());
    });

    // Bind the server to the specified address and port.
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow!("Failed to bind {}: {}", addr, e))?;

    if RuntimeInfo::path(&storage_dir).exists() {
        warn!("⚠️ Replacing an existing runtime file. A previous HexDB process may not have shut down cleanly.");
    }
    let runtime_path = runtime.write(&storage_dir)?;
    info!("📝 Runtime file written to {}.", runtime_path.display());

    info!("💽  HexDB API is listening at http://{}", addr);
    let result = serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
            info!("🛑 HexDB is shutting down gracefully...");
        })
        .await;

    // In-flight requests have finished; flush everything and stop the WAL writer.
    info!("💾 Flushing to SSTables before exit...");
    let stopped = engine.shutdown().await;
    RuntimeInfo::remove(&storage_dir);
    result?;
    stopped?;
    info!("👋 HexDB stopped.");

    Ok(())
}

/// Parse command-line arguments. Returns the `--config` path, if given.
fn parse_args() -> anyhow::Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    let mut config = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                let path = args.next().ok_or_else(|| anyhow!("--config requires a path"))?;
                config = Some(PathBuf::from(path));
            }
            s if s.starts_with("--config=") => config = Some(PathBuf::from(&s["--config=".len()..])),
            "-h" | "--help" => {
                println!("HexDB server\n\nUsage: hexdb_api [--config <path/to/hexdb.toml>]\n\nWithout --config, the HEXDB_CONFIG environment variable, ./hexdb.toml, and hexdb.toml next to the executable are checked in that order.");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("hexdb_api {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => bail!("Unknown argument: {} (try --help)", other),
        }
    }

    Ok(config)
}

/// Decide this hex's role in the lattice.
/// If no Overseer exists, nodes are ranked by RAM, then disk, then ULID, and the top node becomes Overseer.
fn elect_role(config: &HexConfig, local_id: &str, local_name: &str, peers: &[PeerHex]) -> String {
    // If no overseer exists, determine if we should become one
    let has_overseer = peers.iter().any(|p| p.role == "Overseer");

    if has_overseer {
        info!(
            "🤝 Existing Overseer found. Joining lattice '{}' as a Harvester.",
            config.network.lattice_name
        );
        return "Harvester".to_string();
    }

    let mut all_nodes = peers.to_vec();
    all_nodes.push(PeerHex {
        id: local_id.to_string(),
        name: local_name.to_string(),
        role: "Unassigned".to_string(),
        lattice: config.network.lattice_name.clone(),
        ip: config.network.discovery_endpoint.clone(),
    });

    // Rank by RAM, then disk, then ULID.
    // TODO: peers don't report RAM/disk yet, so fixed values are assumed for them.
    let ram = |p: &PeerHex| if p.id == local_id { config.memory.ram_mb } else { 512 };
    let disk = |p: &PeerHex| if p.id == local_id { config.storage.disk_mb } else { 8192 };
    all_nodes.sort_by(|a, b| {
        ram(b)
            .cmp(&ram(a))
            .then(disk(b).cmp(&disk(a)))
            .then(a.id.cmp(&b.id))
    });

    let elected = &all_nodes[0];
    if elected.id == local_id {
        info!(
            "🎖️ Elected as Overseer for lattice '{}'.",
            config.network.lattice_name
        );
        "Overseer".to_string()
    } else {
        info!(
            "🤝 Joining lattice '{}' as Harvester. Overseer is '{}'.",
            config.network.lattice_name,
            elected.name
        );
        "Harvester".to_string()
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("❌ Failed to capture SIGINT. Shutdown can still occur, but it could result in data loss.");
        let mut sigterm = signal(SignalKind::terminate()).expect("❌ Failed to capture SIGTERM. Shutdown can still occur, but it could result in data loss.");

        tokio::select! {
            _ = sigint.recv() => {
                warn!("🛑 Received SIGINT (Ctrl+C)...");
            }
            _ = sigterm.recv() => {
                warn!("🛑 Received SIGTERM...");
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::ctrl_c;

        if let Err(e) = ctrl_c().await {
            error!("❌ Failed to capture shutdown (Ctrl+C): {}. Shutdown can still occur, but it could result in data loss.", e);
            // Never resolve, so a signal-handler failure doesn't shut the server down.
            std::future::pending::<()>().await;
        } else {
            warn!("🛑 Received Ctrl+C...");
        }
    }
}
