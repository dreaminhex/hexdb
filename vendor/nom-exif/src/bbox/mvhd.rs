use crate::time::OffsetDateTime;
use nom::{bytes::complete::take, number::complete::be_u32, Parser};

use super::{FullBoxHeader, ParseBody};

/// Represents a [movie header atom][1].
///
/// mvhd is a fullbox which contains version & flags.
///
/// atom-path: moov/mvhd
///
/// [1]: https://developer.apple.com/documentation/quicktime-file-format/movie_header_atom
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvhdBox {
    header: FullBoxHeader,

    /// seconds since midnight, January 1, 1904
    creation_time: u32,

    /// seconds since midnight, January 1, 1904
    modification_time: u32,

    /// The number of time units that pass per second in its time coordinate
    /// system.
    time_scale: u32,

    /// Indicates the duration of the movie in time scale units.
    ///
    /// # convert to seconds
    ///
    /// seconds = duration / time_scale
    duration: u32,
    // omit 76 bytes...
    next_track_id: u32,
}

impl MvhdBox {
    pub fn duration_ms(&self) -> u64 {
        ((self.duration as f64) / (self.time_scale as f64) * 1000_f64) as u64
    }

    /// Seconds since midnight, January 1, 1904, UTC (a u32 always fits).
    pub fn creation_time(&self) -> OffsetDateTime {
        OffsetDateTime::utc_after(jiff::civil::date(1904, 1, 1).at(0, 0, 0, 0), jiff::SignedDuration::from_secs(self.creation_time as i64))
            .expect("a 32-bit count of seconds after 1904 is in range")
    }
}

impl ParseBody<MvhdBox> for MvhdBox {
    fn parse_body(body: &[u8], header: FullBoxHeader) -> nom::IResult<&[u8], MvhdBox> {
        let (remain, (creation_time, modification_time, time_scale, duration, _, next_track_id)) =
            (be_u32, be_u32, be_u32, be_u32, take(76usize), be_u32).parse(body)?;

        Ok((
            remain,
            MvhdBox {
                header,
                creation_time,
                modification_time,
                time_scale,
                duration,
                next_track_id,
            },
        ))
    }
}
