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
    assert!(state("@test/broken")?["last_error"].as_str().unwrap().contains("nothing to run"));
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

/// Like `webhook_server`, on a given port.
fn webhook_server_on(port: u16) -> Result<Arc<Mutex<Vec<Value>>>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
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
    Ok(received)
}

#[test]
fn plugins_resume_from_their_saved_position() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    // The webhook is down at first.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    write(
        &root.join("hook/plugin.toml"),
        &format!("id = \"@test/hook\"\nname = \"Hook\"\naudit = true\n[webhook]\nurl = \"http://127.0.0.1:{}/hook\"\n", port),
    )?;
    write(&root.join("plugins.json"), &json!({ "@test/hook": { "path": "./hook" } }).to_string())?;
    let registry = root.join("plugins.json").display().to_string().replace('\\', "/");
    let mut server = TestServer::start_with(TestOptions { extra_toml: format!("\n[plugins]\nregistry = '{}'\n", registry), ..Default::default() })?;

    // The plugin opens its feed and fails to deliver a change. Its starting
    // point is saved anyway, so the restarted plugin doesn't skip to the end.
    let warm_up = server.insert("orders", &json!({ "n": -1 }))?;
    wait_until(Duration::from_secs(15), "a failed delivery", || {
        let plugins = server.request(Method::GET, "/plugins", None, &[])?.body;
        Ok(plugins["plugins"].as_array().is_some_and(|all| all.iter().any(|p| p["id"] == "@test/hook" && p["state"] == "error")))
    })?;
    let first = server.insert("orders", &json!({ "n": 0 }))?;
    let received = webhook_server_on(port)?;
    let ids = |received: &Arc<Mutex<Vec<Value>>>| -> Vec<String> {
        received.lock().unwrap().iter().filter(|c| c["tessellation"] == "orders").filter_map(|c| c["id"].as_str().map(String::from)).collect()
    };
    wait_until(Duration::from_secs(15), "the first delivery", || Ok(ids(&received).contains(&first)))?;
    assert!(ids(&received).contains(&warm_up), "the change made before the failure is delivered too");

    // Changes made while the plugin can't deliver, and across a restart, all arrive, in order.
    let mut expected = vec![warm_up.clone(), first.clone()];
    server.stop()?;
    server.launch()?;
    for n in 1..=3 {
        expected.push(server.insert("orders", &json!({ "n": n }))?);
    }
    wait_until(Duration::from_secs(20), "the backlog after a restart", || {
        let got = ids(&received);
        Ok(expected.iter().all(|id| got.contains(id)))
    })?;
    // At least once (a change may repeat after a restart), in order.
    let mut got: Vec<String> = Vec::new();
    for id in ids(&received) {
        if !got.contains(&id) {
            got.push(id);
        }
    }
    assert_eq!(got, expected);

    // The audit trail is delivered too (sign-ins by the test harness).
    assert!(received.lock().unwrap().iter().any(|c| c["tessellation"] == "_audit" && c["document"]["action"] == "auth.login"));
    Ok(())
}

/// (path, JSON body) of each POST a recording server received.
type Received = Arc<Mutex<Vec<(String, Value)>>>;

/// A minimal HTTP server that records (path, JSON body) of each POST.
fn recording_server() -> Result<(String, Received)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            let mut path = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if path.is_empty() {
                    path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_ok() {
                if let Ok(json) = serde_json::from_slice::<Value>(&body) {
                    sink.lock().unwrap().push((path, json));
                }
            }
            let mut stream = reader.into_inner();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    Ok((url, received))
}

#[test]
fn logs_metrics_otlp_and_source_plugins() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let toml_path = |p: &Path| p.display().to_string().replace('\\', "/");
    let (otlp_url, otlp) = recording_server()?;
    let log_out = root.join("logs.ndjson");
    write(&root.join("otlp-metrics/plugin.toml"), &format!("id = \"@test/otlp-metrics\"\nname = \"OTLP metrics\"\ntype = \"metrics\"\nbuiltin = \"otlp\"\n[otlp]\nendpoint = \"{}\"\nheaders = {{ \"x-api-key\" = \"k\" }}\n", otlp_url))?;
    write(&root.join("otlp-logs/plugin.toml"), &format!("id = \"@test/otlp-logs\"\nname = \"OTLP logs\"\ntype = \"logs\"\nbuiltin = \"otlp\"\n[logs]\nlevel = \"info\"\n[otlp]\nendpoint = \"{}\"\n", otlp_url))?;
    write(
        &root.join("log-file/plugin.toml"),
        &format!(
            "id = \"@test/log-file\"\nname = \"Log file\"\ntype = \"logs\"\ncommand = ['{}']\n[logs]\nlevel = \"info\"\n[env]\nECHO_OUT = '{}'\n",
            toml_path(Path::new(env!("CARGO_BIN_EXE_plugin_echo"))),
            toml_path(&log_out)
        ),
    )?;
    write(
        &root.join("source/plugin.toml"),
        &format!(
            "id = \"@test/source\"\nname = \"Source\"\ntype = \"source\"\ncommand = ['{}']\n[access]\nrole = \"writer\"\ntessellations = [\"ingested\"]\n",
            toml_path(Path::new(env!("CARGO_BIN_EXE_plugin_source")))
        ),
    )?;
    write(&root.join("bad/plugin.toml"), "id = \"@test/bad\"\nname = \"Bad\"\ntype = \"stream\"\nbuiltin = \"otlp\"\n[otlp]\nendpoint = \"http://x\"\n")?;
    write(
        &root.join("plugins.json"),
        &json!({
            "@test/otlp-metrics": { "path": "./otlp-metrics" },
            "@test/otlp-logs": { "path": "./otlp-logs" },
            "@test/log-file": { "path": "./log-file" },
            "@test/source": { "path": "./source" },
            "@test/bad": { "path": "./bad" },
        })
        .to_string(),
    )?;
    let mut server = TestServer::start_with(TestOptions {
        extra_toml: format!("\n[plugins]\nregistry = '{}'\n", toml_path(&root.join("plugins.json"))),
        ..Default::default()
    })?;
    let plugin = |id: &str| -> Result<Value> {
        let list = server.request(Method::GET, "/plugins", None, &[])?;
        Ok(list.body["plugins"].as_array().unwrap().iter().find(|p| p["id"] == id).cloned().unwrap_or(Value::Null))
    };
    assert_eq!(plugin("@test/bad")?["state"], "invalid");
    assert!(plugin("@test/bad")?["last_error"].as_str().unwrap().contains("logs or metrics"));
    assert_eq!(plugin("@test/otlp-metrics")?["runtime"], "builtin");

    // The source plugin wrote through the API as its own user, only where allowed.
    wait_until(Duration::from_secs(15), "the source plugin", || Ok(server.count("ingested").unwrap_or(0) == 1))?;
    // Its refused write is logged just after the allowed one.
    wait_until(Duration::from_secs(10), "the refused write in the log", || {
        Ok(server.request(Method::GET, "/logs?q=source%20wrote&limit=10", None, &[])?.body.to_string().contains("forbidden -> 403"))
    })?;
    let user = server.request(Method::GET, "/users/plugin-test-source", None, &[])?;
    assert_eq!(user.body["roles"][0]["name"], "writer", "{}", user.body);

    // Log records reach the process plugin and the OTLP endpoint.
    server.insert("notes", &json!({ "make": "some log lines" }))?;
    wait_until(Duration::from_secs(10), "log lines in the file", || {
        Ok(std::fs::read_to_string(&log_out).map(|t| t.contains("POST /notes")).unwrap_or(false))
    })?;
    let line: Value = serde_json::from_str(std::fs::read_to_string(&log_out)?.lines().next().unwrap())?;
    assert!(line["level"].is_string() && line["message"].is_string(), "{}", line);
    wait_until(Duration::from_secs(10), "OTLP logs", || Ok(otlp.lock().unwrap().iter().any(|(p, _)| p == "/v1/logs")))?;
    let logs_body = otlp.lock().unwrap().iter().find(|(p, _)| p == "/v1/logs").unwrap().1.clone();
    let record = &logs_body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    assert!(record["severityNumber"].as_u64().unwrap() >= 9, "{}", record);
    assert_eq!(logs_body["resourceLogs"][0]["resource"]["attributes"][0]["value"]["stringValue"], "hexdb");

    // Metrics samples (every 15 s) reach the OTLP endpoint.
    wait_until(Duration::from_secs(25), "OTLP metrics", || Ok(otlp.lock().unwrap().iter().any(|(p, _)| p == "/v1/metrics")))?;
    let metrics_body = otlp.lock().unwrap().iter().find(|(p, _)| p == "/v1/metrics").unwrap().1.clone();
    let names: Vec<String> = metrics_body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"hexdb.documents".to_string()) && names.contains(&"hexdb.writes".to_string()), "{:?}", names);

    // Plugin processes don't outlive a crashed server.
    let pid: u32 = {
        let logs = server.request(Method::GET, "/logs?q=source%20pid&limit=5", None, &[])?;
        let text = logs.body["records"][0]["message"].as_str().unwrap_or_default().to_string();
        text.rsplit(' ').next().unwrap_or_default().parse()?
    };
    assert!(process_exists(pid), "the source plugin runs");
    server.kill();
    wait_until(Duration::from_secs(10), "the plugin to exit with the server", || Ok(!process_exists(pid)))?;
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    if cfg!(windows) {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    } else {
        Path::new(&format!("/proc/{}", pid)).exists()
    }
}
