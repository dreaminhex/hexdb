/// HexDB API Module
/// This module provides the main entry point for the HexDB API server.
/// It initializes the server, sets up the necessary components, and starts
/// the server to listen for incoming requests.
use anyhow::{anyhow, bail};
use axum::Router;
use hexdb_api::{handlers::ShutdownHandle, init::init_security, routes::app_router};
use hexdb_core::{
    config::CONFIG_FILE_NAME, discover_peers, init_logging, load_config_from,
    spawn_compaction_task, spawn_extraction_task, spawn_flush_task, spawn_scrub_task, spawn_ttl_sweep_task,
    elect, parse_preference, spawn_discovery_task, spawn_metrics_task, spawn_vertex_monitoring_task,
    bind_discovery, LatticeMember, NameSource, ROLE_OVERSEER,
    HexDBEngine, HexIdentity, PeerHex, RuntimeInfo,
};
use std::{collections::HashSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

#[cfg(windows)]
mod service;

/// Set when the Windows service manager asks the service to stop.
#[cfg_attr(not(windows), allow(dead_code))]
static SERVICE_STOP: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Names a hex can be given. Embedded so the server doesn't depend on its working directory.
const HEX_NAMES: &str = include_str!("../data/names.txt");

fn main() -> anyhow::Result<()> {
    // A parser sandbox worker: decided before anything else starts (no
    // runtime, no threads, no logging), so its restrictions cover all it does.
    if std::env::args().nth(1).as_deref() == Some(hexdb_core::extract::sandbox::WORKER_ARG) {
        hexdb_core::extract::sandbox::worker_main();
    }
    #[cfg(windows)]
    if std::env::args().any(|a| a == service::SERVICE_ARG) {
        return service::run();
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(server())
}

async fn server() -> anyhow::Result<()> {
    let started_at = chrono::Utc::now().timestamp_millis();
    // Several TLS crypto providers are compiled in; choose one explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config_arg = parse_args()?;

    // Initialize logging
    init_logging("hexdb");

    info!("▶️  HexDB is starting...");

    let mut config = load_config_from(config_arg.as_deref()).unwrap_or_else(|e| {
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

    // The storage key ring (current and previous keys) and the lattice key must be valid.
    let keys = Arc::new(config.key_ring().unwrap_or_else(|e| {
        error!("❌ {:#}", e);
        std::process::exit(1);
    }));
    // Settings changed from the admin UI (saved in the data directory) override the file.
    match hexdb_core::settings::load_overrides(&config.storage_dir(), &keys) {
        Ok(overrides) if !overrides.is_empty() => {
            if let Err(e) = hexdb_core::settings::apply(&mut config, &overrides) {
                error!("❌ The saved settings in {} are invalid: {:#}", hexdb_core::settings::SETTINGS_FILE, e);
                std::process::exit(1);
            }
            info!("⚙️ Applied {} saved setting(s): {}.", overrides.len(), overrides.keys().cloned().collect::<Vec<_>>().join(", "));
        }
        Ok(_) => {}
        Err(e) => {
            error!("❌ Couldn't read the saved settings: {:#}", e);
            std::process::exit(1);
        }
    }
    // The lattice name: configured, saved from an earlier start, or new.
    match hexdb_core::network::discovery::resolve_lattice_name(&config, &keys) {
        Ok((name, generated)) => {
            if generated {
                info!("🌌 No lattice name configured; this hex starts a new lattice, '{}'. Give other hexes network.lattice_name = \"{}\" to join it.", name, name);
            }
            config.network.lattice_name = name;
        }
        Err(e) => {
            error!("❌ {:#}", e);
            std::process::exit(1);
        }
    }
    if let Err(e) = config.lattice_key() {
        error!("❌ {:#}", e);
        std::process::exit(1);
    }
    if let Err(e) = hexdb_api::client::TrustedProxies::parse(&config.network.trusted_proxies) {
        error!("❌ {}", e);
        std::process::exit(1);
    }
    if config.network.lattice_secret.trim().is_empty() {
        info!("🔑 network.lattice_secret is not set; hexes authenticate each other with a key derived from storage.encryption_key.");
    }

    let storage_dir = config.storage_dir();
    // Keep the last panic for the crash report if this process dies.
    let _ = std::fs::create_dir_all(&storage_dir);
    hexdb_core::crashes::install_panic_hook(&storage_dir);

    // Decide who we are before building the engine: take the configured or
    // saved name (or pick one), discover other hexes in our lattice, make sure
    // the name is unique, and elect a role.
    let names: Vec<String> = HEX_NAMES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let (id, mut name, name_source) = match hexdb_core::network::discovery::resolve_hex_identity(&config, &keys, &names) {
        Ok(identity) => identity,
        Err(e) => {
            error!("❌ {:#}", e);
            std::process::exit(1);
        }
    };
    if name_source == NameSource::Generated {
        info!("🐾 No identity.name configured; this hex is now called '{}'. The name is saved and kept on later starts.", name);
    }

    let peers = discover_peers(&config, &id.to_string(), &name).await;

    let peer_names: HashSet<&str> = peers.iter().map(|p| p.name.as_str()).collect();
    if peer_names.contains(name.as_str()) {
        if name_source == NameSource::Configured {
            error!("❌ identity.name '{}' is already used by another hex in lattice '{}'. Give this hex a different name.", name, config.network.lattice_name);
            std::process::exit(1);
        }
        if let Some(free) = names.iter().find(|n| !peer_names.contains(n.as_str())) {
            warn!("🎭 Hex name '{}' is already used by another hex. This hex is now called '{}'.", name, free);
            name = free.clone();
        }
    }

    let preference = parse_preference(&config.identity.role).unwrap_or_else(|e| {
        error!("❌ {}", e);
        std::process::exit(1);
    });
    let provisional = PeerHex {
        id: id.to_string(),
        name: name.clone(),
        role: String::new(),
        lattice: config.network.lattice_name.clone(),
        ip: config.network.discovery_endpoint.clone(),
        api_endpoint: config.network.api_endpoint.clone(),
        ram_mb: config.memory.ram_mb,
        disk_mb: config.storage.disk_mb,
        preference,
        last_seq: 0,
        version: env!("CARGO_PKG_VERSION").to_string(),
        replication_state: String::new(),
        applied_seq: 0,
        tls: config.tls.enabled(),
        config_version: 0,
        config_sha256: String::new(),
        config_state: String::new(),
        signs_responses: true,
        slicing: config.replication.slicing,
        slice_copies: config.replication.slice_copies.max(1),
        slices_held: hexdb_core::slicing::ALL_SLICES,
        replication_source: String::new(),
        machine_id: hexdb_core::updates::machine_id(),
    };
    let role = elect(&provisional, &peers);
    match peers.iter().find(|p| p.role == ROLE_OVERSEER) {
        Some(overseer) if role != ROLE_OVERSEER => info!(
            "🤝 Joining lattice '{}' as {}. Overseer is '{}'.",
            config.network.lattice_name, role, overseer.name
        ),
        _ => info!("🎖️ Starting as {} of lattice '{}'.", role, config.network.lattice_name),
    }

    // Open storage: load SSTable indexes, replay the WAL, and start the WAL writer.
    info!("🛠️  Opening storage at {}...", storage_dir.display());
    let identity = HexIdentity {
        id,
        name: name.clone(),
        hex_type: role.clone(),
    };
    if config.files.sandbox {
        match std::env::current_exe() {
            Ok(exe) => hexdb_core::extract::sandbox::set_worker(exe, config.files.sandbox_memory_mb),
            Err(e) => warn!("⚠️ Files are parsed in-process: the parser sandbox can't find this executable ({}).", e),
        }
    } else {
        warn!("⚠️ files.sandbox is off: files are parsed in the server process.");
    }
    let opening = std::time::Instant::now();
    let engine = match HexDBEngine::open(config.clone(), identity, keys.clone()).await {
        Ok(engine) => Arc::new(engine),
        Err(e) => {
            error!("❌ Failed to open storage: {:#}", e);
            std::process::exit(1);
        }
    };
    info!("✅ Hex '{}' (id: {}) initializing as {}...", engine.name, engine.id, engine.role());
    // A sliced replica's slices, history and position; one starting as the
    // Overseer with only some slices collects the rest before serving data.
    engine.restore_replication_state().await;
    // The election above ran before storage was open, so it couldn't know how
    // far along this hex is or which slices it holds. Run it again with what
    // was restored: a restarted Overseer that holds everything gets its role
    // back instead of handing it to a replica that would have to rebuild.
    {
        let local = hexdb_core::network::discovery::local_identity(&engine).await;
        let role = elect(&local, &peers);
        if role != engine.role() {
            info!("🎖️ With its data loaded, this hex starts as {} (not {}).", role, engine.role());
            engine.set_role(&role);
        }
    }
    // The previous process's runtime file is still here if it didn't shut down cleanly.
    if let Some(report) = hexdb_core::crashes::detect(&storage_dir, opening.elapsed().as_millis() as u64) {
        let cause = match (&report.panic, &report.exit) {
            (Some(panic), _) => format!("it panicked: {} at {}", panic.message, panic.location),
            (None, Some(exit)) => format!("it exited with {}", exit),
            (None, None) => "it was killed or the machine stopped".into(),
        };
        warn!("♻️  Recovered from an unclean shutdown of PID {} ({}); storage opened in {} ms.", report.previous_pid, cause, report.recovery_ms);
        if let Err(e) = hexdb_core::crashes::save(&storage_dir, &report) {
            warn!("⚠️ Couldn't save the crash report: {:#}", e);
        }
        engine.audit("system", "server.recovered", &engine.name, serde_json::to_value(&report).unwrap_or_default()).await;
    }

    // Remember the peers found at startup.
    {
        let now = chrono::Utc::now();
        let mut known = engine.peers.lock().await;
        *known = peers
            .into_iter()
            .map(|hex| LatticeMember { hex, last_seen: now, status: "active".into(), missed: 0 })
            .collect();
    }

    // Create channels for shutdown signals
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    // Answer other hexes' discovery handshakes.
    // Bound here, so a hex that can't be reached stops instead of running without its lattice.
    let discovery = bind_discovery(&config.network.discovery_endpoint).await?;
    let listening = engine.clone();
    hexdb_core::network::discovery::spawn_discovery_lifeline(listening, discovery)?;

    // Initialize security settings & create defaults if not present.
    info!("🔐 Initializing security settings...");
    init_security(engine.clone()).await?;
    // Finish schema migrations an earlier run left unfinished.
    if engine.is_writable() {
        engine.resume_schema_migrations().await;
    }

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
    spawn_extraction_task(engine.clone(), shutdown_rx.clone());
    if config.files.scrub_hours > 0 {
        spawn_scrub_task(engine.clone(), Duration::from_secs(config.files.scrub_hours * 3600), shutdown_rx.clone());
    }
    spawn_vertex_monitoring_task(
        engine.clone(),
        Duration::from_secs(config.memory.vertex_integrity_check_frequency.max(1)),
        shutdown_rx.clone(),
    );

    // Re-probe peers and re-elect roles periodically.
    spawn_discovery_task(
        engine.clone(),
        Duration::from_secs(config.network.discovery_interval_seconds.max(1)),
        shutdown_rx.clone(),
    );

    // Plugins consume the change feed while this hex is the Overseer.
    hexdb_core::spawn_plugins(engine.clone(), shutdown_rx.clone());

    // Stream sources and destinations run on the Overseer too, as do schedules.
    hexdb_core::streams::spawn_streams(engine.clone(), shutdown_rx.clone());
    hexdb_core::functions::spawn_scheduler(engine.clone(), shutdown_rx.clone());
    hexdb_core::triggers::spawn_trigger_runner(engine.clone(), shutdown_rx.clone());

    // Apply new lattice configuration versions as they arrive.
    hexdb_core::lattice_config::spawn_config_task(engine.clone(), shutdown_rx.clone());

    // Follow the Overseer whenever this hex isn't one.
    hexdb_core::spawn_replication_task(engine.clone(), shutdown_rx.clone());

    // Record metrics samples for the dashboard's charts.
    spawn_metrics_task(
        engine.clone(),
        Duration::from_secs(hexdb_core::metrics::HISTORY_INTERVAL_SECONDS),
        shutdown_rx.clone(),
    );

    // Apply `hexdb reset-password` requests left in the storage folder.
    hexdb_core::recovery::spawn_recovery_task(engine.clone(), shutdown_rx.clone());

    // Look for newer HexDB releases (updates.check).
    hexdb_core::updates::spawn_update_check(engine.clone(), config.updates.clone(), shutdown_rx.clone());

    // The runtime file lets the CLI find this process and stop it gracefully.
    let tls = config.tls.enabled();
    let runtime = RuntimeInfo::for_current_process(&config.network.api_endpoint, started_at, tls)?;
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

    let acceptor = if tls {
        Some(hexdb_api::server::tls_acceptor(&config.tls).unwrap_or_else(|e| {
            error!("❌ {:#}", e);
            std::process::exit(1);
        }))
    } else {
        None
    };
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow!("Failed to bind {}: {}", addr, e))?;
    write_runtime(&runtime, &storage_dir)?;
    if !tls && !addr.ip().is_loopback() {
        warn!("⚠️ The API is reachable from the network over plain HTTP. Configure [tls] so passwords and data are encrypted in transit.");
    }
    info!("💽  HexDB API is listening at {}://{}", if tls { "https" } else { "http" }, addr);
    let result = hexdb_api::server::serve(listener, app, acceptor, &config.limits, async move {
        let _ = shutdown_rx.changed().await;
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

fn write_runtime(runtime: &RuntimeInfo, storage_dir: &std::path::Path) -> anyhow::Result<()> {
    let runtime_path = runtime.write(storage_dir)?;
    info!("📝 Runtime file written to {}.", runtime_path.display());
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
            // Running as the Windows service (see service.rs).
            #[cfg(windows)]
            "--service" => {}
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

        let ctrl_c = async {
            if let Err(e) = ctrl_c().await {
                error!("❌ Failed to capture shutdown (Ctrl+C): {}. Shutdown can still occur, but it could result in data loss.", e);
                // Never resolve, so a signal-handler failure doesn't shut the server down.
                std::future::pending::<()>().await;
            } else {
                warn!("🛑 Received Ctrl+C...");
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = SERVICE_STOP.notified() => warn!("🛑 The Windows service manager asked HexDB to stop..."),
        }
    }
}
