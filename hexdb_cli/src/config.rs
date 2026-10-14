// `hexdb config`: the lattice configuration from the command line (admins).
//
// Secrets are never taken as command-line arguments (they would end up in
// shell history and process lists): `hexdb config set ai.api_key` prompts for
// the value without echoing it, or reads it from standard input with --stdin.

use anyhow::{bail, Result};
use hexdb_core::HexConfig;
use serde_json::{json, Map, Value};
use std::io::{BufRead, IsTerminal, Write};

pub struct Target<'a> {
    pub config: &'a HexConfig,
    pub base: &'a str,
    pub token: Option<&'a str>,
}

impl Target<'_> {
    async fn get(&self, path: &str) -> Result<Value> {
        let text = crate::request_text(self.config, self.base, reqwest::Method::GET, path, self.token, None).await?;
        Ok(serde_json::from_str(&text)?)
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> Result<Value> {
        let text = crate::request_text(self.config, self.base, method, path, self.token, Some(body)).await?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn schema_entry<'a>(view: &'a Value, key: &str) -> Option<&'a Value> {
    view["schema"].as_array()?.iter().find(|d| d["key"] == key)
}

/// A value for display; secrets show only whether they are set.
fn shown(value: Option<&Value>, secret: bool) -> String {
    match value {
        None => if secret { "(not set)".into() } else { "(default)".into() },
        Some(v) if secret => format!(
            "(set, fingerprint {}, by {} at {})",
            v["fingerprint"].as_str().unwrap_or("?"),
            v["updated_by"].as_str().unwrap_or("?"),
            when(v["updated"].as_i64())
        ),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn when(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_else(|| "-".into())
}

pub async fn get(target: &Target<'_>, key: Option<&str>, json_out: bool) -> Result<()> {
    let view = target.get("/config").await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    if let Some(key) = key {
        let Some(def) = schema_entry(&view, key) else { bail!("Unknown setting '{}'.", key) };
        let secret = def["secret"].as_bool().unwrap_or(false);
        println!("{} = {}", key, shown(view["lattice"].get(key), secret));
        if !secret {
            println!("  default: {}", def["default"]);
        }
        println!("  {}", def["description"].as_str().unwrap_or_default());
        for (hex, overrides) in view["overrides"].as_object().into_iter().flatten() {
            if let Some(v) = overrides.get(key) {
                println!("  hex {}: {}", hex, shown(Some(v), secret));
            }
        }
        return Ok(());
    }
    println!("Lattice configuration version {} {}", view["version"], if view["valid"] == false { "(FAILED VERIFICATION)" } else { "" });
    if view["version"].as_u64().unwrap_or(0) > 0 {
        println!("  saved by {} at {}{}", view["author"].as_str().unwrap_or("?"), when(view["time"].as_i64()), comment(&view["comment"]));
    }
    println!();
    for def in view["schema"].as_array().into_iter().flatten() {
        let key = def["key"].as_str().unwrap_or_default();
        let secret = def["secret"].as_bool().unwrap_or(false);
        let value = view["lattice"].get(key);
        let text = match value {
            None if !secret => format!("{} (default)", plain(&def["default"])),
            other => shown(other, secret),
        };
        println!("{:<40} {}", key, text);
    }
    for (hex, overrides) in view["overrides"].as_object().into_iter().flatten() {
        println!("\nOverrides for hex {}:", hex);
        for (key, value) in overrides.as_object().into_iter().flatten() {
            let secret = schema_entry(&view, key).is_some_and(|d| d["secret"] == true);
            println!("  {:<38} {}", key, shown(Some(value), secret));
        }
    }
    println!("\nHexes:");
    for hex in view["hexes"].as_array().into_iter().flatten() {
        let config = &hex["config"];
        let matches = config["sha256"] == view["sha256"] || view["version"] == 0;
        println!(
            "  {:<20} {:<10} version {} {}{}",
            hex["name"].as_str().unwrap_or("?"),
            hex["role"].as_str().unwrap_or("?"),
            config["version"],
            config["state"].as_str().unwrap_or("?"),
            if matches { "" } else { "  (not the newest version)" }
        );
    }
    Ok(())
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) if s.is_empty() => "\"\"".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn comment(v: &Value) -> String {
    match v.as_str() {
        Some(c) if !c.is_empty() => format!(": {}", c),
        _ => String::new(),
    }
}

/// Read a secret: from standard input with --stdin (or when it isn't a
/// terminal), otherwise from a prompt that doesn't echo.
fn read_secret(key: &str, from_stdin: bool) -> Result<String> {
    let stdin = std::io::stdin();
    let value = if from_stdin || !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        line
    } else {
        eprint!("{} (input hidden): ", key);
        std::io::stderr().flush()?;
        let line = without_echo(|| {
            let mut line = String::new();
            stdin.lock().read_line(&mut line).map(|_| line)
        })?;
        eprintln!();
        line
    };
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.trim().is_empty() {
        bail!("No value given for {}.", key);
    }
    Ok(value)
}

#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE};
    // SAFETY: plain console API calls on this process's own standard input handle.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return read();
        }
        SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT);
        let result = read();
        SetConsoleMode(handle, mode);
        result
    }
}

#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let stty = |arg: &str| std::process::Command::new("stty").arg(arg).stdin(std::process::Stdio::inherit()).status();
    let hidden = stty("-echo").is_ok_and(|s| s.success());
    let result = read();
    if hidden {
        let _ = stty("echo");
    }
    result
}

/// A command-line value: JSON when it parses (true, 42, ["a","b"]), else text.
fn parse_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

fn print_changes(result: &Value) {
    let changes = &result["changes"];
    let mut lines = Vec::new();
    for (label, field) in [("changed", "changed"), ("cleared", "cleared"), ("secret set", "secrets_set"), ("secret cleared", "secrets_cleared")] {
        for key in changes[field].as_array().into_iter().flatten() {
            lines.push(format!("  {} {}", label, key.as_str().unwrap_or_default()));
        }
    }
    if lines.is_empty() {
        println!("Nothing changed (version {}).", changes["version"]);
    } else {
        println!("Saved version {}:", changes["version"]);
        for line in lines {
            println!("{}", line);
        }
    }
}

pub async fn set(target: &Target<'_>, key: &str, value: Option<&str>, from_stdin: bool, hex: Option<&str>, message: Option<&str>) -> Result<()> {
    let view = target.get("/config").await?;
    let Some(def) = schema_entry(&view, key) else { bail!("Unknown setting '{}'. `hexdb config get` lists them.", key) };
    let value = if def["secret"] == true {
        if value.is_some() {
            bail!("{} is a secret: don't put it on the command line. Run `hexdb config set {}` to be prompted, or pipe it in with --stdin.", key, key);
        }
        Value::String(read_secret(key, from_stdin)?)
    } else {
        match (value, from_stdin) {
            (Some(v), _) => parse_value(v),
            (None, true) => parse_value(read_secret(key, true)?.trim()),
            (None, false) => bail!("Give a value for {} (or --stdin).", key),
        }
    };
    let mut changes = Map::new();
    changes.insert(key.to_string(), value);
    let body = json!({ "changes": changes, "hex": hex, "comment": message, "base_version": view["version"] });
    print_changes(&target.send(reqwest::Method::PUT, "/config", &body).await?);
    Ok(())
}

pub async fn unset(target: &Target<'_>, key: &str, hex: Option<&str>, message: Option<&str>) -> Result<()> {
    let mut changes = Map::new();
    changes.insert(key.to_string(), Value::Null);
    let body = json!({ "changes": changes, "hex": hex, "comment": message });
    print_changes(&target.send(reqwest::Method::PUT, "/config", &body).await?);
    Ok(())
}

pub async fn history(target: &Target<'_>, limit: usize, json_out: bool) -> Result<()> {
    let page = target.get(&format!("/config/history?limit={}", limit)).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }
    for v in page["versions"].as_array().into_iter().flatten() {
        if v["valid"] == false {
            println!("{:>5}  FAILED VERIFICATION: {}", v["version"], v["problem"].as_str().unwrap_or_default());
            continue;
        }
        let rollback = v["rollback_of"].as_u64().map(|n| format!(" (rollback to {})", n)).unwrap_or_default();
        println!("{:>5}  {}  {}{}{}", v["version"], when(v["time"].as_i64()), v["author"].as_str().unwrap_or("?"), rollback, comment(&v["comment"]));
        for key in v["diff"]["changed"].as_array().into_iter().flatten() {
            println!("         ~ {}", key.as_str().unwrap_or_default());
        }
        for key in v["diff"]["removed"].as_array().into_iter().flatten() {
            println!("         - {}", key.as_str().unwrap_or_default());
        }
    }
    println!("{} version(s) in all.", page["total"]);
    Ok(())
}

pub async fn show(target: &Target<'_>, version: u64) -> Result<()> {
    let view = target.get(&format!("/config/versions/{}", version)).await?;
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

/// What differs between two versions (secrets by fingerprint only).
pub async fn diff(target: &Target<'_>, from: u64, to: u64) -> Result<()> {
    let a = target.get(&format!("/config/versions/{}", from)).await?;
    let b = target.get(&format!("/config/versions/{}", to)).await?;
    let flatten = |v: &Value| -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = v["lattice"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (hex, scope) in v["hexes"].as_object().into_iter().flatten() {
            out.extend(scope.as_object().into_iter().flatten().map(|(k, v)| (format!("hex:{}/{}", hex, k), v.clone())));
        }
        out
    };
    let (old, new) = (flatten(&a), flatten(&b));
    let find = |list: &[(String, Value)], key: &str| list.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let mut keys: Vec<String> = old.iter().chain(new.iter()).map(|(k, _)| k.clone()).collect();
    keys.sort();
    keys.dedup();
    let mut any = false;
    let show = |v: &Value| if v.get("fingerprint").is_some() { format!("(secret {})", v["fingerprint"].as_str().unwrap_or("?")) } else { v.to_string() };
    for key in keys {
        match (find(&old, &key), find(&new, &key)) {
            (Some(x), Some(y)) if x != y => println!("~ {}: {} -> {}", key, show(&x), show(&y)),
            (Some(x), None) => println!("- {}: {}", key, show(&x)),
            (None, Some(y)) => println!("+ {}: {}", key, show(&y)),
            _ => continue,
        }
        any = true;
    }
    if !any {
        println!("Versions {} and {} have the same settings.", from, to);
    }
    Ok(())
}

pub async fn rollback(target: &Target<'_>, version: u64, message: Option<&str>) -> Result<()> {
    let view = target.send(reqwest::Method::POST, "/config/rollback", &json!({ "version": version, "comment": message })).await?;
    println!("Rolled back to version {}'s settings as version {}.", version, view["version"]);
    Ok(())
}
