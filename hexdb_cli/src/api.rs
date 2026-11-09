// The CLI's requests to a hex's API: JSON in and out, the server's error
// messages as errors, and safe retries for writes.
//
// Every write the CLI sends carries an Idempotency-Key: the one given with
// --idempotency-key, or one made up for this command. When the connection
// drops, times out, or the hex answers 503 (an Overseer restarting or handing
// over), the write is sent again with the same key, up to three times. The
// server stores each write's result with the write itself, so a retry of a
// write that did go through returns that result instead of writing again.

use anyhow::{bail, Context, Result};
use hexdb_core::HexConfig;
use serde_json::Value;
use std::time::Duration;

/// Attempts for a write whose answer was lost.
const WRITE_ATTEMPTS: u32 = 3;

pub struct Target<'a> {
    pub config: &'a HexConfig,
    pub base: &'a str,
    pub token: Option<&'a str>,
}

/// URL-encode one path segment.
pub fn encode(segment: &str) -> String {
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

/// A fresh idempotency key for one CLI command.
pub fn new_key() -> String {
    let bytes = hexdb_core::random_bytes(12);
    format!("hexdb-cli-{}", bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>())
}

enum Failure {
    /// Worth sending again with the same key: the answer was lost or the hex is busy.
    Retry(anyhow::Error),
    Final(anyhow::Error),
}

impl Target<'_> {
    fn client(&self) -> Result<reqwest::Client> {
        let mut builder = hexdb_core::crypt::http_client().timeout(Duration::from_secs(300));
        if !self.config.tls.ca_file.is_empty() {
            let pem = std::fs::read(&self.config.tls.ca_file).with_context(|| format!("Failed to read tls.ca_file {}", self.config.tls.ca_file))?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(builder.build()?)
    }

    async fn attempt(&self, method: &reqwest::Method, path: &str, body: Option<&Value>, key: Option<&str>) -> std::result::Result<(Value, reqwest::header::HeaderMap), Failure> {
        let client = self.client().map_err(Failure::Final)?;
        let mut request = client.request(method.clone(), format!("{}{}", self.base, path));
        if let Some(token) = self.token {
            request = request.bearer_auth(token);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) if e.is_connect() => return Err(Failure::Retry(anyhow::anyhow!("HexDB is not reachable at {}.", self.base))),
            Err(e) if e.is_timeout() => return Err(Failure::Retry(anyhow::anyhow!("HexDB at {} didn't answer in time.", self.base))),
            Err(e) => return Err(Failure::Final(e.into())),
        };
        let status = response.status();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok((value, headers));
        }
        let message = value.pointer("/error/message").and_then(Value::as_str).map(String::from).unwrap_or(text);
        Err(match status {
            reqwest::StatusCode::UNAUTHORIZED => Failure::Final(anyhow::anyhow!("This needs valid credentials: set HEXDB_TOKEN (or --token) to an API key.")),
            reqwest::StatusCode::FORBIDDEN => Failure::Final(anyhow::anyhow!("Not allowed with this API key: {}", message)),
            reqwest::StatusCode::SERVICE_UNAVAILABLE => Failure::Retry(anyhow::anyhow!("{}", message)),
            _ => Failure::Final(anyhow::anyhow!("{}", message)),
        })
    }

    /// A read (or anything not retried): the answer, or the server's error message.
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        match self.attempt(&method, path, body, None).await {
            Ok((value, _)) => Ok(value),
            Err(Failure::Retry(e) | Failure::Final(e)) => Err(e),
        }
    }

    /// A read, with the answer's headers.
    pub async fn read(&self, path: &str) -> Result<(Value, reqwest::header::HeaderMap)> {
        match self.attempt(&reqwest::Method::GET, path, None, None).await {
            Ok(answer) => Ok(answer),
            Err(Failure::Retry(e) | Failure::Final(e)) => Err(e),
        }
    }

    /// A write with an idempotency key, sent again with the same key when its
    /// answer was lost. Returns the answer and its headers.
    pub async fn write(&self, method: reqwest::Method, path: &str, body: Option<&Value>, key: &str) -> Result<(Value, reqwest::header::HeaderMap)> {
        let mut last = None;
        for attempt in 0..WRITE_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            match self.attempt(&method, path, body, Some(key)).await {
                Ok(answer) => return Ok(answer),
                Err(Failure::Final(e)) => return Err(e),
                Err(Failure::Retry(e)) => last = Some(e),
            }
        }
        match last {
            Some(e) => bail!("{} (tried {} times with idempotency key {}; sending it again with that key can't apply it twice)", e, WRITE_ATTEMPTS, key),
            None => bail!("The write failed."),
        }
    }
}

/// Command-line data: JSON text, @file, or - for standard input.
pub fn read_json(arg: &str) -> Result<Value> {
    let text = if arg == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else if let Some(path) = arg.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path))?
    } else {
        arg.to_string()
    };
    serde_json::from_str(&text).context("Not valid JSON (give JSON text, @file.json, or - to read standard input)")
}
