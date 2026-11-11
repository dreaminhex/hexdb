//! A process plugin for tests: appends every line it receives on stdin to the
//! file named by `ECHO_OUT`, and says hello on stdout (which HexDB logs).
//! With `ECHO_PROBE` set to a file, it first reports whether it could read
//! that file ("probe read" or "probe denied"), to test the sandbox.

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
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(out).expect("open ECHO_OUT");
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        writeln!(file, "{}", line).expect("write");
        file.flush().expect("flush");
    }
}
