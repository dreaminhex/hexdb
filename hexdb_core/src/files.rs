// HexDB Core Files
//
// A files tessellation (kind "files") holds binary files. Each file is an
// ordinary document describing it, so it can be found with filters, text
// search, aggregations, GraphQL and SQL, protected by roles and row filters,
// and replicated like any document. Its bytes live in the chunk store
// (`crate::blobs`), referenced through a manifest chunk:
//
//   { "id", "name", "path", "size", "content_type", "sha256",
//     "content": { "manifest", "chunks", "compressed" },
//     "version", "versions": [earlier versions, newest first],
//     "created_at", "modified_at", "uploaded_at", "uploaded_by",
//     "meta": {...extracted metadata...}, "extract": {...},
//     ...fields the client adds (tags, description, ...) }
//
// The system fields can only be written by uploads and the file operations;
// a document write keeps their stored values (see `engine::files`). `name` and
// `path` can be changed (rename, move); a unique index keeps (path, name)
// unique. Deleting a file moves its document to the trash (`_files_trash`),
// from which it can be restored until it expires.

use crate::blobs::{ChunkId, AVG_CHUNK, MAX_CHUNK, MIN_CHUNK};
use anyhow::{bail, Result};
use std::future::Future;

/// The tessellation kind for binary files.
pub const FILES_KIND: &str = "files";
/// Deleted files, from every files tessellation, until they expire.
pub const TRASH_TESSELLATION: &str = "_files_trash";
/// Resumable uploads in progress (expire after a day).
pub const UPLOADS_TESSELLATION: &str = "_uploads";
/// The unique index on (path, name) every files tessellation has.
pub const PATH_INDEX: &str = "path_name";
/// The text index a files tessellation gets (unless it has another).
pub const SEARCH_INDEX: &str = "search";

/// Fields only uploads and file operations may write.
pub const SYSTEM_FIELDS: &[&str] = &[
    "thumbnail",
    "size",
    "content_type",
    "sha256",
    "content",
    "version",
    "versions",
    "created_at",
    "modified_at",
    "uploaded_at",
    "uploaded_by",
    "meta",
    "extract",
];

/// Fields of a trashed file's document that aren't the file's own.
pub const TRASH_FIELDS: &[&str] = &["from", "trashed_at", "trashed_by"];

const MAX_NAME_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 1024;
const MAX_PATH_DEPTH: usize = 64;

tokio::task_local! {
    static INTERNAL: ();
}

/// Run `f` as the files subsystem itself: its writes may set system fields
/// and really delete file documents.
pub async fn as_internal<F: Future>(f: F) -> F::Output {
    INTERNAL.scope((), f).await
}

pub fn is_internal() -> bool {
    INTERNAL.try_with(|_| ()).is_ok()
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    crate::engine::EngineError::Invalid(message.into()).into()
}

/// Check a file or folder name: 1-255 bytes, no slashes or control characters, not `.` or `..`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid(format!("File names must be 1-{} bytes long.", MAX_NAME_BYTES)));
    }
    if name == "." || name == ".." || name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return Err(invalid(format!("'{}' isn't a valid file name (no '/', '\\', control characters, '.' or '..').", name)));
    }
    Ok(())
}

/// Normalize a folder path to `/a/b/` form (the root is `/`). Both `/` and
/// `\` separate folders; empty segments are dropped.
pub fn normalize_path(path: &str) -> Result<String> {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.len() > MAX_PATH_DEPTH {
        return Err(invalid(format!("Folder paths may be at most {} levels deep.", MAX_PATH_DEPTH)));
    }
    for part in &parts {
        validate_name(part)?;
    }
    let normalized = if parts.is_empty() { "/".to_string() } else { format!("/{}/", parts.join("/")) };
    if normalized.len() > MAX_PATH_BYTES {
        return Err(invalid(format!("Folder paths may be at most {} bytes long.", MAX_PATH_BYTES)));
    }
    Ok(normalized)
}

/// Split `a/b/c.jpg` into the folder (`/a/b/`) and the name (`c.jpg`).
pub fn split_file_path(path: &str) -> Result<(String, String)> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let (folder, name) = match trimmed.rfind(['/', '\\']) {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("", trimmed),
    };
    validate_name(name)?;
    Ok((normalize_path(folder)?, name.to_string()))
}

/// The content type of a file: its magic bytes first, then its name's
/// extension, then what the client declared, then `application/octet-stream`.
/// A generic container type (ZIP, OLE) gives way to a more specific extension
/// (DOCX, EPUB, MSG...).
pub fn sniff_type(head: &[u8], name: &str, declared: Option<&str>) -> String {
    let by_name = mime_guess::from_path(name).first().map(|m| m.essence_str().to_string());
    let by_magic = infer::get(head).map(|t| t.mime_type().to_string());
    let generic = |m: &str| matches!(m, "application/zip" | "application/x-ole-storage" | "application/xml" | "text/xml" | "text/plain");
    let declared = declared
        .map(|d| d.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty() && d != "application/octet-stream" && d.contains('/'));
    match (by_magic, by_name) {
        (Some(magic), Some(name)) if generic(&magic) && name != magic => name,
        (Some(magic), _) => magic,
        (None, Some(name)) => name,
        (None, None) => declared.unwrap_or_else(|| "application/octet-stream".into()),
    }
}

/// Whether to try compressing a file's chunks. Images, audio and video
/// (lossless or not) and formats that are already compressed are stored
/// byte for byte; everything else is compressed when it saves at least 5%.
pub fn should_compress(content_type: &str) -> bool {
    let t = content_type.to_ascii_lowercase();
    if t == "image/svg+xml" {
        return true;
    }
    if t.starts_with("image/") || t.starts_with("audio/") || t.starts_with("video/") || t.starts_with("font/woff") {
        return false;
    }
    !matches!(
        t.as_str(),
        "application/zip"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-bzip2"
            | "application/x-xz"
            | "application/zstd"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
            | "application/x-rar-compressed"
            | "application/java-archive"
            | "application/epub+zip"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.oasis.opendocument.text"
            | "application/vnd.oasis.opendocument.spreadsheet"
            | "application/vnd.oasis.opendocument.presentation"
            | "application/vnd.adobe.photoshop"
            | "application/x-photoshop"
            | "application/ogg"
            | "application/vnd.android.package-archive"
    )
}

/// Cuts a stream of bytes into content-defined chunks. Boundaries depend only
/// on the content, so an edit in the middle of a file changes the chunks
/// around it, not the ones before or after.
#[derive(Default)]
pub struct Chunker {
    buf: Vec<u8>,
}

impl Chunker {
    /// Add bytes; returns the chunks that are now complete.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        // With at least MAX_CHUNK bytes buffered, the next boundary is final.
        while self.buf.len() >= MAX_CHUNK {
            let end = next_boundary(&self.buf);
            out.push(self.buf.drain(..end).collect());
        }
        out
    }

    /// The remaining chunks at the end of the stream.
    pub fn finish(mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while !self.buf.is_empty() {
            let end = next_boundary(&self.buf);
            out.push(self.buf.drain(..end).collect());
        }
        out
    }
}

fn next_boundary(buf: &[u8]) -> usize {
    let cdc = fastcdc::v2020::FastCDC::new(buf, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK);
    let (_, end) = cdc.cut(0, buf.len());
    end.clamp(1, buf.len())
}

/// The ordered chunks of one file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Manifest {
    pub chunks: Vec<(ChunkId, u32)>,
}

const MANIFEST_MAGIC: &[u8; 4] = b"HXM1";

impl Manifest {
    pub fn size(&self) -> u64 {
        self.chunks.iter().map(|(_, len)| *len as u64).sum()
    }

    /// `HXM1 | count (4) | (chunk ID (32) | length (4))*`, big-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.chunks.len() * 36);
        out.extend_from_slice(MANIFEST_MAGIC);
        out.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        for (id, len) in &self.chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&len.to_be_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Manifest> {
        if bytes.len() < 8 || &bytes[..4] != MANIFEST_MAGIC {
            bail!("not a file manifest");
        }
        let count = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if bytes.len() != 8 + count * 36 {
            bail!("file manifest has the wrong length");
        }
        let chunks = bytes[8..]
            .as_chunks::<36>()
            .0
            .iter()
            .map(|c| (c[..32].try_into().unwrap(), u32::from_be_bytes(c[32..].try_into().unwrap())))
            .collect();
        Ok(Manifest { chunks })
    }

    /// The chunks covering bytes `start..end`, each with the range to take from it.
    pub fn slice(&self, start: u64, end: u64) -> Vec<(ChunkId, std::ops::Range<usize>)> {
        let mut out = Vec::new();
        let mut offset = 0u64;
        for (id, len) in &self.chunks {
            let chunk_end = offset + *len as u64;
            if chunk_end > start && offset < end {
                let from = start.saturating_sub(offset) as usize;
                let to = (end.min(chunk_end) - offset) as usize;
                out.push((*id, from..to));
            }
            offset = chunk_end;
            if offset >= end {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert!(validate_name("IMG_0042.jpg").is_ok());
        assert!(validate_name("Résumé (final).pdf").is_ok());
        for bad in ["", " ", ".", "..", "a/b", "a\\b", "tab\there", &"x".repeat(256)] {
            assert!(validate_name(bad).is_err(), "{:?}", bad);
        }
        assert_eq!(normalize_path("").unwrap(), "/");
        assert_eq!(normalize_path("/").unwrap(), "/");
        assert_eq!(normalize_path("a//b\\c/").unwrap(), "/a/b/c/");
        assert!(normalize_path("/a/../b").is_err());
        assert_eq!(split_file_path("2024/Iceland/IMG_1.jpg").unwrap(), ("/2024/Iceland/".into(), "IMG_1.jpg".into()));
        assert_eq!(split_file_path("top.txt").unwrap(), ("/".into(), "top.txt".into()));
        assert!(split_file_path("folder/").is_ok_and(|(p, n)| p == "/" && n == "folder"));
    }

    #[test]
    fn sniffs_types() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
        assert_eq!(sniff_type(&jpeg, "photo.bin", None), "image/jpeg");
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.resize(64, 0);
        assert_eq!(sniff_type(&wav, "take1", None), "audio/x-wav");
        assert_eq!(sniff_type(b"name,age\nada,36\n", "people.csv", None), "text/csv");
        assert_eq!(sniff_type(b"PK\x03\x04rest", "report.docx", None), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(sniff_type(b"\x00\x01", "blob", Some("application/x-custom; v=1")), "application/x-custom");
        assert_eq!(sniff_type(b"\x00\x01", "blob", None), "application/octet-stream");
        assert!(!should_compress("audio/x-wav") && !should_compress("image/tiff") && !should_compress("video/mp4"));
        assert!(should_compress("text/csv") && should_compress("image/svg+xml") && should_compress("application/pdf"));
    }

    #[test]
    fn chunking_is_content_defined() {
        let mut x = 7u64;
        let data: Vec<u8> = (0..20_000_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let chunk = |bytes: &[u8], step: usize| {
            let mut c = Chunker::default();
            let mut out = Vec::new();
            for piece in bytes.chunks(step) {
                out.extend(c.push(piece));
            }
            out.extend(c.finish());
            out
        };
        let a = chunk(&data, 65536);
        let b = chunk(&data, 1_000_003);
        assert_eq!(a, b, "independent of how the stream arrives");
        assert_eq!(a.concat(), data);
        assert!(a.iter().all(|c| c.len() <= MAX_CHUNK));
        assert!(a[..a.len() - 1].iter().all(|c| c.len() >= MIN_CHUNK));

        // Insert bytes near the start: most later chunks are unchanged.
        let mut edited = data.clone();
        edited.splice(1000..1000, b"inserted".iter().copied());
        let c = chunk(&edited, 65536);
        let shared = c.iter().filter(|chunk| a.contains(chunk)).count();
        assert!(shared + 2 >= a.len(), "{} of {} chunks shared", shared, a.len());
    }

    #[test]
    fn manifest_round_trip_and_slices() {
        let m = Manifest { chunks: vec![([1u8; 32], 10), ([2u8; 32], 20), ([3u8; 32], 5)] };
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
        assert_eq!(m.size(), 35);
        assert_eq!(m.slice(0, 35).len(), 3);
        assert_eq!(m.slice(12, 31), vec![([2u8; 32], 2..20), ([3u8; 32], 0..1)]);
        assert_eq!(m.slice(10, 30), vec![([2u8; 32], 0..20)]);
        assert!(Manifest::decode(b"HXM1\0\0\0\x01short").is_err());
    }
}
