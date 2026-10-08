// HexDB Core Functions and Schedules
//
// A function is something saved on the server and run by name with
// parameters:
//
//   query        a filtered, sorted query on a tessellation
//   aggregate    an aggregation on a tessellation
//   transaction  operations across tessellations, applied atomically
//   script       Python, TypeScript or JavaScript run in a separate process
//
// Parameters are declared (`params`) and referenced in the definition as
// `{"$param": "name"}`; HexDB substitutes the values, checking their types.
// A function always runs with the permissions of whoever runs it: a query
// needs read access to its tessellation, a transaction the access its
// operations need. A script gets a short-lived session of that user
// (HEXDB_TOKEN, revoked when the script ends) and HEXDB_API, reads
// `{"params", "function", "caller"}` as JSON on stdin, and prints its result
// as JSON on stdout; it is stopped after `timeout_seconds`.
//
// A schedule runs a function every N seconds or on a cron expression (UTC:
// minute hour day-of-month month day-of-week, or @hourly, @daily, @weekly,
// @monthly), as the user who created the schedule. Schedules run on the
// Overseer; each records its last run and result.
//
// Only administrators create, change or delete functions and schedules
// (scripts run code on the server). Anyone signed in can run a function.

use crate::{
    auth::{Credential, Permission, Principal},
    engine::{DocumentQuery, EngineError, HexDBEngine},
    filter::{Filter, SortKey},
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tracing::{info, warn};
use ulid::Ulid;

pub const FUNCTIONS_TESSELLATION: &str = "_functions";
pub const SCHEDULES_TESSELLATION: &str = "_schedules";
/// Largest script output kept (stdout), in bytes.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// Script runtimes and switches (`[functions]` in hexdb.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionsConfig {
    /// Allow script functions at all.
    #[serde(default = "yes")]
    pub scripts: bool,
    /// The Python interpreter.
    #[serde(default = "default_python")]
    pub python: String,
    /// Node.js, for JavaScript and TypeScript (22.6 or later for TypeScript).
    #[serde(default = "default_node")]
    pub node: String,
}

fn yes() -> bool {
    true
}

fn default_python() -> String {
    if cfg!(windows) { "python".into() } else { "python3".into() }
}

fn default_node() -> String {
    "node".into()
}

impl Default for FunctionsConfig {
    fn default() -> Self {
        FunctionsConfig { scripts: true, python: default_python(), node: default_node() }
    }
}

/// A declared parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamDef {
    pub name: String,
    /// string, number, integer, boolean, object, array or any.
    #[serde(rename = "type", default = "any")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

fn any() -> String {
    "any".into()
}

/// A saved function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// query, aggregate, transaction or script.
    pub kind: String,
    #[serde(default)]
    pub params: Vec<ParamDef>,
    /// query and aggregate: the tessellation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tessellation: Option<String>,
    /// query: {"filter", "sort", "limit", "offset", "fields"};
    /// aggregate: an aggregation ({"filter", "group_by", "aggregates", "sort", "limit"});
    /// transaction: {"operations": [...]}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// script: python, typescript or javascript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub updated: i64,
}

fn default_timeout() -> u64 {
    30
}

/// A saved schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDef {
    #[serde(default)]
    pub name: String,
    pub function: String,
    #[serde(default)]
    pub params: Map<String, Value>,
    /// Run every this many seconds (at least 10)...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_seconds: Option<u64>,
    /// ...or on this cron expression (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The user it runs as (the creator).
    #[serde(default)]
    pub run_as: String,
    #[serde(default)]
    pub run_as_login: String,
    #[serde(default)]
    pub next_run: i64,
    #[serde(default)]
    pub last_run: i64,
    /// ok or error.
    #[serde(default)]
    pub last_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_duration_ms: u64,
    #[serde(default)]
    pub runs: u64,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn doc_id(kind: &str, name: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 function id v1", format!("{}\u{0}{}", kind, name).as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

fn validate_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)) {
        bail!(invalid(format!("{} names are 1-64 letters, digits, '_' or '-'.", what)));
    }
    Ok(())
}

fn type_ok(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => true,
    }
}

impl FunctionDef {
    fn validate(&self) -> Result<()> {
        validate_name(&self.name, "Function")?;
        let mut names = BTreeSet::new();
        for p in &self.params {
            validate_name(&p.name, "Parameter")?;
            if !names.insert(&p.name) {
                bail!(invalid(format!("Parameter '{}' is declared twice.", p.name)));
            }
            if !["string", "number", "integer", "boolean", "object", "array", "any"].contains(&p.kind.as_str()) {
                bail!(invalid(format!("Parameter '{}': unknown type '{}'.", p.name, p.kind)));
            }
        }
        match self.kind.as_str() {
            "query" | "aggregate" => {
                let tess = self.tessellation.as_deref().ok_or_else(|| invalid("A query or aggregate function needs a tessellation."))?;
                crate::catalog::validate_tessellation_name(tess).map_err(|e| invalid(e.to_string()))?;
                // The body must parse once parameters are filled with placeholders' defaults or nulls.
                let sample = self.sample_params();
                let body = substitute(self.body.as_ref().unwrap_or(&json!({})), &sample)?;
                if self.kind == "query" {
                    query_from(&body)?;
                } else {
                    crate::aggregate::Aggregation::from_json(&body)?;
                }
            }
            "transaction" => {
                let body = substitute(self.body.as_ref().ok_or_else(|| invalid("A transaction function needs a body with operations."))?, &self.sample_params())?;
                crate::engine::parse_transaction(&body)?;
            }
            "script" => {
                match self.runtime.as_deref() {
                    Some("python" | "typescript" | "javascript") => {}
                    _ => bail!(invalid("A script needs runtime: python, typescript or javascript.")),
                }
                if self.code.as_deref().is_none_or(|c| c.trim().is_empty()) {
                    bail!(invalid("A script needs code."));
                }
                if self.code.as_deref().is_some_and(|c| c.len() > 1_000_000) {
                    bail!(invalid("Script code must be at most 1 MB."));
                }
            }
            other => bail!(invalid(format!("kind must be query, aggregate, transaction or script, not '{}'.", other))),
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > 600 {
            bail!(invalid("timeout_seconds must be 1-600."));
        }
        Ok(())
    }

    /// Placeholder values for validation: defaults, else a value of the right type.
    fn sample_params(&self) -> Map<String, Value> {
        self.params
            .iter()
            .map(|p| {
                let value = p.default.clone().unwrap_or(match p.kind.as_str() {
                    "string" => json!(""),
                    "number" | "integer" => json!(0),
                    "boolean" => json!(false),
                    "object" => json!({}),
                    "array" => json!([]),
                    _ => Value::Null,
                });
                (p.name.clone(), value)
            })
            .collect()
    }

    /// Check arguments against the declared parameters; fills in defaults.
    pub fn bind(&self, args: &Map<String, Value>) -> Result<Map<String, Value>> {
        if let Some(unknown) = args.keys().find(|k| !self.params.iter().any(|p| &p.name == *k)) {
            bail!(invalid(format!("'{}' isn't a parameter of '{}'.", unknown, self.name)));
        }
        let mut out = Map::new();
        for p in &self.params {
            match args.get(&p.name).cloned().or_else(|| p.default.clone()) {
                Some(value) => {
                    if !type_ok(&p.kind, &value) {
                        bail!(invalid(format!("Parameter '{}' must be {}.", p.name, p.kind)));
                    }
                    out.insert(p.name.clone(), value);
                }
                None if p.required => bail!(invalid(format!("Parameter '{}' is required.", p.name))),
                None => {
                    out.insert(p.name.clone(), Value::Null);
                }
            }
        }
        Ok(out)
    }
}

/// Replace every `{"$param": "name"}` in `template` with the parameter's value.
pub fn substitute(template: &Value, params: &Map<String, Value>) -> Result<Value> {
    Ok(match template {
        Value::Object(map) if map.len() == 1 && map.contains_key("$param") => {
            let name = map["$param"].as_str().ok_or_else(|| invalid("$param must name a parameter."))?;
            params.get(name).cloned().ok_or_else(|| invalid(format!("Unknown parameter '{}'.", name)))?
        }
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| Ok((k.clone(), substitute(v, params)?))).collect::<Result<_>>()?),
        Value::Array(items) => Value::Array(items.iter().map(|v| substitute(v, params)).collect::<Result<_>>()?),
        other => other.clone(),
    })
}

fn query_from(body: &Value) -> Result<(DocumentQuery, Vec<String>)> {
    let filter = Filter::parse(body.get("filter").unwrap_or(&Value::Null))?;
    let sort: Vec<SortKey> = match body.get("sort") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| SortKey { field: p.trim_start_matches(['-', '+']).to_string(), descending: p.starts_with('-') })
            .collect(),
        Some(other) => serde_json::from_value(other.clone()).map_err(|e| invalid(format!("sort: {}", e)))?,
    };
    let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(100).clamp(1, 1000) as usize;
    let offset = body.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let fields: Vec<String> = body.get("fields").and_then(Value::as_array).map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect()).unwrap_or_default();
    let with_total = body.get("total").and_then(Value::as_bool).unwrap_or(true);
    Ok((DocumentQuery { filter, sort, offset, limit, after: None, with_total }, fields))
}

// ---------------------------------------------------------------------------
// Cron
// ---------------------------------------------------------------------------

/// A parsed cron expression (UTC).
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days: BTreeSet<u32>,
    months: BTreeSet<u32>,
    weekdays: BTreeSet<u32>,
    any_day: bool,
    any_weekday: bool,
}

fn cron_field(text: &str, min: u32, max: u32) -> Result<BTreeSet<u32>> {
    let mut out = BTreeSet::new();
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| anyhow!("bad step in '{}'", part))?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step can't be 0 in '{}'", part);
        }
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| anyhow!("bad range '{}'", part))?, b.parse().map_err(|_| anyhow!("bad range '{}'", part))?)
        } else {
            let v: u32 = range.parse().map_err(|_| anyhow!("bad value '{}'", part))?;
            (v, if part.contains('/') { max } else { v })
        };
        if start < min || end > max || start > end {
            bail!("'{}' is outside {}-{}", part, min, max);
        }
        out.extend((start..=end).step_by(step as usize));
    }
    Ok(out)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Cron> {
        let expanded = match text.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            bail!(invalid("cron needs 5 fields: minute hour day-of-month month day-of-week (or @hourly, @daily, @weekly, @monthly)."));
        }
        let wrap = |e: anyhow::Error| invalid(format!("cron '{}': {}", text, e));
        let mut weekdays = cron_field(fields[4], 0, 7).map_err(wrap)?;
        if weekdays.remove(&7) {
            weekdays.insert(0);
        }
        Ok(Cron {
            minutes: cron_field(fields[0], 0, 59).map_err(wrap)?,
            hours: cron_field(fields[1], 0, 23).map_err(wrap)?,
            days: cron_field(fields[2], 1, 31).map_err(wrap)?,
            months: cron_field(fields[3], 1, 12).map_err(wrap)?,
            weekdays,
            any_day: fields[2] == "*",
            any_weekday: fields[4] == "*",
        })
    }

    fn day_matches(&self, date: DateTime<Utc>) -> bool {
        let dom = self.days.contains(&date.day());
        let dow = self.weekdays.contains(&date.weekday().num_days_from_sunday());
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            // Both restricted: either may match (standard cron).
            (false, false) => dom || dow,
        }
    }

    /// The first time strictly after `after` that matches (within 5 years).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = after.with_second(0)?.with_nanosecond(0)? + ChronoDuration::minutes(1);
        let mut day = Utc.with_ymd_and_hms(start.year(), start.month(), start.day(), 0, 0, 0).single()?;
        for _ in 0..(366 * 5) {
            if self.months.contains(&day.month()) && self.day_matches(day) {
                for &h in &self.hours {
                    for &m in &self.minutes {
                        let t = day.with_hour(h)?.with_minute(m)?;
                        if t >= start {
                            return Some(t);
                        }
                    }
                }
            }
            day += ChronoDuration::days(1);
        }
        None
    }
}

impl ScheduleDef {
    fn validate(&self) -> Result<()> {
        validate_name(&self.name, "Schedule")?;
        match (&self.every_seconds, &self.cron) {
            (Some(s), None) if *s >= 10 => Ok(()),
            (Some(_), None) => Err(invalid("every_seconds must be at least 10.")),
            (None, Some(c)) => Cron::parse(c).map(|_| ()),
            _ => Err(invalid("Give either every_seconds or cron.")),
        }
    }

    /// When it runs next, after `now`.
    pub fn next_after(&self, now: DateTime<Utc>) -> i64 {
        match (&self.every_seconds, &self.cron) {
            (Some(s), _) => now.timestamp_millis() + *s as i64 * 1000,
            (None, Some(c)) => Cron::parse(c).ok().and_then(|c| c.next_after(now)).map(|t| t.timestamp_millis()).unwrap_or(i64::MAX),
            _ => i64::MAX,
        }
    }
}

// ---------------------------------------------------------------------------
// Storage and running
// ---------------------------------------------------------------------------

impl HexDBEngine {
    pub async fn list_functions(&self) -> Result<Vec<FunctionDef>> {
        if !self.tessellation_exists(FUNCTIONS_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self.list_documents(FUNCTIONS_TESSELLATION, None, usize::MAX).await?;
        let mut list: Vec<FunctionDef> = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    pub async fn get_function(&self, name: &str) -> Result<Option<FunctionDef>> {
        let Some(doc) = self.get_system_document(FUNCTIONS_TESSELLATION, doc_id("function", name)).await? else { return Ok(None) };
        Ok(serde_json::from_value(doc.data_json()).ok())
    }

    /// Create (`create`) or replace a function.
    pub async fn save_function(&self, mut def: FunctionDef, by: &str, create: bool) -> Result<FunctionDef> {
        def.validate()?;
        if def.kind == "script" && !self.config.functions.scripts {
            bail!(invalid("Script functions are turned off (functions.scripts = false)."));
        }
        let exists = self.get_function(&def.name).await?.is_some();
        if create && exists {
            bail!(EngineError::Conflict(format!("A function named '{}' already exists.", def.name)));
        }
        if !create && !exists {
            bail!(EngineError::NotFound(format!("Function '{}' not found.", def.name)));
        }
        def.created_by = by.to_string();
        def.updated = Utc::now().timestamp_millis();
        self.put_system_document(FUNCTIONS_TESSELLATION, doc_id("function", &def.name), serde_json::to_value(&def)?, None).await?;
        Ok(def)
    }

    pub async fn delete_function(&self, name: &str) -> Result<bool> {
        if self.get_function(name).await?.is_none() {
            return Ok(false);
        }
        if let Some(schedule) = self.list_schedules().await?.into_iter().find(|s| s.function == name) {
            bail!(EngineError::Conflict(format!("Schedule '{}' runs this function; delete it first.", schedule.name)));
        }
        self.delete_system_document(FUNCTIONS_TESSELLATION, doc_id("function", name)).await?;
        Ok(true)
    }

    /// Run a function as `principal` with these arguments.
    pub async fn run_function(&self, def: &FunctionDef, principal: &Principal, args: &Map<String, Value>) -> Result<Value> {
        let params = def.bind(args)?;
        let body = substitute(def.body.as_ref().unwrap_or(&json!({})), &params)?;
        match def.kind.as_str() {
            "query" => {
                let tess = def.tessellation.clone().unwrap_or_default();
                principal.require(Permission::Read, &tess)?;
                let (query, fields) = query_from(&body)?;
                let page = self.query_documents(&tess, &query).await?;
                let documents: Vec<Value> = page
                    .documents
                    .iter()
                    .map(|d| if fields.is_empty() { d.to_api_json() } else { crate::document::project(&d.to_api_json(), &fields) })
                    .collect();
                Ok(json!({ "documents": documents, "total": page.total }))
            }
            "aggregate" => {
                let tess = def.tessellation.clone().unwrap_or_default();
                principal.require(Permission::Read, &tess)?;
                let aggregation = crate::aggregate::Aggregation::from_json(&body)?;
                Ok(serde_json::to_value(self.aggregate(&tess, &aggregation).await?)?)
            }
            "transaction" => {
                let ops = crate::engine::parse_transaction(&body)?;
                for op in &ops {
                    let needed = match op.kind {
                        crate::engine::TxOpKind::Get | crate::engine::TxOpKind::Check => Permission::Read,
                        _ => Permission::Write,
                    };
                    principal.require(needed, &op.tessellation)?;
                }
                Ok(serde_json::to_value(self.transaction(&ops, None).await?.value)?)
            }
            "script" => self.run_script(def, principal, &params).await,
            _ => Err(invalid("unknown function kind")),
        }
    }

    async fn run_script(&self, def: &FunctionDef, principal: &Principal, params: &Map<String, Value>) -> Result<Value> {
        if !self.config.functions.scripts {
            bail!(invalid("Script functions are turned off (functions.scripts = false)."));
        }
        let runtime = def.runtime.as_deref().unwrap_or_default();
        let code = def.code.clone().unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("hexdb-fn-{}", Ulid::new()));
        std::fs::create_dir_all(&dir)?;
        let (program, args, file) = match runtime {
            "python" => (self.config.functions.python.clone(), vec![], "main.py"),
            "typescript" => (self.config.functions.node.clone(), vec!["--experimental-strip-types".to_string(), "--no-warnings".to_string()], "main.mts"),
            _ => (self.config.functions.node.clone(), vec![], "main.mjs"),
        };
        std::fs::write(dir.join(file), code)?;
        // A session for the caller, only for this run.
        let (token, claims) = crate::auth::issue_session(&self.config.session_key()?, &principal.user_id, 1);
        let scheme = if self.config.tls.enabled() { "https" } else { "http" };
        let mut command = tokio::process::Command::new(&program);
        command.args(&args).arg(file).current_dir(&dir).env_clear();
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "TEMP", "TMP", "TMPDIR", "HOME", "USERPROFILE", "LANG", "TZ"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HEXDB_API", format!("{}://{}", scheme, self.config.network.api_endpoint.replace("0.0.0.0", "127.0.0.1")))
            .env("HEXDB_TOKEN", &token)
            .env("HEXDB_FUNCTION", &def.name)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if !self.config.tls.ca_file.is_empty() {
            command.env("NODE_EXTRA_CA_CERTS", &self.config.tls.ca_file).env("REQUESTS_CA_BUNDLE", &self.config.tls.ca_file).env("SSL_CERT_FILE", &self.config.tls.ca_file);
        }
        crate::process::contain(&mut command);
        let started = std::time::Instant::now();
        let result = async {
            let mut child = command.spawn().with_context(|| format!("couldn't start {} (set functions.{} in hexdb.toml)", program, if runtime == "python" { "python" } else { "node" }))?;
            crate::process::adopt(&child);
            let input = json!({ "params": params, "function": def.name, "caller": principal.login });
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input.to_string().as_bytes()).await?;
            }
            let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
            let mut stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
            let read_out = async {
                let mut buf = Vec::new();
                (&mut stdout).take(MAX_OUTPUT as u64).read_to_end(&mut buf).await.map(|_| buf)
            };
            let read_err = async {
                let mut buf = Vec::new();
                (&mut stderr).take(64 * 1024).read_to_end(&mut buf).await.map(|_| buf)
            };
            let timeout = Duration::from_secs(def.timeout_seconds);
            let finished = tokio::time::timeout(timeout, async { tokio::join!(child.wait(), read_out, read_err) }).await;
            let Ok((status, out, err)) = finished else {
                let _ = child.kill().await;
                bail!(invalid(format!("The script didn't finish within {} seconds.", def.timeout_seconds)));
            };
            let (status, out, err) = (status?, out?, err?);
            let stderr_text = String::from_utf8_lossy(&err).trim().to_string();
            if !stderr_text.is_empty() {
                info!(target: "hexdb_core::functions", function = %def.name, "[{}] {}", def.name, stderr_text.chars().take(2000).collect::<String>());
            }
            if !status.success() {
                bail!(invalid(format!(
                    "The script exited with {}{}",
                    status,
                    if stderr_text.is_empty() { String::new() } else { format!(": {}", stderr_text.lines().last().unwrap_or_default()) }
                )));
            }
            let text = String::from_utf8_lossy(&out).trim().to_string();
            Ok(serde_json::from_str::<Value>(&text).unwrap_or(json!({ "output": text })))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = self.revoke_script_session(&claims.sid, claims.exp).await {
            warn!("⚠️ Couldn't revoke the session of script '{}': {:#}", def.name, e);
        }
        info!(target: "hexdb_core::functions", "⚙️ Script '{}' run by '{}' in {} ms.", def.name, principal.login, started.elapsed().as_millis());
        result
    }

    async fn revoke_script_session(&self, sid: &str, exp: i64) -> Result<()> {
        if self.is_writable() {
            crate::auth::revoke_session(self, sid, exp).await
        } else {
            crate::replication::post_to_overseer(self, "/lattice/revoke", &json!({ "session_id": sid, "expires_at": exp })).await
        }
    }

    // --- schedules ---------------------------------------------------------

    pub async fn list_schedules(&self) -> Result<Vec<ScheduleDef>> {
        if !self.tessellation_exists(SCHEDULES_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self.list_documents(SCHEDULES_TESSELLATION, None, usize::MAX).await?;
        let mut list: Vec<ScheduleDef> = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    pub async fn get_schedule(&self, name: &str) -> Result<Option<ScheduleDef>> {
        let Some(doc) = self.get_system_document(SCHEDULES_TESSELLATION, doc_id("schedule", name)).await? else { return Ok(None) };
        Ok(serde_json::from_value(doc.data_json()).ok())
    }

    async fn put_schedule(&self, schedule: &ScheduleDef) -> Result<()> {
        self.put_system_document(SCHEDULES_TESSELLATION, doc_id("schedule", &schedule.name), serde_json::to_value(schedule)?, None).await
    }

    /// Create or replace a schedule; it runs as `owner`.
    pub async fn save_schedule(&self, mut schedule: ScheduleDef, owner: &Principal, create: bool) -> Result<ScheduleDef> {
        schedule.validate()?;
        if self.get_function(&schedule.function).await?.is_none() {
            bail!(EngineError::NotFound(format!("Function '{}' not found.", schedule.function)));
        }
        let existing = self.get_schedule(&schedule.name).await?;
        match (&existing, create) {
            (Some(_), true) => bail!(EngineError::Conflict(format!("A schedule named '{}' already exists.", schedule.name))),
            (None, false) => bail!(EngineError::NotFound(format!("Schedule '{}' not found.", schedule.name))),
            (Some(old), false) => {
                schedule.last_run = old.last_run;
                schedule.last_status = old.last_status.clone();
                schedule.last_error = old.last_error.clone();
                schedule.runs = old.runs;
            }
            _ => {}
        }
        schedule.run_as = owner.user_id.clone();
        schedule.run_as_login = owner.login.clone();
        schedule.next_run = schedule.next_after(Utc::now());
        self.put_schedule(&schedule).await?;
        Ok(schedule)
    }

    pub async fn delete_schedule(&self, name: &str) -> Result<bool> {
        if self.get_schedule(name).await?.is_none() {
            return Ok(false);
        }
        self.delete_system_document(SCHEDULES_TESSELLATION, doc_id("schedule", name)).await?;
        Ok(true)
    }

    /// Run a schedule now (as its owner) and record the result.
    pub async fn run_schedule(&self, mut schedule: ScheduleDef) -> Result<Value> {
        let started = std::time::Instant::now();
        let result = async {
            let def = self.get_function(&schedule.function).await?.ok_or_else(|| anyhow!("function '{}' no longer exists", schedule.function))?;
            let (_, user) = crate::users::find_by_id(self, &schedule.run_as).await?.ok_or_else(|| anyhow!("its owner '{}' no longer exists", schedule.run_as_login))?;
            if user.is_locked {
                bail!("its owner '{}' is locked", user.login);
            }
            let definitions = crate::users::role_definitions(self).await?;
            let principal = Principal::new(schedule.run_as.clone(), user.login, user.email_address, user.roles, Credential::ApiKey { key_id: format!("schedule:{}", schedule.name) }, &definitions);
            self.run_function(&def, &principal, &schedule.params).await
        }
        .await;
        schedule.last_run = Utc::now().timestamp_millis();
        schedule.last_duration_ms = started.elapsed().as_millis() as u64;
        schedule.runs += 1;
        schedule.next_run = schedule.next_after(Utc::now());
        match &result {
            Ok(_) => {
                schedule.last_status = "ok".into();
                schedule.last_error = None;
            }
            Err(e) => {
                schedule.last_status = "error".into();
                schedule.last_error = Some(format!("{:#}", e).chars().take(1000).collect());
                warn!("⚠️ Schedule '{}' failed: {:#}", schedule.name, e);
            }
        }
        // The definition may have changed or gone while it ran.
        if let Some(current) = self.get_schedule(&schedule.name).await? {
            let mut updated = current;
            updated.last_run = schedule.last_run;
            updated.last_duration_ms = schedule.last_duration_ms;
            updated.runs = schedule.runs;
            updated.next_run = updated.next_after(Utc::now());
            updated.last_status = schedule.last_status.clone();
            updated.last_error = schedule.last_error.clone();
            self.put_schedule(&updated).await?;
        }
        result
    }
}

/// Run due schedules on the Overseer.
pub fn spawn_scheduler(engine: Arc<HexDBEngine>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        let running: Arc<std::sync::Mutex<BTreeSet<String>>> = Default::default();
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            if !engine.is_writable() {
                continue;
            }
            let Ok(schedules) = engine.list_schedules().await else { continue };
            let now = Utc::now().timestamp_millis();
            for schedule in schedules.into_iter().filter(|s| s.enabled && s.next_run <= now) {
                if !running.lock().unwrap().insert(schedule.name.clone()) {
                    continue;
                }
                let (engine, running) = (engine.clone(), running.clone());
                tokio::spawn(async move {
                    let name = schedule.name.clone();
                    let _ = engine.run_schedule(schedule).await;
                    running.lock().unwrap().remove(&name);
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_substituted_and_checked() {
        let def: FunctionDef = serde_json::from_value(json!({
            "name": "by_status", "kind": "query", "tessellation": "orders",
            "params": [{ "name": "status", "type": "string", "required": true }, { "name": "limit", "type": "integer", "default": 5 }],
            "body": { "filter": { "status": { "$param": "status" } }, "limit": { "$param": "limit" } },
        }))
        .unwrap();
        def.validate().unwrap();
        let bound = def.bind(json!({ "status": "paid" }).as_object().unwrap()).unwrap();
        assert_eq!(substitute(def.body.as_ref().unwrap(), &bound).unwrap(), json!({ "filter": { "status": "paid" }, "limit": 5 }));
        assert!(def.bind(json!({}).as_object().unwrap()).is_err(), "required");
        assert!(def.bind(json!({ "status": 1 }).as_object().unwrap()).is_err(), "typed");
        assert!(def.bind(json!({ "status": "x", "nope": 1 }).as_object().unwrap()).is_err(), "unknown");
    }

    #[test]
    fn cron_finds_the_next_time() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let every15 = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(every15.next_after(t("2026-01-01T10:07:30Z")), Some(t("2026-01-01T10:15:00Z")));
        let weekdays9 = Cron::parse("0 9 * * 1-5").unwrap();
        // 2026-01-03 is a Saturday.
        assert_eq!(weekdays9.next_after(t("2026-01-02T09:00:00Z")), Some(t("2026-01-05T09:00:00Z")));
        assert_eq!(Cron::parse("@daily").unwrap().next_after(t("2026-01-01T00:00:00Z")), Some(t("2026-01-02T00:00:00Z")));
        assert_eq!(Cron::parse("0 0 29 2 *").unwrap().next_after(t("2026-03-01T00:00:00Z")), Some(t("2028-02-29T00:00:00Z")));
        assert!(Cron::parse("60 * * * *").is_err());
        assert!(Cron::parse("* * *").is_err());
    }
}
