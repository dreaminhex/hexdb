//! PNG chunk parser — pure-function implementation.
//!
//! This module walks the PNG chunk stream and extracts:
//! - EXIF from an `eXIf` chunk or a legacy `Raw profile type {exif,APP1}`
//!   `tEXt` chunk.
//! - The `tEXt` chunks as Latin-1-decoded `(key, value)` pairs.
//!
//! The caller drives I/O according to the returned `ParsingErrorState`.

use std::ops::Range;

use crate::error::{MalformedKind, ParsingError, ParsingErrorState};
use crate::parser::ParsingState;

/// PNG chunk-walker state, returned as output once `IEND` is reached.
#[derive(Debug, Clone, Default)]
pub(crate) struct PngState {
    pub exif: Option<PngExifSource>,
    pub text_chunks: Vec<(String, String)>,
    cursor: usize,
    text_total: usize,
    exif_priority: u8,
}

/// Where the EXIF data was found in the PNG.
#[derive(Debug, Clone)]
pub(crate) enum PngExifSource {
    /// PNG 1.5 `eXIf` chunk — TIFF body sits at this byte range inside
    /// the parser buffer. Use this with `bytes::Bytes::slice` for zero-copy.
    EXif(Range<usize>),

    /// Owned TIFF bytes, either decoded from a legacy `Raw profile type *`
    /// `tEXt` chunk or retained across a streaming skip.
    Owned(bytes::Bytes),
}

impl PngState {
    fn retry(self, err: ParsingError) -> ParsingErrorState {
        ParsingErrorState::new(err, Some(ParsingState::Png(self)))
    }

    fn skip(mut self, buf: &[u8], count: usize) -> ParsingErrorState {
        if matches!(
            &self.exif,
            Some(PngExifSource::EXif(range)) if range.len() > MAX_RETAINED_EXIF_SIZE
        ) {
            return ParsingErrorState::new(
                ParsingError::Failed {
                    kind: MalformedKind::PngChunk,
                    message: "PNG: eXIf chunk too large to retain across stream skip".into(),
                },
                None,
            );
        }
        self.exif = self.exif.map(|source| match source {
            PngExifSource::EXif(range) => {
                PngExifSource::Owned(bytes::Bytes::copy_from_slice(&buf[range]))
            }
            owned @ PngExifSource::Owned(_) => owned,
        });
        self.cursor = 0;
        self.retry(ParsingError::ClearAndSkip(count))
    }
}

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_RETAINED_EXIF_SIZE: usize = 64 * 1024 * 1024;

/// Maximum size of a single `tEXt` chunk we'll capture. Above this
/// threshold the chunk is skipped (defensive against crafted inputs).
const MAX_TEXT_CHUNK_SIZE: u32 = 1024 * 1024; // 1 MiB

/// Maximum estimated heap retained for captured `tEXt` entries. After
/// exceeding this, further entries are skipped (already-captured entries kept).
const MAX_TEXT_CHUNKS_TOTAL: usize = 16 * 1024 * 1024; // 16 MiB

/// Decode bytes as Latin-1 into a `String`. Infallible — every Latin-1
/// byte maps to a Unicode code point (U+0000..U+00FF). Per PNG spec, `tEXt`
/// chunks use Latin-1 encoding; we do not sniff for UTF-8.
fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Decode the value of a `Raw profile type *` `tEXt` chunk.
///
/// ImageMagick writes these chunks with a header preamble:
/// ```text
/// \n
/// exif\n
///        54\n           <- length in bytes (decimal, with leading whitespace)
/// 4949 2a00 0800 0000 ...   <- hex bytes
/// ```
///
/// This helper:
/// 1. Skips the leading `\n` line.
/// 2. Skips the second line (`exif`, `app1`, etc).
/// 3. Skips the third line (length).
/// 4. Hex-decodes the rest, ignoring all whitespace.
fn decode_raw_profile_value(s: &str) -> Result<Vec<u8>, ()> {
    let mut lines = s.lines();
    // Skip the empty first line, the type line, and the length line.
    // Tolerate variations: just consume the first 3 newlines worth of header.
    lines.next().ok_or(())?;
    lines.next().ok_or(())?;
    lines.next().ok_or(())?;
    let body: String = lines.collect();
    hex_decode(&body)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut high: Option<u8> = None;
    for c in s.bytes() {
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return Err(()),
        };
        match high.take() {
            None => high = Some(nibble),
            Some(h) => out.push((h << 4) | nibble),
        }
    }
    if high.is_some() {
        return Err(());
    }
    Ok(out)
}

/// Walk the PNG chunk stream and extract EXIF + tEXt entries.
///
/// Pure function: no I/O, takes a buffer slice plus the resume-state
/// from any prior call, returns either output or a `ParsingErrorState`
/// requesting more bytes / skipping bytes.
///
/// `state` is `None` on the initial call. Retries carry the next chunk's
/// cursor and any metadata already found. After a `ClearAndSkip`, the parser
/// has dropped the buffer, so the resumed cursor is zero and `buf` starts at
/// a fresh file offset rather than with the PNG signature.
///
/// `ClearAndSkip(n)` is interpreted by the parser as "advance the
/// parser's logical position by `n` bytes from where it is now". The
/// closure sees `buf` already offset to that position, so the skip
/// request must cover *both* the bytes the walker consumed inside
/// `buf` (`cursor`) and the chunk bytes still beyond it. That is
/// `cursor + total`, not `total - remaining` — the latter would only
/// account for bytes past the buffer's end and leave the walker
/// stranded mid-chunk on retry (issue #55).
#[tracing::instrument(skip(buf, state))]
pub(crate) fn extract_chunks(
    buf: &[u8],
    state: Option<ParsingState>,
) -> Result<PngState, ParsingErrorState> {
    let mut state = match state {
        Some(ParsingState::Png(state)) => state,
        _ => {
            if buf.len() < PNG_SIGNATURE.len() {
                return Err(ParsingErrorState::new(
                    ParsingError::Need(PNG_SIGNATURE.len() - buf.len()),
                    None,
                ));
            }
            if &buf[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
                return Err(ParsingErrorState::new(
                    ParsingError::Failed {
                        kind: MalformedKind::PngChunk,
                        message: "PNG: bad signature".into(),
                    },
                    None,
                ));
            }
            PngState {
                cursor: PNG_SIGNATURE.len(),
                ..Default::default()
            }
        }
    };

    loop {
        // Need 8 bytes for the chunk header (length:4 + type:4).
        let remaining = buf.len() - state.cursor;
        if remaining < 8 {
            return Err(state.retry(ParsingError::Need(8 - remaining)));
        }
        let length = u32::from_be_bytes([
            buf[state.cursor],
            buf[state.cursor + 1],
            buf[state.cursor + 2],
            buf[state.cursor + 3],
        ]);
        let ctype = &buf[state.cursor + 4..state.cursor + 8];

        // Compute total chunk size = 8 (header) + length (data) + 4 (CRC).
        // On 32-bit targets, `length as usize + 12` can wrap when length is
        // close to u32::MAX; bail out as malformed instead.
        let total = match (length as usize).checked_add(12) {
            Some(t) => t,
            None => {
                return Err(ParsingErrorState::new(
                    ParsingError::Failed {
                        kind: MalformedKind::PngChunk,
                        message: "PNG: chunk length overflows addressable size".into(),
                    },
                    None,
                ));
            }
        };

        if ctype == b"IEND" {
            break;
        }

        let needs_body = match ctype {
            b"eXIf" => true,
            b"tEXt" => length <= MAX_TEXT_CHUNK_SIZE,
            _ => false,
        };
        if total > remaining {
            if needs_body {
                return Err(state.retry(ParsingError::Need(total - remaining)));
            }
            let count = state.cursor + total;
            return Err(state.skip(buf, count));
        }

        match ctype {
            b"eXIf" => {
                let data_start = state.cursor + 8;
                let data_end = data_start + length as usize;
                // eXIf has priority 3 (highest), always wins.
                state.exif = Some(PngExifSource::EXif(data_start..data_end));
                state.exif_priority = 3;
            }
            b"tEXt" if needs_body => {
                let data = &buf[state.cursor + 8..state.cursor + 8 + length as usize];
                // tEXt format: Latin-1 keyword + 0x00 + Latin-1 text
                if let Some(nul_pos) = data.iter().position(|&b| b == 0) {
                    let key = decode_latin1(&data[..nul_pos]);
                    let value = decode_latin1(&data[nul_pos + 1..]);

                    // Legacy EXIF detection
                    let candidate_priority: u8 = match key.as_str() {
                        "Raw profile type APP1" => 2,
                        "Raw profile type exif" => 1,
                        _ => 0,
                    };
                    if candidate_priority > 0 && candidate_priority > state.exif_priority {
                        if let Ok(mut bytes) = decode_raw_profile_value(&value) {
                            // Strip APP1's leading "Exif\0\0" if present.
                            if key.ends_with("APP1") && bytes.starts_with(b"Exif\0\0") {
                                bytes.drain(0..6);
                            }
                            // Validate as TIFF (must have a valid byte-order marker
                            // + magic number) before accepting.
                            if bytes.len() >= 8 && crate::exif::TiffHeader::parse(&bytes).is_ok() {
                                state.exif = Some(PngExifSource::Owned(bytes.into()));
                                state.exif_priority = candidate_priority;
                            }
                            // else: silently drop the legacy candidate, keep raw text entry below
                        }
                        // hex_decode failure → silently drop too
                    }

                    let entry_size =
                        std::mem::size_of::<(String, String)>() + key.len() + value.len();
                    if state.text_total + entry_size <= MAX_TEXT_CHUNKS_TOTAL {
                        state.text_total += entry_size;
                        state.text_chunks.push((key, value));
                    }
                    // else: silently skip (already-captured entries kept).
                }
                // else: malformed tEXt (no NUL separator) — silently skip.
            }
            _ => {}
        }
        state.cursor += total;
    }

    Ok(state)
}
