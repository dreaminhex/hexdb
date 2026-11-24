// HexDB Core: a small client for S3-compatible object stores
//
// Point-in-time restore (design/point-in-time-restore.md) ships base backups,
// WAL segments and file chunks to any store that speaks the S3 API: AWS S3,
// Cloudflare R2, Backblaze B2, MinIO, Tigris, Google Cloud Storage's
// interoperability API. It needs four operations, PUT, GET, ListObjectsV2 and
// DELETE, each signed with AWS Signature Version 4.
//
// SigV4 is implemented here on the HMAC, SHA-256 and HTTP libraries HexDB
// already uses: the usual Rust object-store crates would bring back chrono,
// which 2.6 removed. Requests address the bucket in the path
// (`https://host/bucket/key`), which every compatible store accepts, except on
// AWS itself, where the bucket goes in the host name
// (`https://bucket.s3.region.amazonaws.com/key`), as AWS recommends.

use crate::sigv4::{canonical_query, payload_hash, uri_encode, Signer};
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

/// Where and how to reach a bucket.
#[derive(Clone)]
pub struct StoreConfig {
    /// `https://s3.us-east-1.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com`, `http://127.0.0.1:9000`...
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        f.debug_struct("StoreConfig").field("endpoint", &self.endpoint).field("bucket", &self.bucket).field("region", &self.region).finish_non_exhaustive()
    }
}

/// An object in a listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
}

/// A bucket in an S3-compatible store.
pub struct ObjectStore {
    config: StoreConfig,
    /// `https://host[:port]`, without a trailing slash.
    base: String,
    /// The bucket goes in the host name (AWS) rather than the path.
    virtual_host: bool,
    client: reqwest::Client,
}

#[cfg(test)]
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

impl ObjectStore {
    pub fn new(config: StoreConfig) -> Result<Self> {
        let endpoint = config.endpoint.trim().trim_end_matches('/').to_string();
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
            bail!("the object store endpoint must start with https:// (or http:// for a local store)");
        }
        if config.bucket.is_empty() || config.bucket.contains('/') {
            bail!("the bucket name is required and can't contain '/'");
        }
        let host = endpoint.split("://").nth(1).unwrap_or_default();
        let virtual_host = host.ends_with(".amazonaws.com") && !config.bucket.contains('.');
        let base = if virtual_host {
            let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", host));
            format!("{}://{}.{}", scheme, config.bucket, host)
        } else {
            endpoint
        };
        let client = crate::crypt::http_client().timeout(Duration::from_secs(600)).build()?;
        Ok(ObjectStore { config, base, virtual_host, client })
    }

    /// The URL path of `key` (each segment encoded as SigV4 requires).
    fn path(&self, key: &str) -> String {
        let key: String = key.split('/').map(uri_encode).collect::<Vec<_>>().join("/");
        if self.virtual_host {
            format!("/{}", key)
        } else {
            format!("/{}/{}", uri_encode(&self.config.bucket), key)
        }
    }

    async fn send(&self, method: reqwest::Method, path: &str, query: &[(&str, String)], body: Vec<u8>) -> Result<reqwest::Response> {
        let host = self.base.split("://").nth(1).unwrap_or_default().to_string();
        let payload_hash = payload_hash(&body);
        let now = crate::sigv4::amz_date_now();
        let headers = vec![("host".to_string(), host), ("x-amz-content-sha256".to_string(), payload_hash.clone()), ("x-amz-date".to_string(), now.clone())];
        let authorization = authorization(&self.config, method.as_str(), path, query, &headers, &payload_hash, &now);
        let query_string = canonical_query(query);
        let url = if query_string.is_empty() { format!("{}{}", self.base, path) } else { format!("{}{}?{}", self.base, path, query_string) };
        let mut request = self.client.request(method, &url).header("x-amz-content-sha256", payload_hash).header("x-amz-date", now).header("authorization", authorization);
        if !body.is_empty() {
            request = request.body(body);
        }
        request.send().await.with_context(|| format!("couldn't reach the object store at {}", self.base))
    }

    /// Create the bucket if it doesn't exist yet (stores that make buckets elsewhere, as most
    /// hosted ones do, refuse this without harm when it's there already).
    pub async fn ensure_bucket(&self) -> Result<()> {
        let path = if self.virtual_host { "/".to_string() } else { format!("/{}", uri_encode(&self.config.bucket)) };
        let response = self.send(reqwest::Method::PUT, &path, &[], Vec::new()).await?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        check(response, "creating the bucket", &self.config.bucket).await.map(|_| ())
    }

    /// Store `body` under `key` (replacing what was there).
    pub async fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let response = self.send(reqwest::Method::PUT, &self.path(key), &[], body).await?;
        check(response, "storing", key).await.map(|_| ())
    }

    /// The object under `key`, or None if there's none.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let response = self.send(reqwest::Method::GET, &self.path(key), &[], Vec::new()).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = check(response, "reading", key).await?;
        Ok(Some(response.bytes().await?.to_vec()))
    }

    /// Delete `key` (already gone is fine).
    pub async fn delete(&self, key: &str) -> Result<()> {
        let response = self.send(reqwest::Method::DELETE, &self.path(key), &[], Vec::new()).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        check(response, "deleting", key).await.map(|_| ())
    }

    /// Every object whose key starts with `prefix`, in key order.
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let path = if self.virtual_host { "/".to_string() } else { format!("/{}", uri_encode(&self.config.bucket)) };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2".to_string()), ("prefix", prefix.to_string())];
            if let Some(t) = &token {
                query.push(("continuation-token", t.clone()));
            }
            let response = self.send(reqwest::Method::GET, &path, &query, Vec::new()).await?;
            let body = check(response, "listing", prefix).await?.bytes().await?;
            let (page, next) = parse_listing(&body)?;
            out.extend(page);
            match next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        Ok(out)
    }
}

/// The response, or an error naming what failed and the store's own message.
async fn check(response: reqwest::Response, what: &str, key: &str) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = crate::extract::xml::parse(body.as_bytes())
        .and_then(|root| find(&root, "Message").map(|n| n.text.trim().to_string()))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(anyhow!("the object store refused {} '{}': {} {}", what, key, status.as_u16(), message))
}

fn find<'a>(node: &'a crate::extract::xml::Node, name: &str) -> Option<&'a crate::extract::xml::Node> {
    if node.local() == name {
        return Some(node);
    }
    node.children.iter().find_map(|c| find(c, name))
}

/// One ListObjectsV2 page: its objects, and the token for the next page.
fn parse_listing(body: &[u8]) -> Result<(Vec<ObjectInfo>, Option<String>)> {
    let root = crate::extract::xml::parse(body).ok_or_else(|| anyhow!("the object store's listing isn't XML"))?;
    let result = find(&root, "ListBucketResult").ok_or_else(|| anyhow!("the object store's listing has no ListBucketResult"))?;
    let mut objects = Vec::new();
    for contents in result.children.iter().filter(|c| c.local() == "Contents") {
        let field = |name: &str| contents.children.iter().find(|c| c.local() == name).map(|c| c.text.trim().to_string()).unwrap_or_default();
        objects.push(ObjectInfo { key: field("Key"), size: field("Size").parse().unwrap_or(0) });
    }
    let truncated = result.children.iter().any(|c| c.local() == "IsTruncated" && c.text.trim() == "true");
    let next = result.children.iter().find(|c| c.local() == "NextContinuationToken").map(|c| c.text.trim().to_string()).filter(|t| !t.is_empty());
    Ok((objects, if truncated { next } else { None }))
}

/// The `Authorization` header for an S3 request (AWS Signature Version 4).
fn authorization(config: &StoreConfig, method: &str, path: &str, query: &[(&str, String)], headers: &[(String, String)], payload_hash: &str, amz_date: &str) -> String {
    let signer = Signer { access_key_id: &config.access_key_id, secret_access_key: &config.secret_access_key, region: &config.region, service: "s3" };
    signer.authorization(method, path, query, headers, payload_hash, amz_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> StoreConfig {
        // AWS's documented example credentials (Signature Version 4, "Examples of the complete Version 4 signing process").
        StoreConfig {
            endpoint: "https://s3.amazonaws.com".into(),
            bucket: "examplebucket".into(),
            region: "us-east-1".into(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    #[test]
    fn signs_get_object_as_aws_documents() {
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization(&example(), "GET", "/test.txt", &[], &headers, EMPTY_SHA256, "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn signs_list_objects_as_aws_documents() {
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let query = [("max-keys", "2".to_string()), ("prefix", "J".to_string())];
        let auth = authorization(&example(), "GET", "/", &query, &headers, EMPTY_SHA256, "20130524T000000Z");
        assert!(auth.ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{}", auth);
    }

    #[test]
    fn addresses_buckets_by_store() {
        let aws = ObjectStore::new(example()).unwrap();
        assert_eq!(aws.base, "https://examplebucket.s3.amazonaws.com");
        assert_eq!(aws.path("base/42/backup.json"), "/base/42/backup.json");
        let minio = ObjectStore::new(StoreConfig { endpoint: "http://127.0.0.1:9000/".into(), ..example() }).unwrap();
        assert_eq!(minio.base, "http://127.0.0.1:9000");
        assert_eq!(minio.path("a b/c+d"), "/examplebucket/a%20b/c%2Bd");
        assert!(ObjectStore::new(StoreConfig { endpoint: "s3.amazonaws.com".into(), ..example() }).is_err());
    }

    /// Against a real store, when HEXDB_TEST_S3_ENDPOINT names one, with HEXDB_TEST_S3_KEY and
    /// HEXDB_TEST_S3_SECRET (locally and in CI: `docker run -p 9000:9000 rustfs/rustfs`, key and
    /// secret rustfsadmin; MinIO's images are no longer published openly).
    #[tokio::test]
    async fn round_trips_through_a_real_store() {
        let Ok(endpoint) = std::env::var("HEXDB_TEST_S3_ENDPOINT") else { return };
        let store = ObjectStore::new(StoreConfig {
            endpoint,
            bucket: format!("hexdb-test-{}", ulid::Ulid::generate().to_string().to_ascii_lowercase()),
            region: "us-east-1".into(),
            access_key_id: std::env::var("HEXDB_TEST_S3_KEY").unwrap_or_else(|_| "minioadmin".into()),
            secret_access_key: std::env::var("HEXDB_TEST_S3_SECRET").unwrap_or_else(|_| "minioadmin".into()),
        })
        .unwrap();
        store.ensure_bucket().await.unwrap();
        store.ensure_bucket().await.unwrap();
        assert_eq!(store.get("missing").await.unwrap(), None);
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        store.put("wal/00000000000000000042.wal", big.clone()).await.unwrap();
        store.put("chunks/a b+c", b"odd key".to_vec()).await.unwrap();
        for i in 0..5 {
            store.put(&format!("many/{:03}", i), vec![i]).await.unwrap();
        }
        assert_eq!(store.get("wal/00000000000000000042.wal").await.unwrap().as_deref(), Some(&big[..]));
        assert_eq!(store.get("chunks/a b+c").await.unwrap().as_deref(), Some(&b"odd key"[..]));
        let listed = store.list("many/").await.unwrap();
        assert_eq!(listed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["many/000", "many/001", "many/002", "many/003", "many/004"]);
        assert_eq!(store.list("wal/").await.unwrap(), vec![ObjectInfo { key: "wal/00000000000000000042.wal".into(), size: 3_000_000 }]);
        store.delete("many/002").await.unwrap();
        store.delete("many/002").await.unwrap();
        assert_eq!(store.list("many/").await.unwrap().len(), 4);
    }

    #[test]
    fn reads_listing_pages() {
        let page = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>wal/</Prefix><KeyCount>2</KeyCount><IsTruncated>true</IsTruncated>
<Contents><Key>wal/1.wal</Key><Size>10</Size></Contents><Contents><Key>wal/7.wal</Key><Size>2048</Size></Contents>
<NextContinuationToken>abc==</NextContinuationToken></ListBucketResult>"#;
        let (objects, next) = parse_listing(page).unwrap();
        assert_eq!(objects, vec![ObjectInfo { key: "wal/1.wal".into(), size: 10 }, ObjectInfo { key: "wal/7.wal".into(), size: 2048 }]);
        assert_eq!(next.as_deref(), Some("abc=="));
        let last = br#"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        assert_eq!(parse_listing(last).unwrap(), (Vec::new(), None));
    }
}
