// Upload quotas: how much each user may keep in files tessellations.
//
// A user's usage is what their uploads keep stored: every file (in any files
// tessellation, and in the trash) whose current version they uploaded, with
// that file's earlier versions, in bytes and in files. Limits come from the
// user record (`quota_mb`, `quota_files`; 0 means no limit) or else the
// lattice configuration (`quotas.default_mb`, `quotas.default_files`; 0: no
// limit). Administrators get only an explicit limit of their own.
//
// The check runs when an upload is committed (`commit_file`), so nothing is
// stored past a limit, and before one starts when its size is known (a
// resumable upload's declared size, a PUT's Content-Length). Usage is
// counted from the documents (unrestricted by the user's own row filters:
// what they can't see still takes space) and kept for a minute per user, with
// each committed upload added to it.

use crate::engine::{DocumentQuery, EngineError, HexDBEngine};
use crate::filter::Filter;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CACHE_FOR: Duration = Duration::from_secs(60);

/// What a user's uploads keep stored.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct QuotaUsage {
    pub bytes: u64,
    pub files: u64,
}

/// Usage per user, counted recently.
#[derive(Default)]
pub struct QuotaCache(Mutex<HashMap<String, (Instant, QuotaUsage)>>);

/// A user's limits; none: no limit.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct QuotaLimits {
    pub bytes: Option<u64>,
    pub files: Option<u64>,
}

fn limit(value: Option<u64>) -> Option<u64> {
    value.filter(|v| *v > 0)
}

impl HexDBEngine {
    /// The limits that apply to `login`.
    pub async fn quota_limits(&self, login: &str) -> Result<QuotaLimits> {
        let user = crate::users::find_for_login(self, login).await?.map(|(_, u)| u);
        let admin = user.as_ref().is_some_and(|u| u.roles.iter().any(|r| r.name == crate::users::ADMIN_ROLE));
        let default = |key: &str| if admin { None } else { self.config_value(key).as_u64() };
        let mb = user.as_ref().and_then(|u| u.quota_mb).or_else(|| default("quotas.default_mb"));
        let files = user.as_ref().and_then(|u| u.quota_files).or_else(|| default("quotas.default_files"));
        Ok(QuotaLimits { bytes: limit(mb).map(|m| m.saturating_mul(1024 * 1024)), files: limit(files) })
    }

    /// What `login`'s uploads keep stored (counted at most a minute ago).
    pub async fn quota_usage(&self, login: &str) -> Result<QuotaUsage> {
        if let Some((at, usage)) = self.quota_cache.0.lock().unwrap().get(login) {
            if at.elapsed() < CACHE_FOR {
                return Ok(*usage);
            }
        }
        let mut usage = QuotaUsage::default();
        let filter = Filter::parse(&json!({ "uploaded_by": login }))?;
        let names: Vec<String> = self.tessellation_details().into_iter().map(|(name, _)| name).filter(|name| self.holds_files(name)).collect();
        for tess in names {
            let mut offset = 0;
            loop {
                let query = DocumentQuery { filter: filter.clone(), sort: vec![], offset, limit: 1000, after: None, with_total: false };
                let page = self.query_documents_inner(&tess, &query).await?;
                let count = page.documents.len();
                for doc in &page.documents {
                    let data = doc.data_json();
                    let earlier: u64 = data["versions"].as_array().into_iter().flatten().filter_map(|v| v["size"].as_u64()).sum();
                    usage.bytes += data["size"].as_u64().unwrap_or(0) + earlier;
                    usage.files += 1;
                }
                if count < 1000 {
                    break;
                }
                offset += count;
            }
        }
        self.quota_cache.0.lock().unwrap().insert(login.to_string(), (Instant::now(), usage));
        Ok(usage)
    }

    /// Refuse an upload of `bytes` (a new file when `new_file`) that would take `login` past a limit.
    pub async fn check_quota(&self, login: &str, bytes: u64, new_file: bool) -> Result<()> {
        // HexDB's own work (replication, restores by the system) has no quota.
        if login.is_empty() || login == "system" {
            return Ok(());
        }
        let limits = self.quota_limits(login).await?;
        if limits.bytes.is_none() && limits.files.is_none() {
            return Ok(());
        }
        let usage = self.quota_usage(login).await?;
        if let Some(max) = limits.bytes.filter(|max| usage.bytes.saturating_add(bytes) > *max) {
            return Err(EngineError::QuotaExceeded(format!(
                "This would take your files past your {} MB quota ({} MB used). Delete files or old versions, empty your trash, or ask an administrator for more.",
                max / (1024 * 1024),
                usage.bytes / (1024 * 1024)
            ))
            .into());
        }
        if let Some(max) = limits.files.filter(|max| new_file && usage.files >= *max) {
            return Err(EngineError::QuotaExceeded(format!("You have {} files, your quota. Delete some, or ask an administrator for more.", max)).into());
        }
        Ok(())
    }

    /// Add a committed upload to `login`'s counted usage.
    pub(crate) fn quota_add(&self, login: &str, bytes: u64, new_file: bool) {
        if let Some((_, usage)) = self.quota_cache.0.lock().unwrap().get_mut(login) {
            usage.bytes += bytes;
            usage.files += u64::from(new_file);
        }
    }

    /// Forget counted usage (after deletes and purges, which free space).
    pub(crate) fn quota_forget(&self) {
        self.quota_cache.0.lock().unwrap().clear();
    }

    /// `login`'s limits and usage, for display.
    pub async fn quota_view(&self, login: &str) -> Result<Value> {
        let limits = self.quota_limits(login).await?;
        let usage = if limits.bytes.is_some() || limits.files.is_some() { self.quota_usage(login).await? } else { QuotaUsage::default() };
        Ok(json!({ "limit_bytes": limits.bytes, "limit_files": limits.files, "used_bytes": usage.bytes, "used_files": usage.files }))
    }
}
