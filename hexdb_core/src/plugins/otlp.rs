// The built-in OpenTelemetry exporter (`builtin = "otlp"`): metrics samples
// or log records sent to an OTLP/HTTP endpoint as JSON (`/v1/metrics`,
// `/v1/logs`). Grafana (Alloy/Mimir/Loki), Datadog, Dynatrace, Honeycomb, New
// Relic and the OpenTelemetry Collector all accept OTLP.

use crate::{engine::HexDBEngine, logging::LogRecord, metrics::MetricsSample};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct OtlpConfig {
    /// Base URL of the OTLP/HTTP receiver, e.g. `http://localhost:4318`.
    pub endpoint: String,
    /// Extra headers (API keys for hosted backends).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_service")]
    pub service_name: String,
}

fn default_service() -> String {
    "hexdb".into()
}

fn attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn nanos(t: chrono::DateTime<chrono::Utc>) -> String {
    t.timestamp_nanos_opt().unwrap_or_default().to_string()
}

fn resource(engine: &HexDBEngine, config: &OtlpConfig) -> Value {
    json!({ "attributes": [
        attr("service.name", &config.service_name),
        attr("service.instance.id", &engine.id.to_string()),
        attr("hexdb.hex", &engine.name),
        attr("hexdb.lattice", &engine.config.network.lattice_name),
        attr("hexdb.role", &engine.role()),
    ]})
}

fn scope(engine: &HexDBEngine) -> Value {
    json!({ "name": "hexdb", "version": engine.version })
}

fn gauge(name: &str, unit: &str, description: &str, points: Vec<Value>) -> Value {
    json!({ "name": name, "unit": unit, "description": description, "gauge": { "dataPoints": points } })
}

fn counter(name: &str, unit: &str, description: &str, points: Vec<Value>) -> Value {
    // Cumulative, monotonic (aggregationTemporality 2 = cumulative).
    json!({ "name": name, "unit": unit, "description": description, "sum": { "aggregationTemporality": 2, "isMonotonic": true, "dataPoints": points } })
}

/// OTLP metrics for some samples.
pub fn metrics_body(engine: &HexDBEngine, config: &OtlpConfig, samples: &[MetricsSample]) -> Value {
    let start = nanos(engine.start_datetime);
    let point = |t: &str, v: u64| json!({ "timeUnixNano": t, "asInt": v.to_string() });
    let sum_point = |t: &str, v: u64| json!({ "startTimeUnixNano": start, "timeUnixNano": t, "asInt": v.to_string() });
    let mut documents = Vec::new();
    let (mut memory, mut disk, mut unflushed, mut reads, mut writes, mut queries) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    for s in samples {
        let t = nanos(s.timestamp);
        for (tess, count) in &s.documents {
            documents.push(json!({ "timeUnixNano": t, "asInt": count.to_string(), "attributes": [attr("tessellation", tess)] }));
        }
        memory.push(point(&t, s.memory_bytes as u64));
        disk.push(point(&t, s.disk_bytes));
        unflushed.push(point(&t, s.unflushed_entries as u64));
        reads.push(sum_point(&t, s.reads_total));
        writes.push(sum_point(&t, s.writes_total));
        queries.push(sum_point(&t, s.queries_total));
    }
    json!({ "resourceMetrics": [{
        "resource": resource(engine, config),
        "scopeMetrics": [{
            "scope": scope(engine),
            "metrics": [
                gauge("hexdb.documents", "{document}", "Visible documents per tessellation.", documents),
                gauge("hexdb.memory.usage", "By", "Memory used by documents.", memory),
                gauge("hexdb.disk.usage", "By", "Disk used by SSTables.", disk),
                gauge("hexdb.unflushed", "{entry}", "Writes not yet flushed to SSTables.", unflushed),
                counter("hexdb.reads", "{operation}", "Document reads since startup.", reads),
                counter("hexdb.writes", "{operation}", "Document writes since startup.", writes),
                counter("hexdb.queries", "{operation}", "Queries since startup.", queries),
            ],
        }],
    }]})
}

fn severity(level: &str) -> u8 {
    match level {
        "TRACE" => 1,
        "DEBUG" => 5,
        "INFO" => 9,
        "WARN" => 13,
        "ERROR" => 17,
        _ => 0,
    }
}

/// OTLP logs for some log records.
pub fn logs_body(engine: &HexDBEngine, config: &OtlpConfig, records: &[LogRecord]) -> Value {
    let log_records: Vec<Value> = records
        .iter()
        .map(|r| {
            let mut attributes = vec![attr("log.target", &r.target), attr("hexdb.seq", &r.seq.to_string())];
            attributes.extend(r.fields.iter().map(|(k, v)| attr(k, v)));
            json!({
                "timeUnixNano": nanos(r.timestamp),
                "observedTimeUnixNano": nanos(r.timestamp),
                "severityNumber": severity(&r.level),
                "severityText": r.level,
                "body": { "stringValue": r.message },
                "attributes": attributes,
            })
        })
        .collect();
    json!({ "resourceLogs": [{
        "resource": resource(engine, config),
        "scopeLogs": [{ "scope": scope(engine), "logRecords": log_records }],
    }]})
}

/// POST one OTLP payload.
pub async fn send(client: &reqwest::Client, config: &OtlpConfig, signal: &str, body: &Value) -> Result<()> {
    let url = format!("{}/v1/{}", config.endpoint.trim_end_matches('/'), signal);
    let mut request = client.post(&url).json(body);
    for (k, v) in &config.headers {
        request = request.header(k, v);
    }
    let response = request.send().await.with_context(|| format!("POST {}", url))?;
    if !response.status().is_success() {
        bail!("POST {} returned {}", url, response.status());
    }
    Ok(())
}
