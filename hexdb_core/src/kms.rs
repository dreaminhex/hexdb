// HexDB Core: customer-managed keys
//
// The storage key (the data key) can be kept only wrapped by a key-encryption
// key in the customer's AWS KMS or Azure Key Vault ([storage.key]). The hex asks
// the key service to unwrap it at start, keeps it in memory only, and asks
// again every `check_minutes`: a definite refusal means the key was revoked.
// See design/customer-managed-keys.md.

use crate::sigv4::{payload_hash, uri_encode, Signer};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

/// `[storage.key]`: the data key, wrapped by a key in the customer's key service.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ManagedKey {
    /// `aws_kms` or `azure_key_vault`.
    #[serde(default)]
    pub provider: String,
    /// The KMS key's ARN (or ID, with AWS_REGION set), or the Key Vault key's URL.
    #[serde(default)]
    pub key_id: String,
    /// The data key, wrapped by `key_id`: `base64:...`.
    #[serde(default)]
    pub wrapped: String,
    /// How often a running hex asks the key service again (0: only at start).
    #[serde(default = "default_check_minutes")]
    pub check_minutes: u64,
}

fn default_check_minutes() -> u64 {
    15
}

impl Default for ManagedKey {
    fn default() -> Self {
        ManagedKey { provider: String::new(), key_id: String::new(), wrapped: String::new(), check_minutes: default_check_minutes() }
    }
}

impl ManagedKey {
    pub fn is_set(&self) -> bool {
        !self.provider.trim().is_empty() || !self.wrapped.trim().is_empty() || !self.key_id.trim().is_empty()
    }

    pub fn provider(&self) -> Result<Provider> {
        Provider::parse(&self.provider)
    }

    /// The `[storage.key]` section for a configuration file.
    pub fn toml(&self) -> String {
        format!(
            "[storage.key]\nprovider = \"{}\"\nkey_id = \"{}\"\nwrapped = \"{}\"\ncheck_minutes = {}\n",
            self.provider, self.key_id, self.wrapped, self.check_minutes
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    AwsKms,
    AzureKeyVault,
}

impl Provider {
    pub fn parse(name: &str) -> Result<Provider> {
        match name.trim() {
            "aws_kms" => Ok(Provider::AwsKms),
            "azure_key_vault" => Ok(Provider::AzureKeyVault),
            "" => Err(anyhow!("storage.key.provider is not set (aws_kms or azure_key_vault)")),
            other => Err(anyhow!("storage.key.provider is '{}'; it must be aws_kms or azure_key_vault", other)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::AwsKms => "aws_kms",
            Provider::AzureKeyVault => "azure_key_vault",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::AwsKms => "AWS KMS",
            Provider::AzureKeyVault => "Azure Key Vault",
        }
    }
}

/// Why the key service didn't unwrap (or wrap).
#[derive(Debug)]
pub enum KmsError {
    /// A definite answer: the key is disabled, deleted or pending deletion, the
    /// credentials lost permission, or the wrapped value isn't this key's.
    Refused(String),
    /// The key service couldn't be reached or failed: try again later.
    Unreachable(String),
}

impl std::fmt::Display for KmsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KmsError::Refused(m) => write!(f, "refused: {}", m),
            KmsError::Unreachable(m) => write!(f, "unreachable: {}", m),
        }
    }
}

impl std::error::Error for KmsError {}

impl KmsError {
    pub fn is_refused(&self) -> bool {
        matches!(self, KmsError::Refused(_))
    }
}

/// A key service client, for one key.
pub struct KeyService {
    provider: Provider,
    key_id: String,
    client: reqwest::Client,
}

/// The result of wrapping: the wrapped value and the key that wrapped it (with
/// Azure's version, so unwrapping uses the same one).
pub struct Wrapped {
    pub wrapped: String,
    pub key_id: String,
}

impl KeyService {
    pub fn new(provider: Provider, key_id: &str) -> Result<KeyService> {
        let key_id = key_id.trim().to_string();
        if key_id.is_empty() {
            return Err(anyhow!("storage.key.key_id is not set"));
        }
        if provider == Provider::AzureKeyVault {
            let url = reqwest::Url::parse(&key_id).context("storage.key.key_id must be the Key Vault key's URL, https://<vault>.vault.azure.net/keys/<name>/<version>")?;
            let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
            if url.scheme() != "https" && !local {
                return Err(anyhow!("storage.key.key_id must be an https URL"));
            }
        }
        let client = crate::crypt::http_client().timeout(Duration::from_secs(20)).build()?;
        Ok(KeyService { provider, key_id, client })
    }

    pub fn for_key(key: &ManagedKey) -> Result<KeyService> {
        KeyService::new(key.provider()?, &key.key_id)
    }

    /// Unwraps a `base64:` wrapped data key.
    pub async fn unwrap(&self, wrapped: &str) -> std::result::Result<Vec<u8>, KmsError> {
        let blob = decode_wrapped(wrapped).map_err(|e| KmsError::Refused(format!("{:#}", e)))?;
        match self.provider {
            Provider::AwsKms => {
                let body = json!({ "KeyId": self.key_id, "CiphertextBlob": STANDARD.encode(&blob) });
                let response = self.aws("Decrypt", &body).await?;
                let plain = response["Plaintext"].as_str().ok_or_else(|| KmsError::Unreachable("AWS KMS answered without Plaintext".into()))?;
                STANDARD.decode(plain).map_err(|e| KmsError::Unreachable(format!("AWS KMS answered with invalid base64: {}", e)))
            }
            Provider::AzureKeyVault => {
                let body = json!({ "alg": "RSA-OAEP-256", "value": URL_SAFE_NO_PAD.encode(&blob) });
                let response = self.azure("unwrapkey", &body).await?;
                let value = response["value"].as_str().ok_or_else(|| KmsError::Unreachable("Key Vault answered without a value".into()))?;
                URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).map_err(|e| KmsError::Unreachable(format!("Key Vault answered with invalid base64url: {}", e)))
            }
        }
    }

    /// Wraps a data key, returning `base64:...`.
    pub async fn wrap(&self, key: &[u8]) -> std::result::Result<Wrapped, KmsError> {
        match self.provider {
            Provider::AwsKms => {
                let body = json!({ "KeyId": self.key_id, "Plaintext": STANDARD.encode(key) });
                let response = self.aws("Encrypt", &body).await?;
                let blob = response["CiphertextBlob"].as_str().ok_or_else(|| KmsError::Unreachable("AWS KMS answered without CiphertextBlob".into()))?;
                let blob = STANDARD.decode(blob).map_err(|e| KmsError::Unreachable(format!("AWS KMS answered with invalid base64: {}", e)))?;
                let key_id = response["KeyId"].as_str().filter(|k| !k.is_empty()).unwrap_or(&self.key_id).to_string();
                Ok(Wrapped { wrapped: format!("base64:{}", STANDARD.encode(blob)), key_id })
            }
            Provider::AzureKeyVault => {
                let body = json!({ "alg": "RSA-OAEP-256", "value": URL_SAFE_NO_PAD.encode(key) });
                let response = self.azure("wrapkey", &body).await?;
                let value = response["value"].as_str().ok_or_else(|| KmsError::Unreachable("Key Vault answered without a value".into()))?;
                let blob = URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).map_err(|e| KmsError::Unreachable(format!("Key Vault answered with invalid base64url: {}", e)))?;
                let key_id = response["kid"].as_str().filter(|k| !k.is_empty()).unwrap_or(&self.key_id).to_string();
                Ok(Wrapped { wrapped: format!("base64:{}", STANDARD.encode(blob)), key_id })
            }
        }
    }

    /// One AWS KMS call (JSON 1.1 protocol, SigV4-signed).
    async fn aws(&self, action: &str, body: &Value) -> std::result::Result<Value, KmsError> {
        let access_key_id = env("AWS_ACCESS_KEY_ID").ok_or_else(|| KmsError::Refused("AWS_ACCESS_KEY_ID is not set".into()))?;
        let secret = env("AWS_SECRET_ACCESS_KEY").ok_or_else(|| KmsError::Refused("AWS_SECRET_ACCESS_KEY is not set".into()))?;
        let token = env("AWS_SESSION_TOKEN");
        let region = aws_region(&self.key_id).ok_or_else(|| KmsError::Refused("storage.key.key_id isn't a KMS key ARN; use the ARN or set AWS_REGION".into()))?;
        let base = env("HEXDB_AWS_KMS_ENDPOINT").unwrap_or_else(|| format!("https://kms.{}.amazonaws.com", region));
        let base = base.trim_end_matches('/').to_string();
        let host = base.split("://").nth(1).unwrap_or_default().to_string();
        let payload = serde_json::to_vec(body).unwrap_or_default();
        let hash = payload_hash(&payload);
        let now = crate::sigv4::amz_date_now();
        let target = format!("TrentService.{}", action);
        let mut headers = vec![
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("host".to_string(), host),
            ("x-amz-date".to_string(), now.clone()),
            ("x-amz-target".to_string(), target.clone()),
        ];
        if let Some(token) = &token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        let signer = Signer { access_key_id: &access_key_id, secret_access_key: &secret, region: &region, service: "kms" };
        let authorization = signer.authorization("POST", "/", &[], &headers, &hash, &now);
        let mut request = self
            .client
            .post(format!("{}/", base))
            .header("content-type", "application/x-amz-json-1.1")
            .header("x-amz-date", now)
            .header("x-amz-target", target)
            .header("authorization", authorization)
            .body(payload);
        if let Some(token) = token {
            request = request.header("x-amz-security-token", token);
        }
        let response = request.send().await.map_err(|e| KmsError::Unreachable(format!("AWS KMS: {}", e)))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| KmsError::Unreachable(format!("AWS KMS: {}", e)))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json);
        }
        let kind = json["__type"].as_str().unwrap_or_default();
        let kind = kind.rsplit('#').next().unwrap_or(kind).to_string();
        let message = json["message"].as_str().or_else(|| json["Message"].as_str()).unwrap_or(text.as_str());
        let detail = format!("AWS KMS {} answered {} {}: {}", action, status.as_u16(), kind, message.chars().take(300).collect::<String>());
        Err(classify_aws(status.as_u16(), &kind, detail))
    }

    /// One Key Vault key operation, with an Entra ID token.
    async fn azure(&self, operation: &str, body: &Value) -> std::result::Result<Value, KmsError> {
        let token = self.azure_token().await?;
        let url = format!("{}/{}?api-version=7.4", self.key_id.trim_end_matches('/'), operation);
        let response = self.client.post(&url).bearer_auth(token).json(body).send().await.map_err(|e| KmsError::Unreachable(format!("Key Vault: {}", e)))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| KmsError::Unreachable(format!("Key Vault: {}", e)))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json);
        }
        let code = json["error"]["code"].as_str().unwrap_or_default();
        let message = json["error"]["message"].as_str().unwrap_or(text.as_str());
        let detail = format!("Key Vault {} answered {} {}: {}", operation, status.as_u16(), code, message.chars().take(300).collect::<String>());
        Err(classify_http(status.as_u16(), detail))
    }

    /// An access token for Key Vault, from the client-credentials flow.
    async fn azure_token(&self) -> std::result::Result<String, KmsError> {
        let tenant = env("AZURE_TENANT_ID").ok_or_else(|| KmsError::Refused("AZURE_TENANT_ID is not set".into()))?;
        let client_id = env("AZURE_CLIENT_ID").ok_or_else(|| KmsError::Refused("AZURE_CLIENT_ID is not set".into()))?;
        let secret = env("AZURE_CLIENT_SECRET").ok_or_else(|| KmsError::Refused("AZURE_CLIENT_SECRET is not set".into()))?;
        let authority = env("AZURE_AUTHORITY_HOST").unwrap_or_else(|| "https://login.microsoftonline.com".into());
        let url = format!("{}/{}/oauth2/v2.0/token", authority.trim_end_matches('/'), uri_encode(&tenant));
        let form = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
            uri_encode(&client_id),
            uri_encode(&secret),
            uri_encode("https://vault.azure.net/.default")
        );
        let response = self
            .client
            .post(&url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|e| KmsError::Unreachable(format!("Entra ID: {}", e)))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| KmsError::Unreachable(format!("Entra ID: {}", e)))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return json["access_token"].as_str().map(str::to_string).ok_or_else(|| KmsError::Unreachable("Entra ID answered without an access token".into()));
        }
        let code = json["error"].as_str().unwrap_or_default();
        let message = json["error_description"].as_str().unwrap_or(text.as_str());
        Err(classify_http(status.as_u16(), format!("Entra ID answered {} {}: {}", status.as_u16(), code, message.chars().take(300).collect::<String>())))
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The region from a KMS key or alias ARN, else AWS_REGION / AWS_DEFAULT_REGION.
fn aws_region(key_id: &str) -> Option<String> {
    let parts: Vec<&str> = key_id.split(':').collect();
    if parts.len() >= 6 && parts[0] == "arn" && parts[2] == "kms" && !parts[3].is_empty() {
        return Some(parts[3].to_string());
    }
    env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION"))
}

/// AWS KMS errors that mean the key can't be used (as opposed to the service being down).
fn classify_aws(status: u16, kind: &str, detail: String) -> KmsError {
    const TRANSIENT: &[&str] = &["ThrottlingException", "KMSInternalException", "DependencyTimeoutException", "LimitExceededException", "RequestLimitExceeded", "ServiceUnavailable"];
    if TRANSIENT.contains(&kind) {
        return KmsError::Unreachable(detail);
    }
    classify_http(status, detail)
}

/// 4xx (but 408 and 429) is a definite answer; anything else may pass.
fn classify_http(status: u16, detail: String) -> KmsError {
    if (400..500).contains(&status) && status != 408 && status != 429 {
        KmsError::Refused(detail)
    } else {
        KmsError::Unreachable(detail)
    }
}

/// `base64:...` to bytes.
pub fn decode_wrapped(value: &str) -> Result<Vec<u8>> {
    let encoded = value.trim().strip_prefix("base64:").ok_or_else(|| anyhow!("a wrapped key must start with \"base64:\""))?;
    let bytes = STANDARD.decode(encoded.trim()).map_err(|e| anyhow!("a wrapped key isn't valid base64 ({})", e))?;
    if bytes.is_empty() {
        return Err(anyhow!("a wrapped key is empty"));
    }
    Ok(bytes)
}

/// A previous key given as `wrapped:<provider>:<key_id>:base64:...`, split. The key
/// ID may itself hold colons (ARNs) or `://` (Key Vault URLs).
pub fn parse_wrapped_entry(value: &str) -> Option<Result<(Provider, String, String)>> {
    let rest = value.trim().strip_prefix("wrapped:")?;
    Some((|| {
        let (provider, rest) = rest.split_once(':').ok_or_else(|| anyhow!("expected wrapped:<provider>:<key_id>:base64:..."))?;
        let provider = Provider::parse(provider)?;
        let at = rest.rfind(":base64:").ok_or_else(|| anyhow!("expected wrapped:<provider>:<key_id>:base64:..."))?;
        let (key_id, wrapped) = (&rest[..at], &rest[at + 1..]);
        if key_id.is_empty() {
            return Err(anyhow!("a wrapped previous key has no key ID"));
        }
        Ok((provider, key_id.to_string(), wrapped.to_string()))
    })())
}

/// Unwraps, retrying a key service that can't be reached for up to `patience`.
/// A refusal returns at once.
pub async fn unwrap_with_retry(service: &KeyService, wrapped: &str, patience: Duration) -> std::result::Result<Vec<u8>, KmsError> {
    let deadline = std::time::Instant::now() + patience;
    let mut wait = Duration::from_secs(1);
    loop {
        match service.unwrap(wrapped).await {
            Ok(key) => return Ok(key),
            Err(KmsError::Unreachable(m)) if std::time::Instant::now() + wait < deadline => {
                tracing::warn!("⚠️ The key service can't be reached, trying again in {}s: {}", wait.as_secs(), m);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(10));
            }
            Err(e) => return Err(e),
        }
    }
}

fn plain(key: &[u8]) -> Result<String> {
    if key.len() != crate::crypt::ENCRYPTION_KEY_LEN {
        return Err(anyhow!("the unwrapped data key is {} bytes, not {}", key.len(), crate::crypt::ENCRYPTION_KEY_LEN));
    }
    Ok(format!("base64:{}", STANDARD.encode(key)))
}

/// What resolving `[storage.key]` did, for the log and the audit.
pub struct Resolved {
    /// None when only previous keys were wrapped.
    pub provider: Option<Provider>,
    pub key_id: String,
    pub previous: usize,
}

/// Puts the unwrapped data key into `storage.encryption_key` (in memory only) and
/// unwraps any wrapped previous keys. Without `[storage.key]` it does nothing.
pub async fn resolve(config: &mut crate::config::HexConfig, patience: Duration) -> Result<Option<Resolved>> {
    let mut previous = 0;
    for i in 0..config.storage.previous_encryption_keys.len() {
        if let Some(entry) = parse_wrapped_entry(&config.storage.previous_encryption_keys[i]) {
            let (provider, key_id, wrapped) = entry.with_context(|| format!("storage.previous_encryption_keys[{}]", i))?;
            let service = KeyService::new(provider, &key_id)?;
            let key = unwrap_with_retry(&service, &wrapped, patience).await.map_err(|e| anyhow!("storage.previous_encryption_keys[{}]: {} {}", i, provider.label(), e))?;
            config.storage.previous_encryption_keys[i] = plain(&key)?;
            previous += 1;
        }
    }
    let key = config.storage.key.clone();
    if !key.is_set() {
        return Ok(if previous > 0 { Some(Resolved { provider: None, key_id: String::new(), previous }) } else { None });
    }
    if !config.storage.encryption_key.trim().is_empty() {
        return Err(anyhow!("storage.encryption_key and [storage.key] are both set; with a customer-managed key, remove the plain storage.encryption_key"));
    }
    let provider = key.provider()?;
    if key.wrapped.trim().is_empty() {
        return Err(anyhow!("storage.key.wrapped is not set; make one with: hexdb keys new --provider {} --key-id <id>", provider.name()));
    }
    let service = KeyService::for_key(&key)?;
    let data = unwrap_with_retry(&service, &key.wrapped, patience)
        .await
        .map_err(|e| anyhow!("{} didn't unwrap the storage key ({}): {}", provider.label(), key.key_id, e))?;
    config.storage.encryption_key = plain(&data)?;
    Ok(Some(Resolved { provider: Some(provider), key_id: key.key_id, previous }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapped_previous_keys() {
        let (p, id, w) = parse_wrapped_entry("wrapped:aws_kms:arn:aws:kms:eu-west-1:111122223333:key/abc:base64:AAAA").unwrap().unwrap();
        assert_eq!((p, id.as_str(), w.as_str()), (Provider::AwsKms, "arn:aws:kms:eu-west-1:111122223333:key/abc", "base64:AAAA"));
        let (p, id, _) = parse_wrapped_entry("wrapped:azure_key_vault:https://v.vault.azure.net/keys/k/1:base64:AAAA").unwrap().unwrap();
        assert_eq!((p, id.as_str()), (Provider::AzureKeyVault, "https://v.vault.azure.net/keys/k/1"));
        assert!(parse_wrapped_entry("base64:AAAA").is_none());
        assert!(parse_wrapped_entry("wrapped:gcp:x:base64:AAAA").unwrap().is_err());
    }

    #[test]
    fn finds_the_region_and_classifies_errors() {
        assert_eq!(aws_region("arn:aws:kms:ap-south-1:111122223333:alias/hexdb").as_deref(), Some("ap-south-1"));
        assert!(classify_aws(400, "DisabledException", String::new()).is_refused());
        assert!(classify_aws(400, "KMSInvalidStateException", String::new()).is_refused());
        assert!(!classify_aws(400, "ThrottlingException", String::new()).is_refused());
        assert!(!classify_http(503, String::new()).is_refused());
        assert!(classify_http(403, String::new()).is_refused());
        assert!(!classify_http(429, String::new()).is_refused());
    }
}
