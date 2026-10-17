// `hexdb service`: run HexDB as a Windows service
//
// `install` registers `hexdb_api --service --config <hexdb.toml>` to start
// with Windows, under its own virtual account (NT SERVICE\<name>) rather than
// LocalSystem, and grants that account access to the configuration and data
// folders (they're private to their owner). Recovery actions restart the
// service 5, 10 and 30 seconds after a crash, and again after a quiet day.
// It needs an elevated (administrator) prompt.
//
// On Linux and macOS, use the systemd unit (packaging/systemd) or
// `brew services`.

use anyhow::{bail, Result};
use std::path::Path;

#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub fn unsupported() -> Result<()> {
    bail!("`hexdb service` is for Windows. On Linux, use the systemd unit (packaging/systemd/hexdb.service, installed by the Debian package); on macOS, `brew services start hexdb`.")
}

/// The account a service named `name` runs as.
pub fn virtual_account(name: &str) -> String {
    format!("NT SERVICE\\{}", name)
}

/// Service names: letters, digits, '-' and '_' (they're also part of the account name).
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("A service name may contain letters, digits, '-' and '_' (up to 64).");
    }
    Ok(())
}

/// The folders the service account needs: the configuration's (read) and the data directory (modify).
pub fn grants(config_file: &Path, storage_dir: &Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let mut grants = Vec::new();
    if let Some(dir) = config_file.parent() {
        grants.push((dir.to_path_buf(), "(OI)(CI)RX"));
    }
    grants.push((storage_dir.to_path_buf(), "(OI)(CI)M"));
    grants
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::Context;
    use std::{ffi::OsString, process::Command, time::Duration};
    use windows_service::{
        service::{
            ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
        },
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    fn manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
        ServiceManager::local_computer(None::<&str>, access).map_err(|e| elevated(e.into()))
    }

    fn elevated(e: anyhow::Error) -> anyhow::Error {
        let text = format!("{:#}", e);
        if text.contains("Access is denied") || text.contains("os error 5") {
            anyhow::anyhow!("{} Run this from an elevated (administrator) prompt.", text)
        } else {
            e
        }
    }

    fn grant(path: &Path, account: &str, rights: &str) -> Result<()> {
        let output = Command::new("icacls").arg(path).arg("/grant").arg(format!("{}:{}", account, rights)).arg("/T").arg("/C").arg("/Q").output().context("Couldn't run icacls")?;
        if !output.status.success() {
            bail!("icacls couldn't give {} access to {}: {}", account, path.display(), String::from_utf8_lossy(&output.stdout).trim());
        }
        Ok(())
    }

    pub fn install(name: &str, server: &Path, config_file: &Path, storage_dir: &Path, account: Option<&str>) -> Result<()> {
        check_name(name)?;
        let server = std::fs::canonicalize(server).with_context(|| format!("Server binary not found at {}", server.display()))?;
        let config_file = std::fs::canonicalize(config_file).with_context(|| format!("Configuration not found at {}", config_file.display()))?;
        let manager = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let account = account.map(String::from).unwrap_or_else(|| virtual_account(name));
        let info = ServiceInfo {
            name: OsString::from(name),
            display_name: OsString::from(if name == "HexDB" { "HexDB".to_string() } else { format!("HexDB ({})", name) }),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: server.clone(),
            launch_arguments: vec![OsString::from("--service"), OsString::from("--config"), config_file.clone().into_os_string()],
            dependencies: vec![],
            account_name: Some(OsString::from(&account)),
            account_password: None,
        };
        let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START).map_err(|e| elevated(e.into()))?;
        let _ = service.set_description("HexDB document database");
        let restart = |seconds| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(seconds) };
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![restart(5), restart(10), restart(30)]),
            })
            .context("Couldn't set the service's recovery actions")?;
        // A nonzero exit (not only a crash) counts as a failure too.
        service.set_failure_actions_on_non_crash_failures(true).context("Couldn't set the service's recovery actions")?;

        std::fs::create_dir_all(storage_dir).with_context(|| format!("Failed to create {}", storage_dir.display()))?;
        if account.starts_with("NT SERVICE\\") {
            for (path, rights) in grants(&config_file, storage_dir) {
                grant(&path, &account, rights)?;
            }
        }
        println!("Installed the {} service: {} --service --config {}", name, server.display(), config_file.display());
        println!("It runs as {} and starts with Windows. Start it now with `hexdb service start`.", account);
        println!("Logs: {}", storage_dir.join("hexdb.log").display());
        Ok(())
    }

    pub fn uninstall(name: &str) -> Result<()> {
        let manager = manager(ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE).map_err(|e| elevated(e.into()))?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            stop_service(&service)?;
        }
        service.delete().map_err(|e| elevated(e.into()))?;
        println!("Removed the {} service. Its data and configuration are untouched.", name);
        Ok(())
    }

    fn stop_service(service: &windows_service::service::Service) -> Result<()> {
        service.stop().map_err(|e| elevated(e.into()))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while service.query_status()?.current_state != ServiceState::Stopped {
            if std::time::Instant::now() > deadline {
                bail!("The service didn't stop within 120 seconds.");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    pub fn start(name: &str) -> Result<()> {
        let manager = manager(ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS).map_err(|e| elevated(e.into()))?;
        service.start::<OsString>(&[]).map_err(|e| elevated(e.into()))?;
        println!("Started the {} service.", name);
        Ok(())
    }

    pub fn stop(name: &str) -> Result<()> {
        let manager = manager(ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS).map_err(|e| elevated(e.into()))?;
        stop_service(&service)?;
        println!("Stopped the {} service.", name);
        Ok(())
    }

    pub fn status(name: &str) -> Result<()> {
        let manager = manager(ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG).map_err(|e| elevated(e.into()))?;
        let status = service.query_status()?;
        let config = service.query_config()?;
        println!("{}: {:?}", name, status.current_state);
        println!("Runs: {}", config.executable_path.display());
        if let Some(account) = config.account_name {
            println!("As: {}", account.to_string_lossy());
        }
        println!("Starts: {:?}", config.start_type);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_and_grants() {
        assert!(check_name("HexDB").is_ok());
        assert!(check_name("hex-2_b").is_ok());
        assert!(check_name("bad name").is_err());
        assert!(check_name("a\\b").is_err());
        assert_eq!(virtual_account("HexDB"), "NT SERVICE\\HexDB");
        let grants = grants(Path::new("C:/hexdb/hexdb.toml"), Path::new("C:/hexdb/data"));
        assert_eq!(grants[0].1, "(OI)(CI)RX", "the configuration is read-only to the service");
        assert_eq!(grants[1].1, "(OI)(CI)M");
    }
}
