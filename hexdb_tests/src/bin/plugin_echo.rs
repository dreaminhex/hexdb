//! A process plugin for tests: appends every line it receives on stdin to the
//! file named by `ECHO_OUT`, and says hello on stdout (which HexDB logs).
//! With `ECHO_PROBE` set to a file, it first reports whether it could read
//! that file ("probe read" or "probe denied"), to test the sandbox.
//!
//! Delivery tests:
//! - `ECHO_ACK=1` acknowledges each change (`{"ack": <seq>}` on stdout) once it's in the file;
//! - `ECHO_FAIL_AT=<n>` with `ECHO_FAIL_MARKER=<file>`: the first time (no marker yet), it
//!   fails on its n-th line without writing it, after `ECHO_FAIL_DELAY_MS` (default 0),
//!   like a plugin whose destination failed after it read the change;
//! - `ECHO_PAUSE_FILE=<file>`: while that file exists, it holds the change it read (neither
//!   written nor acknowledged), like a plugin whose destination is down.

use std::io::{BufRead, Write};

fn main() {
    let out = std::env::var("ECHO_OUT").expect("ECHO_OUT");
    println!("echo plugin ready for {}", std::env::var("HEXDB_PLUGIN_ID").unwrap_or_default());
    if let Ok(probe) = std::env::var("ECHO_PROBE") {
        match std::fs::read(&probe) {
            Ok(_) => println!("probe read {}", probe),
            Err(e) => println!("probe denied {} ({})", probe, e.kind()),
        }
    }
    let ack = std::env::var("ECHO_ACK").is_ok_and(|v| v == "1");
    let fail_at: Option<usize> = std::env::var("ECHO_FAIL_AT").ok().and_then(|v| v.parse().ok());
    let marker = std::env::var("ECHO_FAIL_MARKER").ok();
    let delay = std::env::var("ECHO_FAIL_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0u64);
    let pause = std::env::var("ECHO_PAUSE_FILE").ok();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(out).expect("open ECHO_OUT");
    for (i, line) in std::io::stdin().lock().lines().enumerate() {
        let line = line.expect("stdin");
        while pause.as_ref().is_some_and(|p| std::path::Path::new(p).exists()) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if let (Some(n), Some(marker)) = (fail_at, &marker) {
            if i + 1 == n && !std::path::Path::new(marker).exists() {
                std::fs::write(marker, "failed once").expect("marker");
                std::thread::sleep(std::time::Duration::from_millis(delay));
                std::process::exit(1);
            }
        }
        writeln!(file, "{}", line).expect("write");
        file.flush().expect("flush");
        if ack {
            if let Some(seq) = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["seq"].as_u64()) {
                println!("{{\"ack\":{}}}", seq);
                std::io::stdout().flush().expect("stdout");
            }
        }
    }
}
