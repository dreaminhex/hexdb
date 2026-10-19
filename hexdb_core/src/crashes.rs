// HexDB Core: crash reports
//
// A hex that stops without shutting down (a panic that ends the process, a
// kill, a power cut) leaves its runtime file behind. The next start finds it,
// writes a crash report to `crashes.json` in the data directory (the last 20)
// and records `server.recovered` in the audit trail; `/status` shows them
// under `restarts`. Recovery itself is the usual startup: the write-ahead log
// replays everything acknowledged before the crash.
//
// The panic hook keeps the most recent panic in `last-panic.json`, so the
// report can say what went wrong. A panic in a background task doesn't end
// the process; those are only counted (`restarts.panics_since_start`).
//
// `hexdb start --silent` runs the server under `hexdb supervise`, which
// restarts it after a crash and says so in HEXDB_RESTART_REASON and
// HEXDB_RESTART_COUNT.

use crate::runtime::RuntimeInfo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const CRASHES_FILE: &str = "crashes.json";
const PANIC_FILE: &str = "last-panic.json";
const KEEP: usize = 20;

static PANICS: AtomicU64 = AtomicU64::new(0);

/// The environment variables `hexdb supervise` sets on a restarted server.
pub const RESTART_REASON_ENV: &str = "HEXDB_RESTART_REASON";
pub const RESTART_COUNT_ENV: &str = "HEXDB_RESTART_COUNT";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PanicRecord {
    /// Epoch milliseconds.
    pub at: i64,
    pub message: String,
    pub location: String,
    pub thread: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrashReport {
    /// When the restart found the crash (epoch milliseconds).
    pub detected_at: i64,
    /// When the crashed process had started, and its PID.
    pub previous_started_at: i64,
    pub previous_pid: u32,
    pub previous_version: String,
    /// The last panic of the crashed process, if it panicked.
    pub panic: Option<PanicRecord>,
    /// How the process exited, as the supervisor saw it (e.g. "exit code: 101").
    pub exit: Option<String>,
    /// Set when `hexdb supervise` restarted the server: its restart count.
    pub supervisor_restarts: Option<u64>,
    /// How long opening storage (replaying the write-ahead log) took.
    pub recovery_ms: u64,
}

fn crashes_path(dir: &Path) -> PathBuf {
    dir.join(CRASHES_FILE)
}

fn panic_path(dir: &Path) -> PathBuf {
    dir.join(PANIC_FILE)
}

/// Record panics in `last-panic.json` (then run the default hook, which prints them).
pub fn install_panic_hook(storage_dir: &Path) {
    let path = panic_path(storage_dir);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let record = PanicRecord {
            at: chrono::Utc::now().timestamp_millis(),
            // Panic messages can quote data; keep them short.
            message: message.chars().take(2000).collect(),
            location: info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default(),
            thread: std::thread::current().name().unwrap_or("unnamed").to_string(),
        };
        if let Ok(text) = serde_json::to_vec_pretty(&record) {
            let _ = crate::runtime::write_private_file(&path, &text);
        }
        default(info);
    }));
}

/// Panics in this process so far (most are in background tasks, which the process survives).
pub fn panics_since_start() -> u64 {
    PANICS.load(Ordering::Relaxed)
}

/// A supervised background task (see `crate::tasks::supervise`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskHealth {
    pub name: String,
    /// Times it panicked and was started again.
    pub restarts: u64,
    pub last_panic: Option<String>,
    /// Epoch milliseconds.
    pub last_panic_at: Option<i64>,
    /// False once it has stopped (at shutdown, or a task that finished its work).
    pub running: bool,
}

static TASKS: std::sync::Mutex<std::collections::BTreeMap<String, TaskHealth>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

pub(crate) fn task_started(name: &str) {
    let mut tasks = TASKS.lock().unwrap_or_else(|e| e.into_inner());
    let task = tasks.entry(name.to_string()).or_insert_with(|| TaskHealth { name: name.to_string(), ..Default::default() });
    task.running = true;
}

pub(crate) fn task_panicked(name: &str, message: &str) {
    let mut tasks = TASKS.lock().unwrap_or_else(|e| e.into_inner());
    let task = tasks.entry(name.to_string()).or_insert_with(|| TaskHealth { name: name.to_string(), ..Default::default() });
    task.restarts += 1;
    task.last_panic = Some(message.chars().take(500).collect());
    task.last_panic_at = Some(chrono::Utc::now().timestamp_millis());
}

pub(crate) fn task_stopped(name: &str) {
    if let Some(task) = TASKS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) {
        task.running = false;
    }
}

/// The supervised tasks, for `/status`.
pub fn tasks() -> Vec<TaskHealth> {
    TASKS.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
}

/// If the previous process left its runtime file behind, it didn't shut down
/// cleanly: describe that crash. Call before this process writes its own runtime file.
pub fn detect(storage_dir: &Path, recovery_ms: u64) -> Option<CrashReport> {
    let previous = RuntimeInfo::read(storage_dir).ok().flatten()?;
    let panic = std::fs::read(panic_path(storage_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PanicRecord>(&bytes).ok())
        .filter(|p| p.at >= previous.started_at);
    Some(CrashReport {
        detected_at: chrono::Utc::now().timestamp_millis(),
        previous_started_at: previous.started_at,
        previous_pid: previous.pid,
        previous_version: previous.version.clone(),
        panic,
        exit: std::env::var(RESTART_REASON_ENV).ok().filter(|s| !s.is_empty()),
        supervisor_restarts: std::env::var(RESTART_COUNT_ENV).ok().and_then(|s| s.parse().ok()),
        recovery_ms,
    })
}

/// The most recent crash reports, newest first.
pub fn recent(storage_dir: &Path) -> Vec<CrashReport> {
    std::fs::read(crashes_path(storage_dir)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Add a report to `crashes.json`, keeping the last 20.
pub fn save(storage_dir: &Path, report: &CrashReport) -> anyhow::Result<()> {
    let mut reports = recent(storage_dir);
    reports.insert(0, report.clone());
    reports.truncate(KEEP);
    crate::runtime::write_private_file(&crashes_path(storage_dir), &serde_json::to_vec_pretty(&reports)?)?;
    Ok(())
}

/// `/status` `restarts`.
pub fn status(storage_dir: &Path) -> Value {
    let reports = recent(storage_dir);
    json!({
        "unclean_shutdowns": reports.len(),
        "last": reports.first(),
        "recent": reports.iter().take(5).collect::<Vec<_>>(),
        "panics_since_start": panics_since_start(),
        "task_restarts": tasks().iter().map(|t| t.restarts).sum::<u64>(),
        "tasks": tasks(),
        "supervised": std::env::var("HEXDB_SUPERVISED").is_ok_and(|v| v == "1"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leftover_runtime_file_is_a_crash() {
        let root = std::env::temp_dir().join(format!("hexdb-crashes-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&root).unwrap();
        struct Dir(PathBuf);
        impl Dir {
            fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Dir(root);
        assert!(detect(dir.path(), 5).is_none(), "a clean start");
        let previous = RuntimeInfo::for_current_process("127.0.0.1:7700", chrono::Utc::now().timestamp_millis() - 1000, false).unwrap();
        previous.write(dir.path()).unwrap();
        let old_panic = PanicRecord { at: previous.started_at - 60_000, message: "older".into(), location: String::new(), thread: String::new() };
        crate::runtime::write_private_file(&panic_path(dir.path()), &serde_json::to_vec(&old_panic).unwrap()).unwrap();
        let report = detect(dir.path(), 5).expect("an unclean shutdown");
        assert_eq!(report.previous_pid, std::process::id());
        assert!(report.panic.is_none(), "a panic from before the crashed process started isn't its");

        let panic = PanicRecord { at: previous.started_at + 10, message: "boom".into(), location: "x.rs:1".into(), thread: "main".into() };
        crate::runtime::write_private_file(&panic_path(dir.path()), &serde_json::to_vec(&panic).unwrap()).unwrap();
        let report = detect(dir.path(), 5).unwrap();
        assert_eq!(report.panic.as_ref().unwrap().message, "boom");

        for _ in 0..25 {
            save(dir.path(), &report).unwrap();
        }
        assert_eq!(recent(dir.path()).len(), KEEP);
        assert_eq!(status(dir.path())["unclean_shutdowns"], KEEP);
    }
}
