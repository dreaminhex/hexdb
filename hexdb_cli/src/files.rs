// `hexdb put`, `hexdb get` and `hexdb ls`: files tessellations from the command line.
//
// Files under 16 MB are uploaded in one request; larger ones in 8 MB parts of
// a resumable upload, retried from the server's offset when a part fails.
// Downloads stream to disk.

use anyhow::{anyhow, bail, Context, Result};
use hexdb_core::HexConfig;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const RESUMABLE_FROM: u64 = 16 * 1024 * 1024;
const PART: u64 = 8 * 1024 * 1024;

/// A client without an overall timeout (transfers take as long as they take).
fn client(config: &HexConfig) -> Result<reqwest::Client> {
    let mut builder = hexdb_core::crypt::http_client().connect_timeout(Duration::from_secs(10));
    if !config.tls.ca_file.is_empty() {
        let pem = std::fs::read(&config.tls.ca_file).with_context(|| format!("Failed to read tls.ca_file {}", config.tls.ca_file))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem)? {
            builder = builder.add_root_certificate(cert);
        }
    }
    Ok(builder.build()?)
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

/// `a/b c/d.txt` -> `a/b%20c/d.txt`.
fn encode_path(path: &str) -> String {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).map(encode).collect::<Vec<_>>().join("/")
}

async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or(body);
    match status {
        reqwest::StatusCode::UNAUTHORIZED => bail!("{} needs valid credentials: set HEXDB_TOKEN (or --token) to an API key.", what),
        reqwest::StatusCode::FORBIDDEN => bail!("{} isn't allowed with this API key: {}", what, message),
        _ => bail!("{} failed ({}): {}", what, status, message),
    }
}

fn modified_millis(path: &Path) -> Option<u128> {
    std::fs::metadata(path).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Upload local files (folders with `recursive`) into `folder` of a files tessellation.
pub async fn put(config: &HexConfig, base: &str, token: Option<&str>, tess: &str, paths: &[PathBuf], folder: &str, recursive: bool) -> Result<()> {
    crate::check_transport(base, token)?;
    let http = client(config)?;
    let mut jobs: Vec<(PathBuf, String)> = Vec::new();
    for path in paths {
        let meta = std::fs::metadata(path).with_context(|| format!("can't read {}", path.display()))?;
        if meta.is_dir() {
            if !recursive {
                bail!("{} is a folder; use -r to upload its contents.", path.display());
            }
            let root = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            walk(path, &format!("{}/{}", folder.trim_end_matches('/'), root), &mut jobs)?;
        } else {
            let name = path.file_name().ok_or_else(|| anyhow!("{} has no file name", path.display()))?.to_string_lossy().into_owned();
            jobs.push((path.clone(), format!("{}/{}", folder.trim_end_matches('/'), name)));
        }
    }
    let mut failed = 0;
    for (local, remote) in &jobs {
        match put_one(&http, base, token, tess, local, remote).await {
            Ok(doc) => {
                let unchanged = doc.get("unchanged").and_then(|v| v.as_bool()).unwrap_or(false);
                println!(
                    "{} {} -> {}{} ({}, version {})",
                    if unchanged { "=" } else { "↑" },
                    local.display(),
                    doc["path"].as_str().unwrap_or("/"),
                    doc["name"].as_str().unwrap_or_default(),
                    human(doc["size"].as_u64().unwrap_or(0)),
                    doc["version"]
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!("✗ {}: {:#}", local.display(), e);
            }
        }
    }
    if failed > 0 {
        bail!("{} of {} file(s) failed.", failed, jobs.len());
    }
    Ok(())
}

fn walk(dir: &Path, remote: &str, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, &format!("{}/{}", remote, name), out)?;
        } else {
            out.push((path, format!("{}/{}", remote, name)));
        }
    }
    Ok(())
}

async fn put_one(http: &reqwest::Client, base: &str, token: Option<&str>, tess: &str, local: &Path, remote: &str) -> Result<serde_json::Value> {
    let size = std::fs::metadata(local)?.len();
    let modified = modified_millis(local).map(|m| m.to_string());
    let auth = |r: reqwest::RequestBuilder| match token {
        Some(t) => r.bearer_auth(t),
        None => r,
    };
    if size < RESUMABLE_FROM {
        let bytes = tokio::fs::read(local).await?;
        let mut url = format!("{}/{}/_files/{}", base, encode(tess), encode_path(remote));
        if let Some(m) = &modified {
            url.push_str(&format!("?modified={}", m));
        }
        let response = check(auth(http.put(&url)).body(bytes).send().await?, &format!("Uploading {}", local.display())).await?;
        let unchanged = response.headers().get("x-hexdb-unchanged").is_some();
        let mut doc: serde_json::Value = response.json().await?;
        doc["unchanged"] = serde_json::json!(unchanged);
        return Ok(doc);
    }

    let begin = serde_json::json!({ "path": remote, "size": size, "modified": modified });
    let session: serde_json::Value = check(auth(http.post(format!("{}/{}/_uploads", base, encode(tess)))).json(&begin).send().await?, "Starting the upload").await?.json().await?;
    let id = session["id"].as_str().ok_or_else(|| anyhow!("the server didn't return an upload ID"))?.to_string();
    let part_url = format!("{}/{}/_uploads/{}", base, encode(tess), id);
    let mut file = tokio::fs::File::open(local).await?;
    let mut offset = 0u64;
    while offset < size {
        let len = PART.min(size - offset);
        let mut attempt = 0;
        loop {
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; len as usize];
            file.read_exact(&mut buf).await?;
            let sent = auth(http.patch(format!("{}?offset={}", part_url, offset))).body(buf).send().await;
            match sent {
                Ok(r) if r.status().is_success() => break,
                Ok(r) if attempt >= 3 => {
                    check(r, "Uploading a part").await?;
                }
                Err(e) if attempt >= 3 => return Err(e.into()),
                _ => {
                    // Ask where the server is, and resume from there.
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(attempt)).await;
                    let status: serde_json::Value = check(auth(http.get(&part_url)).send().await?, "Checking the upload").await?.json().await?;
                    let at = status["offset"].as_u64().unwrap_or(offset);
                    if at >= offset + len {
                        break;
                    }
                }
            }
        }
        offset += len;
        eprint!("\r  {} / {}", human(offset), human(size));
    }
    eprintln!();
    let response = check(auth(http.post(format!("{}/complete", part_url))).send().await?, "Finishing the upload").await?;
    let unchanged = response.headers().get("x-hexdb-unchanged").is_some();
    let mut doc: serde_json::Value = response.json().await?;
    doc["unchanged"] = serde_json::json!(unchanged);
    Ok(doc)
}

/// Download a file by path (`folder/name`) or ID to `out` (default: its name, here).
pub async fn get(config: &HexConfig, base: &str, token: Option<&str>, tess: &str, file: &str, out: Option<PathBuf>, version: Option<u64>) -> Result<()> {
    crate::check_transport(base, token)?;
    let http = client(config)?;
    let by_id = file.len() == 26 && file.chars().all(|c| c.is_ascii_alphanumeric());
    let mut url = if by_id {
        format!("{}/{}/{}/content", base, encode(tess), file)
    } else {
        format!("{}/{}/_files/{}", base, encode(tess), encode_path(file))
    };
    if let Some(v) = version {
        url.push_str(&format!("?version={}", v));
    }
    let mut request = http.get(&url);
    if let Some(t) = token {
        request = request.bearer_auth(t);
    }
    let mut response = check(request.send().await?, &format!("Downloading {}", file)).await?;
    let name = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split("filename=\"").nth(1))
        .and_then(|v| v.split('"').next())
        .map(String::from)
        .unwrap_or_else(|| file.rsplit('/').next().unwrap_or("download").to_string());
    let target = match out {
        Some(path) if path.is_dir() => path.join(&name),
        Some(path) => path,
        None => PathBuf::from(&name),
    };
    let total = response.content_length();
    let partial = target.with_extension(format!("{}.part", target.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
    let mut dest = tokio::fs::File::create(&partial).await.with_context(|| format!("can't write {}", partial.display()))?;
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await? {
        dest.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if let Some(total) = total {
            eprint!("\r  {} / {}", human(written), human(total));
        }
    }
    dest.flush().await?;
    dest.sync_all().await?;
    drop(dest);
    tokio::fs::rename(&partial, &target).await?;
    if total.is_some() {
        eprintln!();
    }
    println!("↓ {} ({})", target.display(), human(written));
    Ok(())
}

/// List a folder (subfolders and files), or search with `query`.
pub async fn ls(config: &HexConfig, base: &str, token: Option<&str>, tess: &str, folder: &str, query: Option<&str>) -> Result<()> {
    crate::check_transport(base, token)?;
    let http = client(config)?;
    let auth = |r: reqwest::RequestBuilder| match token {
        Some(t) => r.bearer_auth(t),
        None => r,
    };
    let folder = format!("/{}/", folder.trim_matches(['/', '\\'])).replace("//", "/");
    let filter = match query {
        Some(q) => serde_json::json!({ "$text": q }),
        None => serde_json::json!({ "path": folder }),
    };
    if query.is_none() {
        let aggregation = serde_json::json!({ "filter": { "path": { "$startsWith": folder } }, "group_by": ["path"], "aggregates": { "n": { "$count": "*" } }, "limit": 10000 });
        let groups: serde_json::Value = check(auth(http.post(format!("{}/{}/_aggregate", base, encode(tess)))).json(&aggregation).send().await?, "Listing folders").await?.json().await?;
        let depth = folder.split('/').filter(|s| !s.is_empty()).count();
        let mut subfolders = std::collections::BTreeMap::new();
        for row in groups["rows"].as_array().into_iter().flatten() {
            let parts: Vec<&str> = row["path"].as_str().unwrap_or("/").split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() > depth {
                *subfolders.entry(parts[depth].to_string()).or_insert(0u64) += row["n"].as_u64().unwrap_or(0);
            }
        }
        for (name, count) in subfolders {
            println!("{:<10} {:>10}  {}/", "folder", format!("{} files", count), name);
        }
    }
    let body = serde_json::json!({ "filter": filter, "sort": if query.is_some() { serde_json::Value::Null } else { serde_json::json!("name") }, "limit": 1000, "fields": ["name", "path", "size", "content_type", "modified_at"] });
    let page: serde_json::Value = check(auth(http.post(format!("{}/{}/_query", base, encode(tess)))).json(&body).send().await?, "Listing files").await?.json().await?;
    for doc in page["documents"].as_array().into_iter().flatten() {
        let name = if query.is_some() { format!("{}{}", doc["path"].as_str().unwrap_or(""), doc["name"].as_str().unwrap_or("")) } else { doc["name"].as_str().unwrap_or("").to_string() };
        println!(
            "{:<10} {:>10}  {}  {}",
            doc["content_type"].as_str().unwrap_or("").split('/').next().unwrap_or(""),
            human(doc["size"].as_u64().unwrap_or(0)),
            doc["modified_at"].as_str().map(|t| &t[..t.len().min(16)]).unwrap_or(""),
            name
        );
    }
    Ok(())
}
