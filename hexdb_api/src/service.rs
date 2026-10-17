// HexDB server: running as a Windows service
//
// `hexdb service install` registers `hexdb_api --service --config <path>`.
// The service manager starts it; this connects to the manager, reports the
// service running, and turns Stop and Shutdown into the usual graceful
// shutdown (flushing to disk), reporting "stop pending" meanwhile so Windows
// waits for it. A service has no console: output goes to hexdb.log in the
// data directory. Restarting after a crash is the service's recovery
// actions, set at install.

use anyhow::anyhow;
use std::{ffi::OsString, sync::OnceLock, time::Duration};
use windows_service::{
    define_windows_service,
    service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType},
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

/// The name the service is registered under (for a single-process service
/// the manager doesn't check it).
const NAME: &str = "HexDB";
/// How long Windows should wait for a graceful stop.
const STOP_WAIT: Duration = Duration::from_secs(90);

pub const SERVICE_ARG: &str = "--service";

static STATUS: OnceLock<ServiceStatusHandle> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Run as the Windows service (blocks until it stops).
pub fn run() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_service_main).map_err(|e| {
        anyhow!(
            "Couldn't connect to the Windows service manager ({}). {} is only for the HexDB service: install it with `hexdb service install`, or run hexdb_api without it.",
            e,
            SERVICE_ARG
        )
    })
}

fn report(state: ServiceState, exit: u32, wait: Duration) {
    let Some(handle) = STATUS.get() else { return };
    let accepted = if state == ServiceState::Running { ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN } else { ServiceControlAccept::empty() };
    let _ = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: if exit == 0 { ServiceExitCode::Win32(0) } else { ServiceExitCode::ServiceSpecific(exit) },
        checkpoint: 0,
        wait_hint: wait,
        process_id: None,
    });
}

/// Send this process's output to `log` (a service has no console).
fn redirect_output(log: &std::path::Path) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(log) else { return };
    let Ok(copy) = file.try_clone() else { return };
    // The handles stay open for the life of the process.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, file.into_raw_handle() as _);
        SetStdHandle(STD_ERROR_HANDLE, copy.into_raw_handle() as _);
    }
}

fn service_main(_args: Vec<OsString>) {
    let handler = |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            report(ServiceState::StopPending, 0, STOP_WAIT);
            crate::SERVICE_STOP.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(handle) = service_control_handler::register(NAME, handler) else { return };
    let _ = STATUS.set(handle);

    // Output goes to hexdb.log in the data directory (as with `hexdb start -s`).
    if let Ok(config) = crate::parse_args().and_then(|path| Ok(crate::load_config_from(path.as_deref())?)) {
        redirect_output(&config.storage_dir().join("hexdb.log"));
    }
    report(ServiceState::StartPending, 0, Duration::from_secs(120));
    report(ServiceState::Running, 0, Duration::ZERO);
    let result = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|runtime| runtime.block_on(crate::server()));
    let exit = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("HexDB stopped with an error: {:#}", e);
            1
        }
    };
    report(ServiceState::Stopped, exit, Duration::ZERO);
}
