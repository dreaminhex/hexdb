//! Crash recovery: an unclean shutdown is reported in /status and the audit
//! trail on the next start, and `hexdb start --silent` restarts a crashed
//! server, but not one that `hexdb stop` stopped.

use anyhow::{bail, Result};
use hexdb_core::runtime::RuntimeInfo;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::json;
use std::process::Command;
use std::time::{Duration, Instant};

fn restarts(server: &TestServer) -> Result<serde_json::Value> {
    let res = server.request(Method::GET, "/status", None, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(res.body["restarts"].clone())
}

#[test]
fn an_unclean_shutdown_is_reported_after_recovery() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("notes", &json!({ "n": 1 }))?;
    server.restart()?;
    assert_eq!(restarts(&server)?["unclean_shutdowns"], 0, "a clean restart isn't a crash");

    server.crash_and_restart()?;
    let report = restarts(&server)?;
    assert_eq!(report["unclean_shutdowns"], 1, "{}", report);
    assert!(report["last"]["previous_pid"].as_u64().is_some(), "{}", report);
    assert!(report["last"]["recovery_ms"].as_u64().is_some());
    assert!(report["last"]["panic"].is_null(), "it was killed, not a panic");
    assert!(server.get_doc("notes", &id)?.is_some(), "acknowledged writes survive the crash");

    let audit = server.request(Method::GET, "/audit?action=server.recovered", None, &[])?;
    assert_eq!(audit.body["events"].as_array().map(Vec::len), Some(1), "{}", audit.body);
    Ok(())
}

fn healthy(server: &TestServer) -> bool {
    hexdb_tests::http().build().unwrap().get(server.url("/health")).timeout(Duration::from_secs(2)).send().is_ok_and(|r| r.status().is_success())
}

fn kill(pid: u32) -> Result<()> {
    let status = if cfg!(windows) {
        Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output()?.status
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).status()?
    };
    if !status.success() {
        bail!("couldn't kill {}", pid);
    }
    Ok(())
}

/// Wait for a server with a PID other than `old` to answer.
fn wait_for_restart(server: &TestServer, old: u32) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(info) = RuntimeInfo::read(&server.data_dir())? {
            if info.pid != old && healthy(server) {
                return Ok(info.pid);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!("the server didn't come back")
}

fn stays_down(server: &TestServer) -> bool {
    let until = Instant::now() + Duration::from_secs(6);
    while Instant::now() < until {
        if healthy(server) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    true
}

#[test]
fn the_cli_supervisor_restarts_a_crashed_server() -> Result<()> {
    let mut server = TestServer::start()?;
    server.stop()?;
    let config = server.dir().join("hexdb.toml");
    let cli = |args: &[&str]| -> Result<std::process::Output> {
        Ok(Command::new(hexdb_tests::cli_bin()?).arg("--config").arg(&config).args(args).current_dir(server.dir()).env("HEXDB_SERVER_BIN", hexdb_tests::server_bin()?).output()?)
    };
    let started = cli(&["start", "--silent"])?;
    assert!(started.status.success(), "{}{}", String::from_utf8_lossy(&started.stdout), String::from_utf8_lossy(&started.stderr));
    let first = RuntimeInfo::read(&server.data_dir())?.expect("a runtime file").pid;

    // A crash: restarted, and the report says the supervisor did it.
    kill(first)?;
    let second = wait_for_restart(&server, first)?;
    let report = restarts(&server)?;
    assert_eq!(report["supervised"], true, "{}", report);
    assert_eq!(report["last"]["supervisor_restarts"], 1, "{}", report);
    assert!(report["last"]["exit"].as_str().is_some(), "{}", report);

    // `hexdb stop`: stays stopped.
    let stopped = cli(&["stop"])?;
    assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
    assert!(stays_down(&server), "a stopped server isn't restarted");

    // A kill after a stop request (what `stop --force` does) isn't undone either.
    assert!(cli(&["start", "--silent"])?.status.success());
    let third = RuntimeInfo::read(&server.data_dir())?.expect("a runtime file").pid;
    assert_ne!(third, second);
    std::fs::write(server.data_dir().join("stop-requested"), b"")?;
    kill(third)?;
    assert!(stays_down(&server), "a forced stop isn't restarted");

    // A server that can't start (its port is taken) isn't retried: `start` reports it.
    let address = server.url("").trim_start_matches("http://").trim_end_matches('/').to_string();
    let _taken = std::net::TcpListener::bind(&address)?;
    let failed = cli(&["start", "--silent"])?;
    assert!(!failed.status.success(), "{}", String::from_utf8_lossy(&failed.stdout));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("during startup"), "{}", String::from_utf8_lossy(&failed.stderr));
    Ok(())
}
