// HexDB Core Remote Storage
//
// Backends for file content kept off the hex (see `crate::blobs`): Amazon S3
// and S3-compatible services, Azure Blob Storage, Google Drive and Dropbox,
// chosen by the lattice configuration's `file_storage.*` settings. They store
// whole sealed pack files, which are already encrypted and Reed-Solomon
// sharded, so a provider only ever holds ciphertext. A store needs four
// operations: upload a file, read a byte range, delete, and a connection check.
//
// Requests are signed here rather than with vendor SDKs (S3: AWS Signature
// Version 4; Azure: Shared Key), and Google Drive and Dropbox use OAuth
// refresh tokens to get short-lived access tokens. Every call retries 429s,
// 5xx answers and connection errors with backoff, honoring Retry-After.
// Calls block: they run on the blocking pool, never on an async thread.

use crate::lattice_config::AppliedConfig;
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ATTEMPTS: u32 = 6;
/// Dropbox upload sessions take the file in pieces of this size.
const DROPBOX_PIECE: u64 = 64 * 1024 * 1024;

/// A place to keep pack files.
pub trait RemoteStore: Send + Sync {
    /// The backend's name, for messages.
    fn kind(&self) -> &'static str;
    /// Upload a whole file as `key` (replacing any object there).
    fn put_file(&self, key: &str, path: &Path) -> Result<()>;
    /// Read `len` bytes of `key` from `offset`.
    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Delete `key` (no error if it's already gone).
    fn delete(&self, key: &str) -> Result<()>;
    /// Write, read back and delete a small object under `key`.
    fn check(&self, key: &str) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("hexdb-probe-{}", ulid::Ulid::new()));
        std::fs::write(&dir, b"HexDB storage check")?;
        let result = (|| {
            self.put_file(key, &dir)?;
            let back = self.get_range(key, 6, 7)?;
            if back != b"storage" {
                bail!("read back {:?} instead of what was written", String::from_utf8_lossy(&back));
            }
            self.delete(key)
        })();
        std::fs::remove_file(&dir).ok();
        result
    }
}

/// The settings that pick a backend, from the configuration in effect on a hex.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendSpec {
    pub kind: String,
    /// Identifies where the objects are (not the credentials): kind, location, prefix.
    pub id: String,
    pub prefix: String,
}

fn setting(config: &AppliedConfig, key: &str) -> String {
    config.values.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

fn secret(config: &AppliedConfig, key: &str) -> Result<String> {
    config.secrets.get(key).map(|s| s.expose().to_string()).ok_or_else(|| anyhow!("{} is not set", key))
}

fn required(config: &AppliedConfig, key: &str) -> Result<String> {
    let value = setting(config, key);
    if value.is_empty() {
        bail!("{} is not set", key);
    }
    Ok(value)
}

/// Which backend a configuration chooses (`None`: local disks only).
pub fn spec(config: &AppliedConfig) -> Option<BackendSpec> {
    let kind = setting(config, "file_storage.backend");
    let location = match kind.as_str() {
        "s3" => format!("{}|{}", setting(config, "file_storage.s3.endpoint"), setting(config, "file_storage.s3.bucket")),
        "azure" => format!("{}|{}|{}", setting(config, "file_storage.azure.endpoint"), setting(config, "file_storage.azure.account"), setting(config, "file_storage.azure.container")),
        "gdrive" => setting(config, "file_storage.gdrive.folder_id"),
        "dropbox" => setting(config, "file_storage.dropbox.folder"),
        _ => return None,
    };
    let prefix = setting(config, "file_storage.prefix").trim_matches('/').to_string();
    Some(BackendSpec { id: format!("{}:{}:{}", kind, location, prefix), kind, prefix })
}

/// Build the store a configuration chooses.
pub fn open(config: &AppliedConfig) -> Result<Option<Box<dyn RemoteStore>>> {
    let Some(spec) = spec(config) else { return Ok(None) };
    let store: Box<dyn RemoteStore> = match spec.kind.as_str() {
        "s3" => Box::new(S3::new(
            setting(config, "file_storage.s3.endpoint"),
            required(config, "file_storage.s3.bucket")?,
            setting(config, "file_storage.s3.region"),
            required(config, "file_storage.s3.access_key_id")?,
            secret(config, "file_storage.s3.secret_access_key")?,
        )?),
        "azure" => Box::new(Azure::new(
            setting(config, "file_storage.azure.endpoint"),
            required(config, "file_storage.azure.account")?,
            required(config, "file_storage.azure.container")?,
            &secret(config, "file_storage.azure.account_key")?,
        )?),
        "gdrive" => Box::new(GoogleDrive::new(
            required(config, "file_storage.gdrive.folder_id")?,
            required(config, "file_storage.gdrive.client_id")?,
            secret(config, "file_storage.gdrive.client_secret")?,
            secret(config, "file_storage.gdrive.refresh_token")?,
        )?),
        "dropbox" => Box::new(Dropbox::new(
            setting(config, "file_storage.dropbox.folder"),
            required(config, "file_storage.dropbox.app_key")?,
            secret(config, "file_storage.dropbox.app_secret")?,
            secret(config, "file_storage.dropbox.refresh_token")?,
        )?),
        other => bail!("unknown storage backend {}", other),
    };
    Ok(Some(store))
}

fn client() -> Result<Client> {
    Ok(Client::builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(1800)).build()?)
}

/// Send a request, retrying 429s, 5xx answers and connection errors with backoff.
fn send(what: &str, build: impl Fn() -> Result<RequestBuilder>) -> Result<Response> {
    let mut wait = Duration::from_secs(1);
    for attempt in 1..=ATTEMPTS {
        let outcome = build()?.send();
        let retry_after = match &outcome {
            Ok(r) if r.status() == StatusCode::TOO_MANY_REQUESTS || r.status().is_server_error() => r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(120))),
            Ok(_) => return outcome.map_err(Into::into),
            Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => None,
            Err(_) => return outcome.with_context(|| what.to_string()),
        };
        if attempt == ATTEMPTS {
            return match outcome {
                Ok(r) => Err(failure(what, r)),
                Err(e) => Err(anyhow!(e).context(what.to_string())),
            };
        }
        std::thread::sleep(retry_after.unwrap_or(wait));
        wait = (wait * 2).min(Duration::from_secs(60));
    }
    unreachable!()
}

/// A failed response as an error (body shortened).
fn failure(what: &str, response: Response) -> anyhow::Error {
    let status = response.status();
    let body: String = response.text().unwrap_or_default().chars().take(400).collect();
    anyhow!("{}: {} {}", what, status, body.trim())
}

fn ok(what: &str, response: Response) -> Result<Response> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(failure(what, response))
    }
}

fn read_body(what: &str, response: Response, len: usize) -> Result<Vec<u8>> {
    let response = ok(what, response)?;
    let mut out = Vec::with_capacity(len);
    response.take(len as u64).read_to_end(&mut out)?;
    if out.len() != len {
        bail!("{}: got {} of {} bytes", what, out.len(), len);
    }
    Ok(out)
}

fn file_body(path: &Path) -> Result<(reqwest::blocking::Body, u64)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    Ok((reqwest::blocking::Body::sized(file, len), len))
}

/// RFC 3986 percent-encoding of everything but unreserved characters (and `/` when `keep_slash`).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Amazon S3 and S3-compatible services
// ---------------------------------------------------------------------------

pub struct S3 {
    client: Client,
    /// Empty: Amazon S3 (virtual-hosted style); otherwise path style under this URL.
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

impl S3 {
    pub fn new(endpoint: String, bucket: String, region: String, access_key: String, secret_key: String) -> Result<S3> {
        let region = if region.is_empty() { "us-east-1".to_string() } else { region };
        Ok(S3 { client: client()?, endpoint: endpoint.trim_end_matches('/').to_string(), bucket, region, access_key, secret_key })
    }

    /// The object's URL and its host and path, as signed.
    fn location(&self, key: &str) -> (String, String, String) {
        let path_key = uri_encode(key, true);
        if self.endpoint.is_empty() {
            let host = format!("{}.s3.{}.amazonaws.com", self.bucket, self.region);
            let path = format!("/{}", path_key);
            (format!("https://{}{}", host, path), host, path)
        } else {
            let after_scheme = self.endpoint.split("://").nth(1).unwrap_or(&self.endpoint);
            let (host, base) = match after_scheme.split_once('/') {
                Some((h, b)) => (h.to_string(), format!("/{}", b.trim_matches('/'))),
                None => (after_scheme.to_string(), String::new()),
            };
            let path = format!("{}/{}/{}", base, uri_encode(&self.bucket, false), path_key);
            let scheme = self.endpoint.split("://").next().unwrap_or("https");
            (format!("{}://{}{}", scheme, host, path), host, path)
        }
    }

    /// A request signed with AWS Signature Version 4 (the payload isn't signed).
    fn request(&self, method: reqwest::Method, key: &str) -> RequestBuilder {
        let (url, host, path) = self.location(key);
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let authorization = sigv4_authorization(&self.access_key, &self.secret_key, &self.region, method.as_str(), &host, &path, &amz_date);
        self.client
            .request(method, url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .header("authorization", authorization)
    }
}

/// The Authorization header for an S3 request without a query string.
fn sigv4_authorization(access_key: &str, secret_key: &str, region: &str, method: &str, host: &str, path: &str, amz_date: &str) -> String {
    let date = &amz_date[..8];
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "{}\n{}\n\nhost:{}\nx-amz-content-sha256:UNSIGNED-PAYLOAD\nx-amz-date:{}\n\n{}\nUNSIGNED-PAYLOAD",
        method, path, host, amz_date, signed_headers
    );
    let scope = format!("{}/{}/s3/aws4_request", date, region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, hex::encode(Sha256::digest(canonical.as_bytes())));
    let key = signing_key(secret_key, date, region, "s3");
    let signature = hex::encode(hmac_sha256(&key, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}", access_key, scope, signed_headers, signature)
}

fn signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{}", secret_key).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

impl RemoteStore for S3 {
    fn kind(&self) -> &'static str {
        "S3"
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let what = format!("uploading {} to S3", key);
        let response = send(&what, || {
            let (body, len) = file_body(path)?;
            Ok(self.request(reqwest::Method::PUT, key).header("content-length", len).body(body))
        })?;
        ok(&what, response).map(|_| ())
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let what = format!("reading {} from S3", key);
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let response = send(&what, || Ok(self.request(reqwest::Method::GET, key).header("range", range.clone())))?;
        read_body(&what, response, len)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let what = format!("deleting {} from S3", key);
        let response = send(&what, || Ok(self.request(reqwest::Method::DELETE, key)))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ok(&what, response).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Azure Blob Storage
// ---------------------------------------------------------------------------

const AZURE_VERSION: &str = "2021-08-06";

pub struct Azure {
    client: Client,
    /// Base URL of the account, e.g. https://<account>.blob.core.windows.net (or an emulator's).
    base: String,
    account: String,
    container: String,
    key: Vec<u8>,
}

impl Azure {
    pub fn new(endpoint: String, account: String, container: String, key: &str) -> Result<Azure> {
        let base = if endpoint.is_empty() { format!("https://{}.blob.core.windows.net", account) } else { endpoint.trim_end_matches('/').to_string() };
        let key = STANDARD.decode(key.trim()).context("file_storage.azure.account_key is not base64")?;
        Ok(Azure { client: client()?, base, account, container, key })
    }

    /// A request signed with Shared Key. `headers` are x-ms-* headers besides date and version.
    fn request(&self, method: reqwest::Method, key: &str, content_length: u64, mut headers: Vec<(String, String)>) -> RequestBuilder {
        let url = format!("{}/{}/{}", self.base, uri_encode(&self.container, false), uri_encode(key, true));
        let path = url.splitn(4, '/').nth(3).map(|p| format!("/{}", p)).unwrap_or_default();
        headers.push(("x-ms-date".into(), chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()));
        headers.push(("x-ms-version".into(), AZURE_VERSION.into()));
        let authorization = azure_authorization(&self.account, &self.key, method.as_str(), content_length, &headers, &path);
        let mut builder = self.client.request(method, url).header("authorization", authorization);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

/// The Shared Key Authorization header (no query string, no standard headers but Content-Length).
fn azure_authorization(account: &str, key: &[u8], method: &str, content_length: u64, headers: &[(String, String)], path: &str) -> String {
    let mut ms: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).filter(|(k, _)| k.starts_with("x-ms-")).collect();
    ms.sort();
    let canonical_headers: String = ms.iter().map(|(k, v)| format!("{}:{}\n", k, v)).collect();
    let length = if content_length == 0 { String::new() } else { content_length.to_string() };
    let to_sign = format!("{}\n\n\n{}\n\n\n\n\n\n\n\n\n{}/{}{}", method, length, canonical_headers, account, path);
    format!("SharedKey {}:{}", account, STANDARD.encode(hmac_sha256(key, to_sign.as_bytes())))
}

impl RemoteStore for Azure {
    fn kind(&self) -> &'static str {
        "Azure Blob Storage"
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let what = format!("uploading {} to Azure", key);
        let response = send(&what, || {
            let (body, len) = file_body(path)?;
            Ok(self
                .request(reqwest::Method::PUT, key, len, vec![("x-ms-blob-type".into(), "BlockBlob".into())])
                .header("content-length", len)
                .body(body))
        })?;
        ok(&what, response).map(|_| ())
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let what = format!("reading {} from Azure", key);
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let response = send(&what, || Ok(self.request(reqwest::Method::GET, key, 0, vec![("x-ms-range".into(), range.clone())])))?;
        read_body(&what, response, len)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let what = format!("deleting {} from Azure", key);
        let response = send(&what, || Ok(self.request(reqwest::Method::DELETE, key, 0, Vec::new())))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ok(&what, response).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// OAuth access tokens (Google Drive, Dropbox)
// ---------------------------------------------------------------------------

/// A refresh token turned into short-lived access tokens, cached until shortly before they expire.
struct OAuth {
    token_url: &'static str,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl OAuth {
    fn access_token(&self, client: &Client, provider: &str) -> Result<String> {
        if let Some((token, until)) = self.cached.lock().unwrap().as_ref() {
            if Instant::now() < *until {
                return Ok(token.clone());
            }
        }
        let what = format!("getting a {} access token", provider);
        let response = send(&what, || {
            Ok(client.post(self.token_url).form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ]))
        })?;
        if response.status() == StatusCode::BAD_REQUEST || response.status() == StatusCode::UNAUTHORIZED {
            let body: String = response.text().unwrap_or_default().chars().take(300).collect();
            bail!("{} refused the refresh token (it may have expired or been revoked; connect the account again): {}", provider, body.trim());
        }
        let body: Value = ok(&what, response)?.json()?;
        let token = body["access_token"].as_str().ok_or_else(|| anyhow!("{}: no access_token in the answer", what))?.to_string();
        let lifetime = body["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120).max(60);
        *self.cached.lock().unwrap() = Some((token.clone(), Instant::now() + Duration::from_secs(lifetime)));
        Ok(token)
    }

    fn forget(&self) {
        *self.cached.lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------
// Google Drive
// ---------------------------------------------------------------------------

/// Files go flat into one folder, named after their keys with `/` as `__`.
pub struct GoogleDrive {
    client: Client,
    folder: String,
    oauth: OAuth,
    /// Key -> Drive file ID, as found or created.
    ids: Mutex<std::collections::HashMap<String, String>>,
}

impl GoogleDrive {
    pub fn new(folder: String, client_id: String, client_secret: String, refresh_token: String) -> Result<GoogleDrive> {
        Ok(GoogleDrive {
            client: client()?,
            folder,
            oauth: OAuth { token_url: "https://oauth2.googleapis.com/token", client_id, client_secret, refresh_token, cached: Mutex::new(None) },
            ids: Mutex::new(Default::default()),
        })
    }

    fn name(key: &str) -> String {
        key.replace('/', "__")
    }

    fn token(&self) -> Result<String> {
        self.oauth.access_token(&self.client, "Google")
    }

    /// The Drive file ID of `key`, if it exists.
    fn find(&self, key: &str) -> Result<Option<String>> {
        if let Some(id) = self.ids.lock().unwrap().get(key) {
            return Ok(Some(id.clone()));
        }
        let query = format!("name = '{}' and '{}' in parents and trashed = false", Self::name(key).replace('\'', "\\'"), self.folder.replace('\'', "\\'"));
        let what = format!("finding {} on Google Drive", key);
        let response = send(&what, || {
            Ok(self.client.get("https://www.googleapis.com/drive/v3/files").bearer_auth(self.token()?).query(&[
                ("q", query.as_str()),
                ("fields", "files(id)"),
                ("supportsAllDrives", "true"),
                ("includeItemsFromAllDrives", "true"),
            ]))
        })?;
        let body: Value = ok(&what, response)?.json()?;
        let id = body["files"].as_array().and_then(|f| f.first()).and_then(|f| f["id"].as_str()).map(String::from);
        if let Some(id) = &id {
            self.ids.lock().unwrap().insert(key.to_string(), id.clone());
        }
        Ok(id)
    }

    fn check_auth(&self, response: Response) -> Response {
        if response.status() == StatusCode::UNAUTHORIZED {
            self.oauth.forget();
        }
        response
    }
}

impl RemoteStore for GoogleDrive {
    fn kind(&self) -> &'static str {
        "Google Drive"
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        // Replacing: remove the old file first (Drive allows duplicate names).
        self.delete(key)?;
        let what = format!("uploading {} to Google Drive", key);
        let len = std::fs::metadata(path)?.len();
        let metadata = json!({ "name": Self::name(key), "parents": [self.folder] });
        let response = send(&what, || {
            Ok(self
                .client
                .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&supportsAllDrives=true")
                .bearer_auth(self.token()?)
                .header("x-upload-content-length", len)
                .header("x-upload-content-type", "application/octet-stream")
                .json(&metadata))
        })?;
        let response = ok(&what, self.check_auth(response))?;
        let session = response.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| anyhow!("{}: no upload session", what))?.to_string();
        let response = send(&what, || {
            let (body, len) = file_body(path)?;
            Ok(self.client.put(&session).header("content-length", len).body(body))
        })?;
        let body: Value = ok(&what, response)?.json()?;
        let id = body["id"].as_str().ok_or_else(|| anyhow!("{}: no file ID in the answer", what))?;
        self.ids.lock().unwrap().insert(key.to_string(), id.to_string());
        Ok(())
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let id = self.find(key)?.ok_or_else(|| anyhow!("{} is not on Google Drive", key))?;
        let what = format!("reading {} from Google Drive", key);
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let url = format!("https://www.googleapis.com/drive/v3/files/{}?alt=media&supportsAllDrives=true", uri_encode(&id, false));
        let response = send(&what, || Ok(self.client.get(&url).bearer_auth(self.token()?).header("range", range.clone())))?;
        read_body(&what, self.check_auth(response), len)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let Some(id) = self.find(key)? else { return Ok(()) };
        let what = format!("deleting {} from Google Drive", key);
        let url = format!("https://www.googleapis.com/drive/v3/files/{}?supportsAllDrives=true", uri_encode(&id, false));
        let response = send(&what, || Ok(self.client.delete(&url).bearer_auth(self.token()?)))?;
        self.ids.lock().unwrap().remove(key);
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ok(&what, self.check_auth(response)).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Dropbox
// ---------------------------------------------------------------------------

pub struct Dropbox {
    client: Client,
    /// Folder for HexDB's files, e.g. /HexDB ("" for the app folder's root).
    folder: String,
    oauth: OAuth,
}

impl Dropbox {
    pub fn new(folder: String, app_key: String, app_secret: String, refresh_token: String) -> Result<Dropbox> {
        let folder = format!("/{}", folder.trim_matches('/'));
        Ok(Dropbox {
            client: client()?,
            folder: if folder == "/" { String::new() } else { folder },
            oauth: OAuth { token_url: "https://api.dropboxapi.com/oauth2/token", client_id: app_key, client_secret: app_secret, refresh_token, cached: Mutex::new(None) },
        })
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{}", self.folder, key)
    }

    fn token(&self) -> Result<String> {
        self.oauth.access_token(&self.client, "Dropbox")
    }

    /// A content-endpoint call with its argument in the Dropbox-API-Arg header.
    fn content(&self, endpoint: &str, arg: &Value) -> Result<RequestBuilder> {
        Ok(self
            .client
            .post(format!("https://content.dropboxapi.com/2/{}", endpoint))
            .bearer_auth(self.token()?)
            .header("dropbox-api-arg", ascii_json(arg)))
    }
}

/// JSON for an HTTP header: non-ASCII characters escaped, as Dropbox requires.
fn ascii_json(value: &Value) -> String {
    value.to_string().chars().map(|c| if c.is_ascii() { c.to_string() } else { format!("\\u{:04x}", c as u32) }).collect()
}

impl RemoteStore for Dropbox {
    fn kind(&self) -> &'static str {
        "Dropbox"
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        // Upload sessions: files over 150 MB can't go in one request.
        let what = format!("uploading {} to Dropbox", key);
        let len = std::fs::metadata(path)?.len();
        let piece = |offset: u64| -> Result<Vec<u8>> {
            use std::io::{Seek, SeekFrom};
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            file.take(DROPBOX_PIECE).read_to_end(&mut buf)?;
            Ok(buf)
        };
        let first = piece(0)?;
        let response = send(&what, || Ok(self.content("files/upload_session/start", &json!({ "close": false }))?.header("content-type", "application/octet-stream").body(first.clone())))?;
        let body: Value = ok(&what, response)?.json()?;
        let session = body["session_id"].as_str().ok_or_else(|| anyhow!("{}: no session", what))?.to_string();
        let mut offset = first.len() as u64;
        while offset < len {
            let data = piece(offset)?;
            let arg = json!({ "cursor": { "session_id": session, "offset": offset }, "close": false });
            let response = send(&what, || Ok(self.content("files/upload_session/append_v2", &arg)?.header("content-type", "application/octet-stream").body(data.clone())))?;
            ok(&what, response)?;
            offset += data.len() as u64;
        }
        let arg = json!({ "cursor": { "session_id": session, "offset": offset }, "commit": { "path": self.path(key), "mode": "overwrite", "mute": true } });
        let response = send(&what, || Ok(self.content("files/upload_session/finish", &arg)?.header("content-type", "application/octet-stream").body(Vec::new())))?;
        ok(&what, response).map(|_| ())
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let what = format!("reading {} from Dropbox", key);
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let arg = json!({ "path": self.path(key) });
        let response = send(&what, || Ok(self.content("files/download", &arg)?.header("range", range.clone())))?;
        read_body(&what, response, len)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let what = format!("deleting {} from Dropbox", key);
        let body = json!({ "path": self.path(key) });
        let response = send(&what, || Ok(self.client.post("https://api.dropboxapi.com/2/files/delete_v2").bearer_auth(self.token()?).json(&body)))?;
        if response.status() == StatusCode::CONFLICT {
            // path_lookup/not_found: already gone.
            let text = response.text().unwrap_or_default();
            if text.contains("not_found") {
                return Ok(());
            }
            bail!("{}: 409 {}", what, text.chars().take(300).collect::<String>());
        }
        ok(&what, response).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_signing_key_matches_the_aws_example() {
        // From the AWS Signature Version 4 documentation.
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex::encode(key), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    #[test]
    fn s3_urls_and_signatures() {
        let aws = S3::new(String::new(), "media".into(), "eu-west-1".into(), "AKID".into(), "secret".into()).unwrap();
        assert_eq!(aws.location("hx/a b.hxp").0, "https://media.s3.eu-west-1.amazonaws.com/hx/a%20b.hxp");
        let minio = S3::new("http://127.0.0.1:9000/".into(), "media".into(), String::new(), "AKID".into(), "secret".into()).unwrap();
        let (url, host, path) = minio.location("p/vertex-0/00000001.hxp");
        assert_eq!(url, "http://127.0.0.1:9000/media/p/vertex-0/00000001.hxp");
        assert_eq!((host.as_str(), path.as_str()), ("127.0.0.1:9000", "/media/p/vertex-0/00000001.hxp"));
        let auth = sigv4_authorization("AKID", "secret", "us-east-1", "GET", &host, &path, "20261014T120000Z");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/20261014/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="));
        assert_eq!(auth, sigv4_authorization("AKID", "secret", "us-east-1", "GET", &host, &path, "20261014T120000Z"), "deterministic");
    }

    #[test]
    fn azure_string_to_sign_uses_the_account_and_path() {
        let key = b"key";
        let headers = vec![("x-ms-version".to_string(), AZURE_VERSION.to_string()), ("x-ms-date".to_string(), "Tue, 14 Oct 2026 12:00:00 GMT".to_string())];
        let a = azure_authorization("acct", key, "GET", 0, &headers, "/c/k");
        let expected = {
            let to_sign = format!("GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:Tue, 14 Oct 2026 12:00:00 GMT\nx-ms-version:{}\n/acct/c/k", AZURE_VERSION);
            format!("SharedKey acct:{}", STANDARD.encode(hmac_sha256(key, to_sign.as_bytes())))
        };
        assert_eq!(a, expected);
    }

    #[test]
    fn dropbox_header_arguments_are_ascii() {
        assert_eq!(ascii_json(&json!({ "path": "/HexDB/café" })), "{\"path\":\"/HexDB/caf\\u00e9\"}");
    }
}
