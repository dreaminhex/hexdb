// HexDB Core Metadata Extraction
//
// Reads what a file says about itself, for a files tessellation's index (see
// `crate::files`). Every file gets a `meta` object with:
//
//   - normalized fields that mean the same across formats: `title`, `artist`,
//     `album`, `author`, `keywords`, `taken_at`, `width`, `height`,
//     `duration_ms`, `gps` ({lat, lon, alt}), `camera` ({make, model}), ...
//     The first source that has a value wins; format-specific tags are read
//     before generic ones (XMP), so they win;
//   - the raw tags under one key per tag family: `exif`, `iptc`, `xmp`,
//     `id3v2`, `vorbis`, `mp4`, `riff_info`, `bext`, `ixml`, `matroska`,
//     `quicktime`, `asf`, `pdf`, `office`, `epub`, `dicom`, `c2pa`, ...;
//   - `text`: the start of the file's text (plain text, Word, OpenDocument),
//     for full-text search.
//
// Extraction only reads: through a `Source` with a byte budget and a
// deadline, so a malformed or hostile file can't make it read the whole store
// or run forever. Each extractor runs on its own and catches panics; one that
// fails adds to `errors` and the others still run. Values are capped (string
// length, array length, total size), and binary blobs (thumbnails, maker
// notes, cover art) are left out.
//
// `VERSION` is stored with the results; raising it when extractors improve
// lets the server re-extract older files.

mod docs;
mod image;
mod media;
pub mod thumbnail;
mod video;
mod xml;

use serde_json::{json, Map, Value};
use std::io::{self, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

/// Bumped whenever extraction changes what it finds.
pub const VERSION: u64 = 1;

/// Longest string kept in a value.
const MAX_STRING: usize = 4096;
/// Most items kept in an array or object.
const MAX_ITEMS: usize = 500;
/// Largest `meta` object (JSON bytes) before raw tag families are dropped.
const MAX_META_BYTES: usize = 256 * 1024;

/// How much work extraction may do for one file.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes read from the file at most.
    pub max_read_bytes: u64,
    /// Time allowed.
    pub timeout: Duration,
    /// Text kept for full-text search.
    pub max_text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_read_bytes: 64 * 1024 * 1024, timeout: Duration::from_secs(20), max_text_bytes: 64 * 1024 }
    }
}

/// Random access to a file's bytes.
pub trait ByteSource: Send {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read up to `buf.len()` bytes at `offset`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

impl ByteSource for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.as_slice().len());
        let n = buf.len().min(self.as_slice().len() - start);
        buf[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}

/// A file being read for extraction: `Read + Seek`, refusing to read past its
/// byte budget or after its deadline.
pub struct Source {
    inner: Box<dyn ByteSource>,
    pos: u64,
    read: u64,
    budget: u64,
    deadline: Instant,
}

impl Source {
    pub fn new(inner: Box<dyn ByteSource>, limits: &Limits) -> Source {
        Source { inner, pos: 0, read: 0, budget: limits.max_read_bytes, deadline: Instant::now() + limits.timeout }
    }

    pub fn len(&self) -> u64 {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check(&self, want: usize) -> io::Result<()> {
        if Instant::now() > self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "extraction took too long"));
        }
        if self.read + want as u64 > self.budget {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "extraction read too much of the file"));
        }
        Ok(())
    }

    /// `len` bytes at `offset` (fewer at the end of the file).
    pub fn range(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let len = len.min(self.len().saturating_sub(offset) as usize);
        self.check(len)?;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = self.inner.read_at(offset + filled as u64, &mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        self.read += filled as u64;
        Ok(buf)
    }

    /// The first `len` bytes.
    pub fn head(&mut self, len: usize) -> io::Result<Vec<u8>> {
        self.range(0, len)
    }

    /// The last `len` bytes.
    pub fn tail(&mut self, len: usize) -> io::Result<Vec<u8>> {
        let start = self.len().saturating_sub(len as u64);
        self.range(start, len)
    }

    /// The whole file, if it's at most `max` bytes.
    pub fn all(&mut self, max: u64) -> io::Result<Vec<u8>> {
        if self.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("file is over {} MB", max / (1024 * 1024))));
        }
        self.range(0, self.len() as usize)
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min(self.len().saturating_sub(self.pos) as usize);
        if want == 0 {
            return Ok(0);
        }
        self.check(want)?;
        let n = self.inner.read_at(self.pos, &mut buf[..want])?;
        self.pos += n as u64;
        self.read += n as u64;
        Ok(n)
    }
}

impl Seek for Source {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(n) => self.len() as i128 + n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

/// What extraction found.
#[derive(Debug, Default, Clone)]
pub struct Extracted {
    pub meta: Map<String, Value>,
    /// The extractors that found something.
    pub extractors: Vec<String>,
    /// Problems, one line each (the file is still stored and indexed).
    pub errors: Vec<String>,
}

impl Extracted {
    /// The `extract` status object stored on the file's document.
    pub fn status(&self) -> Value {
        json!({ "version": VERSION, "extractors": self.extractors, "errors": self.errors, "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true) })
    }
}

/// The state extractors share for one file.
pub(crate) struct Ctx<'a> {
    pub source: &'a mut Source,
    pub name: String,
    pub content_type: String,
    /// The first 256 KB.
    pub head: Vec<u8>,
    pub meta: Map<String, Value>,
    pub text: String,
    pub limits: Limits,
}

impl Ctx<'_> {
    /// The file's extension, lowercase.
    pub fn ext(&self) -> String {
        self.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
    }

    /// Store a raw tag family (merged if it's already there).
    pub fn raw(&mut self, family: &str, values: Map<String, Value>) {
        if values.is_empty() {
            return;
        }
        match self.meta.get_mut(family) {
            Some(Value::Object(existing)) => {
                for (k, v) in values {
                    existing.entry(k).or_insert(v);
                }
            }
            _ => {
                self.meta.insert(family.into(), Value::Object(values));
            }
        }
    }

    /// Set a normalized field unless an earlier source set it.
    pub fn norm(&mut self, key: &str, value: Value) {
        let empty = match &value {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if empty {
            return;
        }
        let value = match value {
            Value::String(s) => Value::String(s.trim().to_string()),
            other => other,
        };
        self.meta.entry(key.to_string()).or_insert(value);
    }

    /// `norm` for a string.
    pub fn norm_str(&mut self, key: &str, value: Option<&str>) {
        if let Some(v) = value {
            self.norm(key, json!(v));
        }
    }

    /// Merge into a normalized object field (e.g. `camera.make`).
    pub fn norm_in(&mut self, key: &str, field: &str, value: Value) {
        if value.is_null() || value.as_str().is_some_and(|s| s.trim().is_empty()) {
            return;
        }
        let entry = self.meta.entry(key.to_string()).or_insert_with(|| json!({}));
        if let Value::Object(map) = entry {
            map.entry(field.to_string()).or_insert(value);
        }
    }

    /// Add keywords (deduplicated).
    pub fn keywords<I: IntoIterator<Item = String>>(&mut self, words: I) {
        let entry = self.meta.entry("keywords".to_string()).or_insert_with(|| json!([]));
        if let Value::Array(list) = entry {
            for word in words {
                let word = word.trim().to_string();
                if !word.is_empty() && !list.iter().any(|w| w.as_str() == Some(word.as_str())) && list.len() < MAX_ITEMS {
                    list.push(Value::String(word));
                }
            }
        }
    }

    /// Append searchable text, up to the limit.
    pub fn add_text(&mut self, text: &str) {
        let room = self.limits.max_text_bytes.saturating_sub(self.text.len());
        if room == 0 || text.trim().is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(truncate(text.trim(), room));
    }
}

/// `s` cut to at most `max` bytes on a character boundary.
pub(crate) fn truncate_text(s: &str, max: usize) -> &str {
    truncate(s, max)
}

/// `s` cut to at most `max` bytes on a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Text from bytes that may be UTF-8, UTF-16 (with a BOM) or Latin-1, without NULs.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    let text = if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        let be = bytes[0] == 0xFE;
        let units: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|c| if be { u16::from_be_bytes(*c) } else { u16::from_le_bytes(*c) }).collect();
        String::from_utf16_lossy(&units)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        }
    };
    text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).replace('\0', " ")
}

/// Keep values within the caps: strings, arrays, objects and depth.
fn cap(value: &mut Value, depth: usize) {
    match value {
        Value::String(s) => {
            if s.len() > MAX_STRING {
                *s = truncate(s, MAX_STRING).to_string();
            }
        }
        Value::Array(items) => {
            if depth > 6 {
                items.clear();
            }
            items.truncate(MAX_ITEMS);
            items.iter_mut().for_each(|v| cap(v, depth + 1));
        }
        Value::Object(map) => {
            if depth > 6 || map.len() > MAX_ITEMS {
                let keep: Vec<String> = if depth > 6 { Vec::new() } else { map.keys().take(MAX_ITEMS).cloned().collect() };
                map.retain(|k, _| keep.contains(k));
            }
            map.values_mut().for_each(|v| cap(v, depth + 1));
        }
        _ => {}
    }
}

/// Drop the largest raw tag families (then trim the text) until `meta` fits.
fn fit(meta: &mut Map<String, Value>) {
    let size = |v: &Map<String, Value>| serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0);
    while size(meta) > MAX_META_BYTES {
        let largest = meta
            .iter()
            .filter(|(k, v)| v.is_object() && !matches!(k.as_str(), "gps" | "camera" | "exposure" | "location"))
            .max_by_key(|(_, v)| serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0))
            .map(|(k, _)| k.clone());
        match largest {
            Some(key) => {
                meta.remove(&key);
                if let Some(list) = meta.entry("dropped".to_string()).or_insert_with(|| json!([])).as_array_mut() {
                    list.push(json!(key));
                }
            }
            None => {
                if let Some(Value::String(text)) = meta.get_mut("text") {
                    let half = text.len() / 2;
                    *text = truncate(text, half).to_string();
                    if half == 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }
}

type Extractor = fn(&mut Ctx) -> anyhow::Result<bool>;

/// Every extractor, most specific first. Each decides from the file's type
/// and bytes whether it applies, and returns whether it found anything.
const EXTRACTORS: &[(&str, Extractor)] = &[
    ("exif", image::exif),
    ("png", image::png_text),
    ("iptc", image::iptc),
    ("psd", image::psd),
    ("image", image::dimensions),
    ("audio", media::audio),
    ("bext", media::riff_chunks),
    ("asf", media::asf),
    ("track", media::track),
    ("matroska", media::matroska),
    ("pdf", docs::pdf),
    ("eps", docs::eps),
    ("office", docs::office),
    ("odf", docs::odf),
    ("epub", docs::epub),
    ("font", docs::font),
    ("svg", xml::svg),
    ("dicom", docs::dicom),
    ("archive", docs::archive),
    ("text", docs::text),
    ("xmp", xml::xmp),
    ("mpeg7", xml::mpeg7),
    ("c2pa", docs::c2pa),
];

/// Extract everything known about a file.
pub fn extract(source: &mut Source, name: &str, content_type: &str, limits: &Limits) -> Extracted {
    let mut out = Extracted::default();
    let head = match source.head(256 * 1024) {
        Ok(head) => head,
        Err(e) => {
            out.errors.push(format!("reading the file: {}", e));
            return out;
        }
    };
    let mut ctx = Ctx { source, name: name.to_string(), content_type: content_type.to_ascii_lowercase(), head, meta: Map::new(), text: String::new(), limits: *limits };
    for (label, run) in EXTRACTORS {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut ctx)));
        match result {
            Ok(Ok(true)) => out.extractors.push(label.to_string()),
            Ok(Ok(false)) => {}
            Ok(Err(e)) => out.errors.push(format!("{}: {}", label, truncate(&format!("{:#}", e), 300))),
            Err(_) => out.errors.push(format!("{}: the parser failed on this file", label)),
        }
    }
    let mut meta = std::mem::take(&mut ctx.meta);
    if !ctx.text.is_empty() {
        meta.insert("text".into(), Value::String(std::mem::take(&mut ctx.text)));
    }
    for value in meta.values_mut() {
        cap(value, 0);
    }
    fit(&mut meta);
    out.meta = meta;
    out
}

/// Extract from bytes in memory (tests and small files).
pub fn extract_bytes(bytes: Vec<u8>, name: &str, content_type: &str) -> Extracted {
    let limits = Limits::default();
    let mut source = Source::new(Box::new(bytes), &limits);
    extract(&mut source, name, content_type, &limits)
}

#[cfg(test)]
mod tests;
