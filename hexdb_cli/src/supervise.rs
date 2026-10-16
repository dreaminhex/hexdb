// `hexdb supervise`: restart a crashed server
//
// `hexdb start --silent` runs this in the background instead of the server
// itself; it runs the server and restarts it when it crashes. It stops (and
// doesn't restart) when:
//   * the server exits cleanly (`hexdb stop`, `POST /shutdown`, a signal);
//   * `hexdb stop` asked for the stop (it leaves a stop request file first,
//     so `stop --force` isn't undone);
//   * the first start fails before the server is listening (it writes its
//     runtime file then): a bad configuration or a taken port won't get
//     better by retrying, and `hexdb start` reports it;
//   * the server crashed MAX_RESTARTS times within RESTART_WINDOW (a crash
//     loop: see hexdb.log and `restarts` in /status).
// Restarts wait 1 s, doubling to 60 s while the server keeps crashing soon
// after starting. Under systemd, Docker or Fly, those restart the server
// themselves and `hexdb supervise` isn't used.

use anyhow::{Context, Result};
use hexdb_core::{
    crashes::{RESTART_COUNT_ENV, RESTART_REASON_ENV},
    runtime::RuntimeInfo,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

const STOP_REQUEST: &str = "stop-requested";
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// A server that ran this long is healthy again: restart quickly next time.
const STABLE: Duration = Duration::from_secs(300);
/// A stop request older than this is left over from a stop that didn't happen.
const STOP_REQUEST_TTL: Duration = Duration::from_secs(600);

fn stop_request_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join(STOP_REQUEST)
}

/// Called by `hexdb stop` before it stops the server, so the supervisor lets it stay stopped.
pub fn request_stop(storage_dir: &Path) {
    let _ = fs::write(stop_request_path(storage_dir), b"");
}

/// True (and the request is consumed) if `hexdb stop` asked for this stop recently.
fn stop_requested(storage_dir: &Path) -> bool {
    let path = stop_request_path(storage_dir);
    let Ok(meta) = fs::metadata(&path) else { return false };
    let _ = fs::remove_file(&path);
    meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age <= STOP_REQUEST_TTL)
}

fn say(message: &str) {
    eprintln!("{} [supervisor] {}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"), message);
}

pub async fn supervise(server: &Path, config: Option<&Path>, storage_dir: &Path, log_path: &Path) -> Result<()> {
    // A stop request from before this supervisor started isn't for it.
    let _ = fs::remove_file(stop_request_path(storage_dir));
    let mut restarts: Vec<Instant> = Vec::new();
    let mut total_restarts = 0u64;
    let mut delay = Duration::from_secs(1);
    let mut last_exit: Option<String> = None;
    loop {
        let log = fs::OpenOptions::new().create(true).append(true).open(log_path).with_context(|| format!("Failed to open {}", log_path.display()))?;
        let mut cmd = std::process::Command::new(server);
        if let Some(config) = config {
            cmd.arg("--config").arg(config);
        }
        cmd.env("HEXDB_SUPERVISED", "1");
        if let Some(exit) = &last_exit {
            cmd.env(RESTART_REASON_ENV, exit).env(RESTART_COUNT_ENV, total_restarts.to_string());
        }
        cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        crate::platform::detach(&mut cmd);
        let started = Instant::now();
        let mut child = tokio::process::Command::from(cmd).spawn().with_context(|| crate::spawn_hint(server))?;
        let pid = child.id();
        let status = child.wait().await?;
        let ran = started.elapsed();
        // The server writes its runtime file once it's listening (and removes it on a clean exit).
        let served = RuntimeInfo::read(storage_dir).ok().flatten().is_some_and(|info| Some(info.pid) == pid);

        if status.success() {
            return Ok(());
        }
        if stop_requested(storage_dir) {
            say(&format!("HexDB stopped ({}) as requested by hexdb stop.", status));
            return Ok(());
        }
        if total_restarts == 0 && !served {
            say(&format!("HexDB exited during startup ({}); not restarting.", status));
            std::process::exit(status.code().unwrap_or(1));
        }
        let now = Instant::now();
        restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        if restarts.len() >= MAX_RESTARTS {
            say(&format!(
                "HexDB crashed ({}) {} times in {} minutes; not restarting. See the log above and `restarts` in /status after starting it again.",
                status,
                restarts.len() + 1,
                RESTART_WINDOW.as_secs() / 60
            ));
            std::process::exit(1);
        }
        delay = if ran >= STABLE { Duration::from_secs(1) } else { delay };
        say(&format!("HexDB exited with {} after {} s; restarting in {} s.", status, ran.as_secs(), delay.as_secs()));
        tokio::time::sleep(delay).await;
        // `hexdb stop` while we waited: the server is already down, stay down.
        if stop_requested(storage_dir) {
            say("Not restarting: hexdb stop was run.");
            return Ok(());
        }
        restarts.push(now);
        total_restarts += 1;
        last_exit = Some(status.to_string());
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}
