//! HexDB Workbench: named, color-coded environments (each a lattice), opened in
//! their own windows on the lattice's own admin UI, reached directly or through
//! an SSH tunnel; API keys in the OS keychain; schema and index compare.
//!
//! An environment's window shows the lattice's pages, which get no access to
//! the workbench (no IPC capability covers them), and keeps its own cookies and
//! storage, so signing in to one environment never signs in to another.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod compare;
mod envs;
mod tunnel;

use envs::{Environment, Store};
use serde::Serialize;
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder};
use tunnel::{HostKey, Tunnels};

struct App {
    store: Store,
    tunnels: Tunnels,
    data_dir: std::path::PathBuf,
    client: reqwest::Client,
}

type Answer<T> = Result<T, String>;

fn text(e: anyhow::Error) -> String {
    format!("{:#}", e)
}

/// The address the workbench uses for an environment: its own, or the tunnel's local end.
async fn base_url(app: &App, env: &Environment) -> anyhow::Result<String> {
    match &env.ssh {
        None => Ok(env.url.trim_end_matches('/').to_string()),
        Some(ssh) => {
            let remote = reqwest::Url::parse(&env.url)?;
            let port = app.tunnels.open(&env.id, ssh, &remote).await?;
            Ok(format!("http://127.0.0.1:{}", port))
        }
    }
}

#[tauri::command]
fn list_environments(app: State<'_, App>) -> Answer<Vec<Environment>> {
    app.store.list().map_err(text)
}

#[tauri::command]
fn save_environment(app: State<'_, App>, environment: Environment, key: Option<String>) -> Answer<Environment> {
    app.store.save(environment, key.as_deref()).map_err(text)
}

#[tauri::command]
fn delete_environment(app: State<'_, App>, handle: tauri::AppHandle, id: String) -> Answer<()> {
    app.tunnels.close(&id);
    if let Some(window) = handle.get_webview_window(&format!("env-{}", id)) {
        let _ = window.close();
    }
    app.store.delete(&id).map_err(text)
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    version: Option<String>,
    message: String,
}

/// Is the lattice reachable (through its tunnel, if it has one), and which version is it?
#[tauri::command]
async fn check_environment(app: State<'_, App>, id: String) -> Answer<Health> {
    let env = app.store.get(&id).map_err(text)?;
    let base = match base_url(&app, &env).await {
        Ok(b) => b,
        Err(e) => return Ok(Health { ok: false, version: None, message: text(e) }),
    };
    match app.client.get(format!("{}/health", base)).send().await {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let version = body["version"].as_str().map(str::to_string);
            Ok(Health { ok: true, message: format!("Reachable{}.", version.as_ref().map(|v| format!(", HexDB {}", v)).unwrap_or_default()), version })
        }
        Ok(r) => Ok(Health { ok: false, version: None, message: format!("It answered {}.", r.status()) }),
        Err(e) => Ok(Health { ok: false, version: None, message: format!("Not reachable: {}", e) }),
    }
}

const BANNER: &str = r#"
(() => {
  const env = __ENV__;
  const show = () => {
    if (document.getElementById("hexdb-workbench-banner")) return;
    const bar = document.createElement("div");
    bar.id = "hexdb-workbench-banner";
    bar.textContent = env.name + "  •  " + env.url;
    bar.setAttribute("role", "status");
    bar.style.cssText = "position:fixed;top:0;left:0;right:0;z-index:2147483647;height:22px;line-height:22px;padding:0 12px;font:600 12px system-ui,sans-serif;color:#fff;background:" + env.color + ";pointer-events:none;letter-spacing:.02em";
    document.documentElement.appendChild(bar);
    document.documentElement.style.setProperty("box-shadow", "inset 0 0 0 3px " + env.color);
    document.body && (document.body.style.marginTop = "22px");
  };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", show); else show();
})();
"#;

fn css_color(name: &str) -> &'static str {
    match name {
        "green" => "#15803d",
        "blue" => "#1d4ed8",
        "purple" => "#7e22ce",
        "amber" => "#b45309",
        "red" => "#b91c1c",
        _ => "#4b5563",
    }
}

/// Opens (or focuses) the environment's window on the lattice's admin UI.
#[tauri::command]
async fn open_environment(app: State<'_, App>, handle: tauri::AppHandle, id: String) -> Answer<()> {
    let label = format!("env-{}", id);
    if let Some(window) = handle.get_webview_window(&label) {
        return window.set_focus().map_err(|e| e.to_string());
    }
    let env = app.store.get(&id).map_err(text)?;
    let base = base_url(&app, &env).await.map_err(text)?;
    let url = base.parse::<tauri::Url>().map_err(|e| e.to_string())?;
    let banner = BANNER.replace("__ENV__", &serde_json::json!({ "name": env.name, "url": env.url, "color": css_color(&env.color) }).to_string());
    let builder = WebviewWindowBuilder::new(&handle, &label, WebviewUrl::External(url))
        .title(format!("{} - HexDB Workbench", env.name))
        .inner_size(1280.0, 820.0)
        .initialization_script(&banner);
    // Each environment keeps its own cookies and storage (tunnels all use 127.0.0.1, where
    // cookies would otherwise be shared across ports).
    #[cfg(not(target_os = "macos"))]
    let builder = builder.data_directory(app.data_dir.join("webviews").join(&id));
    #[cfg(target_os = "macos")]
    let builder = {
        let mut store = [0u8; 16];
        for (i, b) in id.bytes().enumerate() {
            store[i % 16] ^= b;
        }
        builder.data_store_identifier(store)
    };
    let window = builder.build().map_err(|e| e.to_string())?;
    let tunnels_id = id.clone();
    let handle2 = handle.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            if let Some(app) = handle2.try_state::<App>() {
                app.tunnels.close(&tunnels_id);
            }
        }
    });
    Ok(())
}

/// The SSH host's keys and fingerprints, for the user to check before trusting it.
#[tauri::command]
async fn scan_host(app: State<'_, App>, id: String) -> Answer<Vec<HostKey>> {
    let env = app.store.get(&id).map_err(text)?;
    let ssh = env.ssh.ok_or("This environment has no SSH tunnel.")?;
    app.tunnels.scan(&ssh).await.map_err(text)
}

#[tauri::command]
fn trust_host(app: State<'_, App>, keys: Vec<HostKey>) -> Answer<()> {
    app.tunnels.trust(&keys).map_err(text)
}

/// Compares the tessellations, indexes and schemas of two environments (each needs an API key).
#[tauri::command]
async fn compare_environments(app: State<'_, App>, left: String, right: String) -> Answer<Vec<compare::Difference>> {
    let mut snapshots = Vec::new();
    for id in [&left, &right] {
        let env = app.store.get(id).map_err(text)?;
        let key = app.store.key(id).map_err(text)?.ok_or_else(|| format!("{} has no API key in the keychain; add one (read access is enough).", env.name))?;
        let base = base_url(&app, &env).await.map_err(text)?;
        snapshots.push(compare::fetch(&app.client, &base, &key).await.map_err(|e| format!("{}: {:#}", env.name, e))?);
    }
    Ok(compare::compare(&snapshots[0], &snapshots[1]))
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
            app.manage(App { store: Store::new(&dir), tunnels: Tunnels::new(&dir), data_dir, client });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_environments,
            save_environment,
            delete_environment,
            check_environment,
            open_environment,
            scan_host,
            trust_host,
            compare_environments
        ])
        .run(tauri::generate_context!())
        .expect("error while running HexDB Workbench");
}
