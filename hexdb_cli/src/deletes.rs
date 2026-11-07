// `hexdb rm`, `hexdb deletes` and `hexdb trash`: deleting with a preview, the
// delete history, restoring and purging.
//
// Every `rm` by filter, folder or `--all` first asks the server what would
// go (a dry run: the matches and what cascades from them), shows it, and asks
// before deleting: typing the tessellation's name for `--all`, the root
// folder, or more than 100 documents, y otherwise. Without a terminal it
// needs `--yes`. The delete then sends the previewed count as `expect`, so it
// fails instead of deleting more if the data changed in between. Deleting by
// ID previews too when something would cascade.

use anyhow::{bail, Context, Result};
use hexdb_core::HexConfig;
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

/// Above this many documents, confirming means typing the tessellation's name.
const TYPE_TO_CONFIRM: u64 = 100;

pub struct Target<'a> {
    pub config: &'a HexConfig,
    pub base: &'a str,
    pub token: Option<&'a str>,
}

/// What to delete.
pub struct RmArgs<'a> {
    pub tessellation: &'a str,
    /// Document IDs, or folders (starting with '/') of a files tessellation.
    pub targets: &'a [String],
    pub filter: Option<&'a str>,
    pub all: bool,
    pub recursive: bool,
    pub yes: bool,
    pub json: bool,
}

fn encode(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

impl Target<'_> {
    /// Send a request; the parsed answer, or the server's error message.
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut builder = hexdb_core::crypt::http_client().timeout(Duration::from_secs(300));
        if !self.config.tls.ca_file.is_empty() {
            let pem = std::fs::read(&self.config.tls.ca_file).with_context(|| format!("Failed to read tls.ca_file {}", self.config.tls.ca_file))?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        let mut request = builder.build()?.request(method, format!("{}{}", self.base, path));
        if let Some(token) = self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|e| if e.is_connect() { anyhow::anyhow!("HexDB is not reachable at {}.", self.base) } else { e.into() })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let message = value.pointer("/error/message").and_then(Value::as_str).map(String::from).unwrap_or(text);
            match status {
                reqwest::StatusCode::UNAUTHORIZED => bail!("This needs valid credentials: set HEXDB_TOKEN (or --token) to an API key."),
                reqwest::StatusCode::FORBIDDEN => bail!("Not allowed with this API key: {}", message),
                _ => bail!("{}", message),
            }
        }
        Ok(value)
    }
}

/// The request that deletes (or previews): a path and a body or query.
fn delete_request(args: &RmArgs<'_>) -> Result<(String, Option<Value>)> {
    let tess = encode(args.tessellation);
    let folders: Vec<&String> = args.targets.iter().filter(|t| t.starts_with('/')).collect();
    let ids: Vec<&String> = args.targets.iter().filter(|t| !t.starts_with('/')).collect();
    let kinds = [!folders.is_empty(), !ids.is_empty(), args.filter.is_some()].iter().filter(|k| **k).count();
    if kinds > 1 || folders.len() > 1 {
        bail!("Delete one thing at a time: some IDs, one folder, or a --filter.");
    }
    if kinds == 0 && !args.all {
        bail!("Nothing to delete: give IDs, a folder, or --filter '{{...}}'. To delete every document in '{}', use --all.", args.tessellation);
    }
    if let Some(folder) = folders.first() {
        let path: Vec<String> = folder.split('/').filter(|s| !s.is_empty()).map(encode).collect();
        let base = if path.is_empty() { format!("/{}/_folders", tess) } else { format!("/{}/_folders/{}", tess, path.join("/")) };
        let mut query = vec![format!("recursive={}", args.recursive)];
        if args.all {
            query.push("all=true".into());
        }
        return Ok((format!("{}?{}", base, query.join("&")), None));
    }
    let filter = if !ids.is_empty() {
        json!({ "id": { "$in": ids } })
    } else if let Some(text) = args.filter {
        serde_json::from_str(text).context("--filter must be a JSON object, e.g. '{\"status\": \"draft\"}'")?
    } else {
        json!({})
    };
    Ok((format!("/{}/_delete", tess), Some(json!({ "filter": filter, "all": args.all }))))
}

async fn run_delete(target: &Target<'_>, path: &str, body: &Option<Value>, dry_run: bool, expect: Option<u64>) -> Result<Value> {
    match body {
        Some(body) => {
            let mut body = body.clone();
            body["dry_run"] = json!(dry_run);
            if let Some(expect) = expect {
                body["expect"] = json!(expect);
            }
            target.send(reqwest::Method::POST, path, Some(&body)).await
        }
        None => {
            let mut path = path.to_string();
            if dry_run {
                path.push_str("&dry_run=true");
            }
            if let Some(expect) = expect {
                path.push_str(&format!("&expect={}", expect));
            }
            target.send(reqwest::Method::DELETE, &path, None).await
        }
    }
}

fn counts(value: &Value) -> String {
    value
        .as_object()
        .map(|m| m.iter().map(|(t, n)| format!("{} in {}", n, t)).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn print_preview(preview: &Value) {
    println!("This would delete {} document(s) and file(s): {}.", preview["total"], counts(&preview["deleted"]));
    if preview["detached"].as_object().is_some_and(|d| !d.is_empty()) {
        println!("It would also unlink (set the parent link to null): {}.", counts(&preview["detached"]));
    }
    if let Some(sample) = preview["sample"].as_array().filter(|s| !s.is_empty()) {
        let list: Vec<&str> = sample.iter().filter_map(Value::as_str).collect();
        println!("For example: {}", list.join(", "));
    }
}

/// Ask on the terminal. `name`: the text that must be typed (else y/N).
fn confirm(question: &str, name: Option<&str>) -> Result<bool> {
    let stdin = std::io::stdin();
    match name {
        Some(name) => print!("{} Type '{}' to confirm: ", question, name),
        None => print!("{} [y/N] ", question),
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let answer = line.trim();
    Ok(match name {
        Some(name) => answer == name,
        None => answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"),
    })
}

/// `hexdb rm`.
pub async fn rm(target: &Target<'_>, args: RmArgs<'_>) -> Result<()> {
    let (path, body) = delete_request(&args)?;
    let preview = run_delete(target, &path, &body, true, None).await?;
    let total = preview["total"].as_u64().unwrap_or(0);
    if total == 0 {
        if args.json {
            println!("{}", preview);
        } else {
            println!("Nothing matches; nothing was deleted.");
        }
        return Ok(());
    }
    if let Some(blocked) = preview["blocked"].as_object().filter(|b| !b.is_empty()) {
        let list: Vec<String> = blocked.iter().map(|(t, b)| format!("{} in {}", b["count"], t)).collect();
        bail!("Documents link to these with on_delete restrict ({}). Delete or move them first; nothing was deleted.", list.join(", "));
    }
    // Plain deletes by ID with nothing cascading need no preview.
    let plain = !args.targets.is_empty() && !args.targets[0].starts_with('/') && total == args.targets.len() as u64 && preview["detached"].as_object().is_none_or(|d| d.is_empty());
    if !args.json && !plain {
        print_preview(&preview);
    }
    if !args.yes && !plain {
        if !std::io::stdin().is_terminal() {
            bail!("Not deleting without a terminal to confirm on: check the preview above and add --yes.");
        }
        let root = args.targets.first().is_some_and(|t| t.trim_matches('/').is_empty());
        let serious = args.all || root || total > TYPE_TO_CONFIRM;
        if !confirm("Delete them? They go to the trash, and `hexdb deletes restore` puts them back.", serious.then_some(args.tessellation))? {
            println!("Nothing was deleted.");
            return Ok(());
        }
    }
    let done = run_delete(target, &path, &body, false, Some(total)).await?;
    if args.json {
        println!("{}", done);
    } else {
        println!("Deleted {} document(s) and file(s): {}.", done["total"], counts(&done["deleted"]));
        if let Some(batch) = done["batch"].as_str() {
            println!("Delete batch {}. To undo it: hexdb deletes restore {}", batch, batch);
        }
    }
    Ok(())
}

/// `hexdb deletes list`.
pub async fn list(target: &Target<'_>, query: &[(&str, Option<String>)], json: bool) -> Result<()> {
    let params: Vec<String> = query.iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{}={}", k, encode(v)))).collect();
    let path = if params.is_empty() { "/deletes".to_string() } else { format!("/deletes?{}", params.join("&")) };
    let body = target.send(reqwest::Method::GET, &path, None).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let batches = body["batches"].as_array().cloned().unwrap_or_default();
    if batches.is_empty() {
        println!("No deletes in the history.");
        return Ok(());
    }
    println!("{:<26}  {:<24}  {:<11}  {:<12}  {:<9}  WHAT", "BATCH", "AT", "KIND", "BY", "STATE");
    for b in &batches {
        let state = if b["purged_at"].is_string() {
            "purged"
        } else if b["restored_at"].is_string() {
            "restored"
        } else if b["trashed"] == false {
            "not kept"
        } else {
            "in trash"
        };
        println!(
            "{:<26}  {:<24}  {:<11}  {:<12}  {:<9}  {}",
            b["batch"].as_str().unwrap_or(""),
            b["at"].as_str().unwrap_or(""),
            b["kind"].as_str().unwrap_or(""),
            b["by"].as_str().unwrap_or(""),
            state,
            counts(&b["counts"])
        );
    }
    println!("{} of {} shown.", batches.len(), body["total"]);
    Ok(())
}

/// `hexdb deletes show`.
pub async fn show(target: &Target<'_>, batch: &str) -> Result<()> {
    let body = target.send(reqwest::Method::GET, &format!("/deletes/{}", encode(batch)), None).await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// `hexdb deletes restore`.
pub async fn restore(target: &Target<'_>, batch: &str, skip_existing: bool) -> Result<()> {
    let path = format!("/deletes/{}/restore?skip_existing={}", encode(batch), skip_existing);
    let body = target.send(reqwest::Method::POST, &path, None).await?;
    println!("Restored: {}.", counts(&body["restored"]));
    if let Some(skipped) = body["skipped"].as_array().filter(|s| !s.is_empty()) {
        println!("Left out, because they exist again: {}.", skipped.len());
    }
    if body["relinked"].as_u64().unwrap_or(0) > 0 {
        println!("Parent links set back: {}.", body["relinked"]);
    }
    Ok(())
}

/// `hexdb deletes purge`.
pub async fn purge(target: &Target<'_>, batch: &str, yes: bool) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("Purging can't be undone: add --yes to purge without a terminal.");
        }
        if !confirm(&format!("Purge batch {} for good? This can't be undone.", batch), None)? {
            println!("Nothing was purged.");
            return Ok(());
        }
    }
    let body = target.send(reqwest::Method::DELETE, &format!("/deletes/{}", encode(batch)), None).await?;
    println!("Purged {} item(s) of batch {} for good.", body["purged"], batch);
    Ok(())
}

/// `hexdb trash`.
pub async fn trash(target: &Target<'_>, tess: &str, json: bool) -> Result<()> {
    let body = target.send(reqwest::Method::GET, &format!("/{}/_trash", encode(tess)), None).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let docs = body["documents"].as_array().cloned().unwrap_or_default();
    if docs.is_empty() {
        println!("The trash of '{}' is empty.", tess);
        return Ok(());
    }
    for doc in &docs {
        let deleted = &doc["_deleted"];
        let when = deleted["at"].as_str().or_else(|| doc["trashed_at"].as_str()).unwrap_or("");
        let who = deleted["by"].as_str().or_else(|| doc["trashed_by"].as_str()).unwrap_or("");
        let name = doc["name"].as_str().map(|n| format!("{}{}", doc["path"].as_str().unwrap_or(""), n)).unwrap_or_default();
        println!("{}  {}  {}  {}", doc["id"].as_str().unwrap_or(""), when, who, name);
    }
    println!("{} in the trash.", docs.len());
    Ok(())
}
