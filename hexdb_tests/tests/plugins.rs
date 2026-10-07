//! Plugins: a process plugin and a webhook plugin receive committed changes;
//! broken and disabled plugins are reported.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !check()? {
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// A minimal HTTP server that records POST bodies.
fn webhook_server() -> Result<(String, Arc<Mutex<Vec<Value>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/hook", listener.local_addr()?);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_ok() {
                if let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&body) {
                    sink.lock().unwrap().extend(items);
                }
            }
            let mut stream = reader.into_inner();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    Ok((url, received))
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, text)?;
    Ok(())
}

#[test]
fn plugins_receive_committed_changes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let echo_out = root.join("echo.ndjson");
    let (hook_url, received) = webhook_server()?;
    let toml_path = |p: &Path| p.display().to_string().replace('\\', "/");

    write(
        &root.join("echo/plugin.toml"),
        &format!(
            "id = \"@test/echo\"\nname = \"Echo\"\nversion = \"1.0.0\"\ncommand = ['{}']\ntessellations = [\"orders\"]\n[env]\nECHO_OUT = '{}'\n",
            toml_path(Path::new(env!("CARGO_BIN_EXE_plugin_echo"))),
            toml_path(&echo_out)
        ),
    )?;
    write(
        &root.join("hook/plugin.toml"),
        &format!("id = \"@test/hook\"\nname = \"Hook\"\n[webhook]\nurl = \"{}\"\nbatch_size = 50\n", hook_url),
    )?;
    write(&root.join("broken/plugin.toml"), "id = \"@test/broken\"\nname = \"Nothing to run\"\n")?;
    write(&root.join("off/plugin.toml"), "id = \"@test/off\"\nname = \"Off\"\ncommand = \"does-not-matter\"\n")?;
    write(
        &root.join("plugins.json"),
        &json!({
            "@test/echo": { "path": "./echo" },
            "@test/hook": { "path": "./hook" },
            "@test/broken": { "path": "./broken" },
            "@test/off": { "path": "./off", "enabled": false },
            "@test/missing": { "path": "./missing" },
        })
        .to_string(),
    )?;

    let server = TestServer::start_with(TestOptions {
        extra_toml: format!("\n[plugins]\nregistry = '{}'\n", toml_path(&root.join("plugins.json"))),
        ..Default::default()
    })?;

    let state = |id: &str| -> Result<Value> {
        let list = server.request(Method::GET, "/plugins", None, &[])?;
        Ok(list.body["plugins"].as_array().unwrap().iter().find(|p| p["id"] == id).cloned().unwrap_or(Value::Null))
    };
    wait_until(Duration::from_secs(10), "plugins to start", || {
        Ok(state("@test/echo")?["state"] == "running" && state("@test/hook")?["state"] == "running")
    })?;
    assert_eq!(state("@test/broken")?["state"], "invalid");
    assert!(state("@test/broken")?["last_error"].as_str().unwrap().contains("neither"));
    assert_eq!(state("@test/off")?["state"], "disabled");
    assert_eq!(state("@test/missing")?["state"], "invalid");
    assert_eq!(state("@test/echo")?["runtime"], "process");
    assert_eq!(state("@test/hook")?["runtime"], "webhook");

    let order = server.insert("orders", &json!({ "total": 12 }))?;
    server.insert("notes", &json!({ "text": "not for the echo plugin" }))?;
    server.patch("orders", &json!({ "id": order, "total": 13 }))?;
    server.delete("orders", &order)?;

    // The process plugin got the orders changes only, as JSON lines, in order.
    wait_until(Duration::from_secs(10), "the echo plugin", || {
        Ok(std::fs::read_to_string(&echo_out).map(|t| t.lines().count() >= 3).unwrap_or(false))
    })?;
    let lines: Vec<Value> = std::fs::read_to_string(&echo_out)?.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let ops: Vec<&str> = lines.iter().map(|c| c["op"].as_str().unwrap()).collect();
    assert_eq!(ops, ["put", "put", "delete"]);
    assert!(lines.iter().all(|c| c["tessellation"] == "orders"));
    assert_eq!(lines[1]["document"]["total"], 13);

    // The webhook got every user change.
    wait_until(Duration::from_secs(10), "the webhook", || Ok(received.lock().unwrap().len() >= 4))?;
    let tessellations: Vec<String> = received.lock().unwrap().iter().map(|c| c["tessellation"].as_str().unwrap().to_string()).collect();
    assert_eq!(tessellations, ["orders", "notes", "orders", "orders"]);

    // Delivery counts are reported, and the plugin's stdout reached the server log.
    wait_until(Duration::from_secs(5), "delivery counts", || Ok(state("@test/echo")?["delivered"] == 3))?;
    let logs = server.request(Method::GET, "/logs?target=hexdb_core::plugins&limit=50", None, &[])?;
    assert!(logs.body.to_string().contains("echo plugin ready for @test/echo"), "{}", logs.body);
    Ok(())
}
