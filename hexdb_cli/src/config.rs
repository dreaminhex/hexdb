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

pub async fn ai_test(target: &Target<'_>) -> Result<()> {
    let result = target.send(reqwest::Method::POST, "/ai/test", &json!({})).await?;
    if result["ok"] == true {
        println!(
            "OK: {} {} answered {:?} in {} ms ({} input, {} output tokens).",
            result["provider"].as_str().unwrap_or("?"),
            result["model"].as_str().unwrap_or("?"),
            result["reply"].as_str().unwrap_or_default(),
            result["millis"],
            result["input_tokens"],
            result["output_tokens"]
        );
        Ok(())
    } else {
        bail!("{} {} failed: {}", result["provider"].as_str().unwrap_or("?"), result["model"].as_str().unwrap_or("?"), result["error"].as_str().unwrap_or("unknown error"))
    }
}

pub async fn ai_usage(target: &Target<'_>, json_out: bool) -> Result<()> {
    let view = target.get("/ai").await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    let ai = &view["ai"];
    if ai["configured"] == true {
        println!("Provider: {} ({})", ai["provider"].as_str().unwrap_or("?"), ai["model"].as_str().unwrap_or("?"));
    } else {
        println!("No AI provider: {}", ai["problem"].as_str().unwrap_or("not configured"));
    }
    let usage = &view["usage"];
    let budget = usage["budget"].as_u64().unwrap_or(0);
    println!(
        "{}: {} tokens ({} input, {} output) in {} call(s), {} failed; budget {}",
        usage["month"].as_str().unwrap_or("this month"),
        usage["total_tokens"],
        usage["input_tokens"],
        usage["output_tokens"],
        usage["calls"],
        usage["failures"],
        if budget == 0 { "unlimited".to_string() } else { format!("{} ({} left)", budget, usage["remaining"]) }
    );
    for (feature, f) in usage["features"].as_object().into_iter().flatten() {
        println!("  {:<24} {} in, {} out, {} call(s)", feature, f["input_tokens"], f["output_tokens"], f["calls"]);
    }
    Ok(())
}

pub async fn ai_estimate(target: &Target<'_>) -> Result<()> {
    let view = target.get("/ai/estimate").await?;
    println!("Estimates (rough; providers count tokens differently). Tagging sends: {}", view["send"]);
    for t in view["tessellations"].as_array().into_iter().flatten() {
        let (tag, vid) = (&t["tagging"], &t["video_thumbnails"]);
        println!("{} ({} files)", t["tessellation"].as_str().unwrap_or("?"), t["files"]);
        println!("  tagging: about {} tokens per file; {} untagged, about {} tokens to tag them", tag["tokens_per_file"], tag["untagged"], tag["tokens_for_untagged"]);
        if vid["videos"].as_u64().unwrap_or(0) > 0 {
            println!(
                "  video thumbnails: about {} tokens per video; {} without one chosen by the AI, about {} tokens",
                vid["tokens_per_video"], vid["without_ai_thumbnail"], vid["tokens_for_those"]
            );
        }
    }
    Ok(())
}

pub async fn ai_queue(target: &Target<'_>, tessellation: &str, route: &str, all: bool) -> Result<()> {
    let path = format!("/{}/{}{}", tessellation, route, if all { "?all=true" } else { "" });
    let result = target.send(reqwest::Method::POST, &path, &json!({})).await?;
    println!("Queued {} file(s); they're processed in the background, one at a time.", result["queued"]);
    Ok(())
}

pub async fn storage_test(target: &Target<'_>) -> Result<()> {
    let result = target.send(reqwest::Method::POST, "/config/storage-test", &json!({})).await?;
    if result["ok"] == true {
        println!("OK: {} works for hex {}.", result["backend"].as_str().unwrap_or("?"), result["hex"].as_str().unwrap_or("?"));
        if let Some(message) = result["message"].as_str() {
            println!("{}", message);
        }
        Ok(())
    } else {
        bail!("{} failed: {}", result["backend"].as_str().unwrap_or("?"), result["error"].as_str().unwrap_or("unknown error"))
    }
}

fn prompt(label: &str) -> Result<String> {
    eprint!("{}: ", label);
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("No value given for {}.", label);
    }
    Ok(line)
}

/// Exchange an authorization code for tokens at `token_url`.
async fn exchange(token_url: &str, form: &[(&str, &str)]) -> Result<String> {
    let response = reqwest::Client::new().post(token_url).form(form).send().await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("the provider refused the code ({}): {}", status, body);
    }
    body["refresh_token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("the provider returned no refresh token (for Google, remove HexDB's access at myaccount.google.com/permissions and connect again)"))
}

/// Wait for the sign-in to come back to http://127.0.0.1:<port>/?code=...&state=...
/// with our `state`, and return the code. Requests without it are answered and ignored.
fn wait_for_code(listener: std::net::TcpListener, state: &str) -> Result<String> {
    use std::io::Read as _;
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf)?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or_default().to_string();
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{}=", name))).map(|v| v.replace('+', " "));
        if param("state").as_deref() != Some(state) {
            let page = "This doesn't belong to the sign-in HexDB started.";
            let _ = write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", page.len(), page);
            continue;
        }
        let (status, page) = match (param("code"), param("error")) {
            (Some(_), _) => ("200 OK", "HexDB received the sign-in. You can close this tab and go back to the terminal."),
            (None, Some(_)) => ("400 Bad Request", "The sign-in was cancelled or refused. Go back to the terminal."),
            (None, None) => continue, // e.g. a favicon request
        };
        let _ = write!(stream, "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, page.len(), page);
        return match param("code") {
            Some(code) => Ok(percent_decode(&code)),
            None => bail!("the sign-in was refused: {}", param("error").unwrap_or_default()),
        };
    }
    bail!("no sign-in arrived")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) }).collect()
}

/// Sign in to Google Drive or Dropbox with the user's own app, and save the
/// app's ID and secret and the refresh token in the lattice configuration.
pub async fn connect(target: &Target<'_>, provider: &str, hex: Option<&str>) -> Result<()> {
    let mut changes = Map::new();
    match provider {
        "gdrive" => {
            println!("Google Drive: use an OAuth client of type \"Desktop app\" from your Google Cloud project (APIs & Services > Credentials), with the Google Drive API enabled.");
            let client_id = prompt("Client ID")?;
            let client_secret = read_secret("Client secret", false)?;
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
            // A random state ties the answer to this sign-in.
            let state = hexdb_core::random_bytes(16).iter().map(|b| format!("{:02x}", b)).collect::<String>();
            let url = format!(
                "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}",
                encode(&client_id),
                encode(&redirect),
                encode("https://www.googleapis.com/auth/drive"),
                state
            );
            println!("\nOpen this address in a browser on this computer and allow access:\n\n{}\n", url);
            let code = tokio::task::spawn_blocking(move || wait_for_code(listener, &state)).await??;
            let refresh = exchange(
                "https://oauth2.googleapis.com/token",
                &[("grant_type", "authorization_code"), ("code", &code), ("client_id", &client_id), ("client_secret", &client_secret), ("redirect_uri", &redirect)],
            )
            .await?;
            changes.insert("file_storage.gdrive.client_id".into(), json!(client_id));
            changes.insert("file_storage.gdrive.client_secret".into(), json!(client_secret));
            changes.insert("file_storage.gdrive.refresh_token".into(), json!(refresh));
        }
        _ => {
            println!("Dropbox: use an app from the Dropbox App Console with the files.content.read and files.content.write permissions.");
            let app_key = prompt("App key")?;
            let app_secret = read_secret("App secret", false)?;
            let url = format!("https://www.dropbox.com/oauth2/authorize?client_id={}&response_type=code&token_access_type=offline", encode(&app_key));
            println!("\nOpen this address in a browser, allow access, and paste the code it shows:\n\n{}\n", url);
            let code = prompt("Code")?;
            let refresh = exchange(
                "https://api.dropboxapi.com/oauth2/token",
                &[("grant_type", "authorization_code"), ("code", &code), ("client_id", &app_key), ("client_secret", &app_secret)],
            )
            .await?;
            changes.insert("file_storage.dropbox.app_key".into(), json!(app_key));
            changes.insert("file_storage.dropbox.app_secret".into(), json!(app_secret));
            changes.insert("file_storage.dropbox.refresh_token".into(), json!(refresh));
        }
    }
    let body = json!({ "changes": changes, "hex": hex, "comment": format!("Connected {}", if provider == "gdrive" { "Google Drive" } else { "Dropbox" }) });
    print_changes(&target.send(reqwest::Method::PUT, "/config", &body).await?);
    println!("Set file_storage.backend to {} (and the folder) to start using it, then run `hexdb config storage-test`.", provider);
    Ok(())
}
