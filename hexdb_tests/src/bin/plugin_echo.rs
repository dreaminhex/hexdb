//! A process plugin for tests: appends every line it receives on stdin to the
//! file named by `ECHO_OUT`, and says hello on stdout (which HexDB logs).

use std::io::{BufRead, Write};

fn main() {
    let out = std::env::var("ECHO_OUT").expect("ECHO_OUT");
    println!("echo plugin ready for {}", std::env::var("HEXDB_PLUGIN_ID").unwrap_or_default());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(out).expect("open ECHO_OUT");
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        writeln!(file, "{}", line).expect("write");
        file.flush().expect("flush");
    }
}
