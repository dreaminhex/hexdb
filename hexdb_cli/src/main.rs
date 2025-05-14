use clap::{Parser, Subcommand};
use std::{fs, process::Command, path::PathBuf};
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;
use hexdb_core::load_config;
use std::os::windows::process::CommandExt;
use tracing::{info, warn, error};

#[derive(Parser)]
#[command(name = "hexdb", about = " ⌬  HexDB CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // Start the HexDB node
    Start {
        // Run in background
        #[arg(short, long)]
        silent: bool,
    },
    // Stop the HexDB node
    Stop,
    // Check node health
    Health {
        #[arg(short, long, default_value = "127.0.0.1:7700")]
        url: String,
    },
    // Check node status
    Status {
        #[arg(short, long, default_value = "127.0.0.1:7700")]
        url: String,
    },
    // Manage plugins
    Plugins {
        #[command(subcommand)]
        sub: PluginCommand,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    Add {
        id: String,
    },
    Remove {
        id: String,
    },
    List,
}

#[derive(Serialize, Deserialize)]
struct PluginEntry {
    path: String,
    plugin_type: String,
}

const REGISTRY_PATH: &str = "plugins.json";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start { silent } => {
            if silent {
                let mut cmd = Command::new("cargo");
                let child = cmd.args(&["run", "-p", "hexdb_api"])
                    .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                    .spawn()?;

                // Get the PID from the child process handle
                let pid = child.id();
                std::fs::write(".hexdb.pid", pid.to_string())?;
                info!("⌬  HexDB started in background (PID {}).", pid);
                return Ok(()); // Return immediately to give control back
            } else {
                let mut child = TokioCommand::new("cargo")
                    .args(&["run", "-p", "hexdb_api"])
                    .spawn()?;
                child.wait().await?;
            }
        }

        Commands::Stop => {
            let pid_str = std::fs::read_to_string(".hexdb.pid")
                .expect("❌ Failed to read .hexdb.pid");
            let pid: u32 = pid_str.trim().parse().expect("Invalid PID");

            #[cfg(target_family = "unix")]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;

                kill(Pid::from_raw(pid as i32), Signal::SIGTERM)?;
            }

            #[cfg(target_family = "windows")]
            {
                use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE, PROCESS_QUERY_LIMITED_INFORMATION};
                use windows_sys::Win32::Foundation::CloseHandle;

                unsafe {
                    // First try with PROCESS_TERMINATE
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if handle != std::ptr::null_mut() {
                        TerminateProcess(handle, 0);
                        CloseHandle(handle);
                        info!("✅ Successfully terminated process {}", pid);
                        return Ok(());
                    }

                    // If we can't terminate, try to get process info to verify it exists
                    let info_handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if info_handle != std::ptr::null_mut() {
                        error!("❗ Process {} exists but couldn't be terminated.", pid);
                        CloseHandle(info_handle);
                    } else {
                        error!("❗ Process {} not found", pid);
                    }
                }
            }

            std::fs::remove_file(".hexdb.pid").ok();
            warn!("🛑 HexDB stopped.");
        }

        Commands::Health { .. } => {
            let config = load_config().expect("❌ Failed to load config.");
            let url = format!("http://{}/health", config.network.api_endpoint);
            let res = reqwest::get(&url).await?;
            let body = res.text().await?;
            info!("Health: {}", body);
        }

        Commands::Status { .. } => {
            let config = load_config().expect("❌ Failed to load config.");
            let url = format!("http://{}/status", config.network.api_endpoint);
            let res = reqwest::get(&url).await?;
            let body = res.text().await?;
            info!("Status: {}", body);
        }

        Commands::Plugins { sub } => match sub {
            PluginCommand::Add { id } => {
                add_plugin(&id)?;
            }
            PluginCommand::Remove { id } => {
                remove_plugin(&id)?;
            }
            PluginCommand::List => {
                list_plugins()?;
            }
        },
    }

    Ok(())
}

fn resolve_registry_path() -> PathBuf {
    shellexpand::tilde(REGISTRY_PATH).to_string().into()
}

fn load_registry() -> anyhow::Result<std::collections::HashMap<String, PluginEntry>> {
    let path = resolve_registry_path();
    if !path.exists() {
        return Ok(Default::default());
    }
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

fn save_registry(registry: &std::collections::HashMap<String, PluginEntry>) -> anyhow::Result<()> {
    let path = resolve_registry_path();
    let json = serde_json::to_string_pretty(registry)?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, json)?;
    Ok(())
}

fn add_plugin(id: &str) -> anyhow::Result<()> {
    let plugin_dir = format!("plugins/{}", id.replace('@', ""));
    let toml_path = format!("{}/plugin.toml", plugin_dir);

    let content = fs::read_to_string(&toml_path)?;
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
    info!("📦 Plugin {} added from {}", id, plugin_dir);
    Ok(())
}

fn remove_plugin(id: &str) -> anyhow::Result<()> {
    let mut registry = load_registry()?;
    if registry.remove(id).is_some() {
        save_registry(&registry)?;
        info!("🗑️ Removed plugin {}", id);
    } else {
        info!("❗ Plugin {} not found", id);
    }
    Ok(())
}

fn list_plugins() -> anyhow::Result<()> {
    let registry = load_registry()?;
    if registry.is_empty() {
        info!("(no plugins installed)");
    } else {
        info!("📦 Installed Plugins:");
        for (id, entry) in registry {
            info!("  {} [{}] @ {}", id, entry.plugin_type, entry.path);
        }
    }
    Ok(())
}
