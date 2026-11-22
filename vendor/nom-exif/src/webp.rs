//! WebP (RIFF) chunk parser — pure-function EXIF extractor.
//!
//! WebP is a RIFF container: `"RIFF"` + fileSize(LE u32) + `"WEBP"`, then a
//! sequence of chunks (`FourCC` + size(LE u32) + payload + one pad byte when
//! size is odd). EXIF lives in an `EXIF` chunk (raw TIFF), present only in
//! extended (VP8X) files, and sits *after* the image bitstream — so the
//! walker must skip potentially-large `VP8 `/`VP8L`/`ANMF` chunks to reach it.
//!
//! Unlike PNG there is no terminator chunk, so the walk is bounded by the
//! RIFF size field. That bound (bytes left in the chunk region) is threaded
//! across a `ClearAndSkip` via [`ParsingState::WebpPastHeader`] so the walker
//! stops cleanly (`Ok(None)` → `ExifNotFound`) instead of reading to EOF.
//!
//! Stateless and pure: operates on a `&[u8]` buffer plus the resume-state
//! from any prior call. The caller (`MediaParser`) drives all I/O.

use crate::error::{MalformedKind, ParsingError, ParsingErrorState};
use crate::parser::ParsingState;

const WEBP_HEADER_LEN: usize = 12; // "RIFF" + size(4) + "WEBP"
const CHUNK_HEADER_LEN: usize = 8; // FourCC(4) + size(4)

/// Walk the RIFF chunk stream and return the `EXIF` chunk payload (raw TIFF),
/// borrowed from `buf`, if present.
///
/// `state` is `Some(WebpPastHeader(stream_left))` only after a `ClearAndSkip`:
/// `buf[0]` is then a chunk boundary rather than the 12-byte file header, and
/// `stream_left` is the number of chunk-region bytes remaining from `buf[0]`.
pub(crate) fn extract_exif(
    state: Option<ParsingState>,
    buf: &[u8],
) -> Result<(Option<&[u8]>, Option<ParsingState>), ParsingErrorState> {
    // True only when resuming after a ClearAndSkip (buf[0] is a chunk
    // boundary). Captured before `state` is consumed by the match below.
    let past_header = matches!(state, Some(ParsingState::WebpPastHeader(_)));

    // (cursor start, chunk-region bytes remaining from cursor)
    let (mut cursor, mut stream_left) = match state {
        Some(ParsingState::WebpPastHeader(left)) => (0usize, left),
        _ => {
            if buf.len() < WEBP_HEADER_LEN {
                return Err(ParsingErrorState::new(
                    ParsingError::Need(WEBP_HEADER_LEN - buf.len()),
                    None,
                ));
            }
            if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WEBP" {
                return Err(ParsingErrorState::new(
                    ParsingError::Failed {
                        kind: MalformedKind::WebpChunk,
                        message: "WebP: bad RIFF/WEBP header".into(),
                    },
                    None,
                ));
            }
            // RIFF size = whole file minus 8 ("RIFF" + size field). The chunk
            // region is everything after the "WEBP" FourCC, i.e. riff_size - 4.
            let riff_size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            (WEBP_HEADER_LEN, riff_size.saturating_sub(4))
        }
    };

    // Preserve the resume bound across `Need` returns (buffer only grows).
    // Only meaningful once past the header: in the initial header path a
    // `Need` must return `None` so the resumed call re-validates the 12-byte
    // RIFF header still sitting at buf[0] (mirrors png.rs). Returning
    // `WebpPastHeader` there would reset the cursor to 0 and misread "RIFF".
    let preserve = |left: usize| past_header.then_some(ParsingState::WebpPastHeader(left));

    loop {
        // No room in the declared chunk region for another chunk header → done.
        if stream_left < CHUNK_HEADER_LEN {
            return Ok((None, Some(ParsingState::WebpPastHeader(stream_left))));
        }
        // Buffer doesn't yet hold the next chunk header — ask for more bytes.
        let in_buf = buf.len() - cursor;
        if in_buf < CHUNK_HEADER_LEN {
            return Err(ParsingErrorState::new(
                ParsingError::Need(CHUNK_HEADER_LEN - in_buf),
                preserve(stream_left),
            ));
        }

        let fourcc = &buf[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            buf[cursor + 4],
            buf[cursor + 5],
            buf[cursor + 6],
            buf[cursor + 7],
        ]);
        // total = header(8) + payload(size) + pad(1 if size is odd).
        let pad = (size & 1) as usize;
        let total = match (size as usize).checked_add(CHUNK_HEADER_LEN + pad) {
            Some(t) => t,
            None => {
                return Err(ParsingErrorState::new(
                    ParsingError::Failed {
                        kind: MalformedKind::WebpChunk,
                        message: "WebP: chunk size overflows addressable size".into(),
                    },
                    preserve(stream_left),
                ));
            }
        };

        // Chunk claims more than the RIFF container allows — stop at the
        // container boundary. Placed before the EXIF branch so we never
        // return bytes from outside the declared RIFF region.
        if total > stream_left {
            return Ok((None, Some(ParsingState::WebpPastHeader(0))));
        }

        if fourcc == b"EXIF" {
            if total > in_buf {
                return Err(ParsingErrorState::new(
                    ParsingError::Need(total - in_buf),
                    preserve(stream_left),
                ));
            }
            let data_start = cursor + CHUNK_HEADER_LEN;
            let data_end = data_start + size as usize;
            let mut payload = &buf[data_start..data_end];
            // Some encoders wrongly prepend the JPEG APP1 "Exif\0\0" marker.
            if payload.starts_with(b"Exif\0\0") {
                payload = &payload[6..];
            }
            // A chunk containing only the stray marker (no TIFF body) is not
            // usable EXIF — report it as absent rather than an empty slice.
            if payload.is_empty() {
                return Ok((None, preserve(stream_left)));
            }
            return Ok((Some(payload), preserve(stream_left)));
        }

        // Non-EXIF chunk. If it isn't fully buffered, skip it whole: advance
        // the parser by `cursor + total` (bytes walked in `buf` plus this
        // chunk) and resume past the header with the reduced bound.
        if total > in_buf {
            return Err(ParsingErrorState::new(
                ParsingError::ClearAndSkip(cursor + total),
                Some(ParsingState::WebpPastHeader(stream_left - total)),
            ));
        }
        cursor += total;
        stream_left -= total;
    }
}
