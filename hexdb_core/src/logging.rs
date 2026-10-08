// HexDB Core Logging
// Log records go to stdout (pretty, for operators) and into an in-memory ring
// buffer (structured, for the /logs API and the admin UI's Logs page).

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// How many recent log records the server keeps in memory.
pub const LOG_CAPACITY: usize = 5_000;

/// One captured log record.
#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    /// Monotonic per-process sequence number; use it to page and to poll for new records.
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    /// ERROR, WARN, INFO, DEBUG, or TRACE.
    pub level: String,
    /// Module that emitted the record, e.g. `hexdb_core::network::discovery`.
    pub target: String,
    pub message: String,
    /// Structured fields other than the message.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

/// Filters for reading the log buffer.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Minimum severity (e.g. WARN returns WARN and ERROR).
    pub level: Option<Level>,
    /// Only records with `seq` greater than this (tailing).
    pub after: Option<u64>,
    /// Only records with `seq` less than this (paging backwards).
    pub before: Option<u64>,
    /// Case-insensitive substring of the message, target, or a field value.
    pub search: Option<String>,
    /// Prefix of the target module.
    pub target: Option<String>,
    /// Maximum records to return.
    pub limit: usize,
}

/// Ring buffer of recent log records.
pub struct LogBuffer {
    inner: Mutex<LogBufferInner>,
}

struct LogBufferInner {
    records: VecDeque<LogRecord>,
    next_seq: u64,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        LogBuffer {
            inner: Mutex::new(LogBufferInner {
                records: VecDeque::with_capacity(capacity.min(1024)),
                next_seq: 1,
                capacity: capacity.max(1),
            }),
        }
    }

    pub fn push(&self, timestamp: DateTime<Utc>, level: &Level, target: &str, message: String, fields: BTreeMap<String, String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.records.len() == inner.capacity {
            inner.records.pop_front();
        }
        inner.records.push_back(LogRecord {
            seq,
            timestamp,
            level: level.as_str().to_string(),
            target: target.to_string(),
            message,
            fields,
        });
    }

    /// Records matching `query`, oldest first. With `after` set, the oldest
    /// matches after it are returned (tailing); otherwise the newest matches.
    pub fn query(&self, query: &LogQuery) -> Vec<LogRecord> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let needle = query.search.as_deref().map(str::to_lowercase).filter(|s| !s.is_empty());
        let limit = query.limit.max(1);
        let matches = |r: &&LogRecord| -> bool {
            if query.after.is_some_and(|after| r.seq <= after) {
                return false;
            }
            if query.before.is_some_and(|before| r.seq >= before) {
                return false;
            }
            if let Some(min) = query.level {
                // tracing orders levels by verbosity: ERROR < WARN < ... < TRACE.
                match r.level.parse::<Level>() {
                    Ok(level) if level <= min => {}
                    _ => return false,
                }
            }
            if let Some(prefix) = &query.target {
                if !r.target.starts_with(prefix.as_str()) {
                    return false;
                }
            }
            if let Some(needle) = &needle {
                let hit = r.message.to_lowercase().contains(needle)
                    || r.target.to_lowercase().contains(needle)
                    || r.fields.values().any(|v| v.to_lowercase().contains(needle));
                if !hit {
                    return false;
                }
            }
            true
        };
        if query.after.is_some() {
            inner.records.iter().filter(matches).take(limit).cloned().collect()
        } else {
            let mut newest: Vec<LogRecord> = inner.records.iter().rev().filter(matches).take(limit).cloned().collect();
            newest.reverse();
            newest
        }
    }

    /// Sequence number of the most recent record (0 when empty).
    pub fn last_seq(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).next_seq - 1
    }

    /// Number of records currently held.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).capacity
    }
}

static LOG_BUFFER: OnceLock<LogBuffer> = OnceLock::new();

/// The process-wide log buffer. Exists (empty) even if logging wasn't initialized.
pub fn log_buffer() -> &'static LogBuffer {
    LOG_BUFFER.get_or_init(|| LogBuffer::new(LOG_CAPACITY))
}

/// Parse a level name such as "warn" or "ERROR".
pub fn parse_level(value: &str) -> Option<Level> {
    value.trim().parse::<Level>().ok()
}

/// A tracing layer that copies events into the log buffer.
struct BufferLayer;

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut text = String::new();
        let _ = write!(text, "{:?}", value);
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.insert(field.name().to_string(), text);
        }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let meta = event.metadata();
        log_buffer().push(Utc::now(), meta.level(), meta.target(), collector.message, collector.fields);
    }
}

pub fn init_logging(service_name: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .with_writer(std::io::stdout)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .pretty();

    // The filter applies to both outputs, so RUST_LOG controls what the UI sees too.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout)
        .with(BufferLayer)
        .try_init();

    tracing::info!(service = %service_name, "🗎 Logging initialized.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(buffer: &LogBuffer, level: Level, target: &str, message: &str) {
        buffer.push(Utc::now(), &level, target, message.to_string(), BTreeMap::new());
    }

    #[test]
    fn keeps_the_newest_records_up_to_capacity() {
        let buffer = LogBuffer::new(3);
        for i in 0..5 {
            push(&buffer, Level::INFO, "t", &format!("m{}", i));
        }
        let all = buffer.query(&LogQuery { limit: 10, ..Default::default() });
        assert_eq!(all.iter().map(|r| r.message.as_str()).collect::<Vec<_>>(), ["m2", "m3", "m4"]);
        assert_eq!(all.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(buffer.last_seq(), 5);
    }

    #[test]
    fn filters_by_level_target_search_and_sequence() {
        let buffer = LogBuffer::new(100);
        push(&buffer, Level::INFO, "hexdb_core::wal", "flushed 3 entries");
        push(&buffer, Level::WARN, "hexdb_core::network::discovery", "peer lost");
        push(&buffer, Level::ERROR, "hexdb_api", "boom");
        push(&buffer, Level::DEBUG, "hexdb_core::wal", "details");

        let q = |query: LogQuery| {
            buffer
                .query(&LogQuery { limit: query.limit.max(10), ..query })
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(q(LogQuery { level: Some(Level::WARN), ..Default::default() }), [2, 3]);
        assert_eq!(q(LogQuery { target: Some("hexdb_core::wal".into()), ..Default::default() }), [1, 4]);
        assert_eq!(q(LogQuery { search: Some("PEER".into()), ..Default::default() }), [2]);
        assert_eq!(q(LogQuery { after: Some(2), ..Default::default() }), [3, 4]);

        let page = |query: LogQuery| buffer.query(&query).iter().map(|r| r.seq).collect::<Vec<_>>();
        assert_eq!(page(LogQuery { before: Some(3), limit: 1, ..Default::default() }), [2]);
        assert_eq!(page(LogQuery { limit: 2, ..Default::default() }), [3, 4]);
    }
}
