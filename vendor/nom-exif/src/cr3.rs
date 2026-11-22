use std::ops::Range;

use nom::IResult;

use crate::{
    bbox::Cr3MoovBox,
    error::{
        nom_error_to_parsing_error_with_state, MalformedKind, ParsingError, ParsingErrorState,
    },
    exif::{check_exif_header2, TiffHeader},
    parser::ParsingState,
};

pub(crate) fn parse_moov_box(input: &[u8]) -> IResult<&[u8], Option<Cr3MoovBox>> {
    Cr3MoovBox::parse(input)
}

/// Result containing all CMT ranges for CR3 files.
/// Each tuple contains (block_id, data_range).
#[derive(Debug, Clone)]
pub(crate) struct Cr3CmtRanges {
    /// All CMT ranges: (block_id, range)
    pub ranges: Vec<(&'static str, Range<usize>)>,
}

/// Extract all CMT data ranges from a CR3 file buffer.
/// Returns the moov box and all CMT ranges if available.
pub(crate) fn extract_all_cmt_ranges(
    buf: &[u8],
) -> Result<Option<Cr3CmtRanges>, ParsingErrorState> {
    let (_, moov) = parse_moov_box(buf)
        .map_err(|e| nom_error_to_parsing_error_with_state(e, MalformedKind::IsoBmffBox, None))?;

    let Some(moov) = moov else {
        return Ok(None);
    };

    let ranges = moov.all_cmt_data_offsets();
    if ranges.is_empty() {
        return Err(ParsingErrorState::new(
            ParsingError::Failed {
                kind: MalformedKind::IsoBmffBox,
                message:
                    "CR3 file contains no EXIF data: Canon UUID box found but no CMT offsets available"
                        .into(),
            },
            None,
        ));
    }

    // Enforce the invariant that parse_moov_box's streaming::take guarantees:
    // when we reach here all CMT blocks are fully loaded. parse_cr3_exif_iter
    // (which slices each range out of the shared buffer upfront) relies on
    // this; better to surface a structured error here than to let Bytes::slice
    // panic later.
    for (block_id, range) in &ranges {
        if range.end > buf.len() {
            tracing::error!(
                block_id,
                range_end = range.end,
                buf_len = buf.len(),
                "CMT range extends beyond loaded buffer (parse_moov_box invariant violated)"
            );
            return Err(ParsingErrorState::new(
                ParsingError::Failed {
                    kind: MalformedKind::IsoBmffBox,
                    message: format!(
                        "CR3 CMT block {block_id} range {range:?} extends past loaded buffer ({} bytes)",
                        buf.len()
                    ),
                },
                None,
            ));
        }
    }

    Ok(Some(Cr3CmtRanges { ranges }))
}

pub(crate) fn extract_exif_data(
    state: Option<ParsingState>,
    buf: &[u8],
) -> Result<(Option<&[u8]>, Option<ParsingState>), ParsingErrorState> {
    let (data, state) = match state {
        Some(ParsingState::Cr3ExifSize(size)) => {
            let (_, data) = nom::bytes::streaming::take(size)(buf).map_err(|e| {
                nom_error_to_parsing_error_with_state(e, MalformedKind::IsoBmffBox, state.clone())
            })?;
            (Some(data), state)
        }
        None => {
            let (_, moov) = parse_moov_box(buf).map_err(|e| {
                nom_error_to_parsing_error_with_state(e, MalformedKind::IsoBmffBox, state)
            })?;

            if let Some(moov) = moov {
                if let Some(range) = moov.exif_data_offset() {
                    if range.end > buf.len() {
                        let state = ParsingState::Cr3ExifSize(range.len());
                        let clear_and_skip = ParsingError::ClearAndSkip(range.start);
                        return Err(ParsingErrorState::new(clear_and_skip, Some(state)));
                    } else {
                        (Some(&buf[range]), None)
                    }
                } else {
                    return Err(ParsingErrorState::new(
                        ParsingError::Failed {
                            kind: MalformedKind::IsoBmffBox,
                            message:
                                "CR3 file contains no EXIF data: Canon UUID box found but no CMT1 offset available"
                                    .into(),
                        },
                        None,
                    ));
                }
            } else {
                (None, None)
            }
        }
        _ => unreachable!(),
    };

    // For CR3 files, the CMT1 data already contains TIFF header, so we don't need to check for EXIF header
    let data = data.and_then(|x| {
        if TiffHeader::parse(x).is_ok() {
            Some(x)
        } else {
            // Try to find TIFF header if not at the beginning
            check_exif_header2(x).map(|x| x.0).ok()
        }
    });

    Ok((data, state))
}
