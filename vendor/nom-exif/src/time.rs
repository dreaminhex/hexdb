//! Date-times for EXIF and track metadata.
//!
//! HexDB's vendored copy: upstream nom-exif uses chrono, which is unmaintained
//! (RUSTSEC-2026-0543). This module keeps chrono's behaviour where callers can
//! see it, on top of jiff: values compare by instant (like chrono's
//! `DateTime<FixedOffset>`), `to_rfc3339` prints what chrono's did (fraction
//! only when there is one, 3, 6 or 9 digits; `+00:00`, never `Z`), and parsing
//! accepts what chrono's `%+` and `%Y:%m:%d %H:%M:%S %z` accepted in practice.

use jiff::{civil, tz::Offset, SignedDuration, Timestamp};
use std::fmt;

/// A date and time without an offset (EXIF's plain `DateTime` tags).
pub type NaiveDateTime = civil::DateTime;

/// A date and time at a fixed UTC offset.
#[derive(Clone, Copy)]
pub struct OffsetDateTime {
    local: civil::DateTime,
    offset: Offset,
}

impl OffsetDateTime {
    /// `local` at `offset`; None if that falls outside the supported range.
    pub fn new(local: civil::DateTime, offset: Offset) -> Option<Self> {
        offset.to_timestamp(local).ok().map(|_| OffsetDateTime { local, offset })
    }

    /// An instant, shown at `offset`.
    pub fn from_timestamp(timestamp: Timestamp, offset: Offset) -> Self {
        OffsetDateTime { local: offset.to_datetime(timestamp), offset }
    }

    /// Seconds since the Unix epoch, plus nanoseconds, as an instant in UTC.
    pub fn from_unix(seconds: i64, nanos: i32) -> Option<Self> {
        let timestamp = Timestamp::new(seconds, nanos).ok()?;
        Some(Self::from_timestamp(timestamp, Offset::UTC))
    }

    /// `base` (UTC) plus a duration: the epochs of MP4 (1904) and Matroska (2001) dates.
    pub fn utc_after(base: civil::DateTime, after: SignedDuration) -> Option<Self> {
        let start = Offset::UTC.to_timestamp(base).ok()?;
        Some(Self::from_timestamp(start.checked_add(after).ok()?, Offset::UTC))
    }

    /// The instant.
    pub fn timestamp(&self) -> Timestamp {
        self.offset.to_timestamp(self.local).expect("checked when made")
    }

    /// The date and time at this value's own offset, without it.
    pub fn naive_local(&self) -> civil::DateTime {
        self.local
    }

    pub fn offset(&self) -> Offset {
        self.offset
    }

    /// The same instant at another offset.
    pub fn with_offset(&self, offset: Offset) -> Self {
        Self::from_timestamp(self.timestamp(), offset)
    }

    /// RFC 3339, as chrono's `to_rfc3339` printed it.
    pub fn to_rfc3339(&self) -> String {
        let d = self.local;
        let mut out = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second());
        let nanos = d.subsec_nanosecond();
        if nanos != 0 {
            if nanos % 1_000_000 == 0 {
                out.push_str(&format!(".{:03}", nanos / 1_000_000));
            } else if nanos % 1_000 == 0 {
                out.push_str(&format!(".{:06}", nanos / 1_000));
            } else {
                out.push_str(&format!(".{:09}", nanos));
            }
        }
        out.push_str(&format_offset(self.offset));
        out
    }

    /// RFC 3339 / ISO 8601 with an offset, as chrono's `%+` parsed it:
    /// `2024-02-02T16:09:57+08:00`, `...+0800`, `...Z`, an optional fraction,
    /// and a space instead of the `T`.
    pub fn parse_rfc3339(s: &str) -> Option<Self> {
        let s = s.trim();
        let (local, rest) = parse_civil(s, b'-', true)?;
        let offset = parse_offset(rest)?;
        Self::new(local, offset)
    }

    /// An EXIF date (`2024:01:02 03:04:05`) followed by a space and an offset
    /// (`+08:00` or `+0800`), as chrono's `%Y:%m:%d %H:%M:%S %z` parsed it.
    pub fn parse_exif(s: &str) -> Option<Self> {
        let (local, rest) = parse_civil(s, b':', false)?;
        let rest = rest.strip_prefix(' ')?;
        Self::new(local, parse_offset(rest)?)
    }
}

/// An EXIF date without an offset (`2024:01:02 03:04:05`).
pub fn parse_exif_naive(s: &str) -> Option<civil::DateTime> {
    match parse_civil(s, b':', false)? {
        (local, "") => Some(local),
        _ => None,
    }
}

impl PartialEq for OffsetDateTime {
    /// Equal when they're the same instant, whatever their offsets (as in chrono).
    fn eq(&self, other: &Self) -> bool {
        self.timestamp() == other.timestamp()
    }
}

impl Eq for OffsetDateTime {}

impl PartialOrd for OffsetDateTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OffsetDateTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp().cmp(&other.timestamp())
    }
}

impl std::hash::Hash for OffsetDateTime {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.timestamp().hash(state);
    }
}

impl fmt::Debug for OffsetDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl fmt::Display for OffsetDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// `+08:00`, `-03:30`, `+00:00` (with seconds only if it has them).
fn format_offset(offset: Offset) -> String {
    let total = offset.seconds();
    let sign = if total < 0 { '-' } else { '+' };
    let total = total.unsigned_abs();
    let (h, m, s) = (total / 3600, total % 3600 / 60, total % 60);
    if s == 0 {
        format!("{}{:02}:{:02}", sign, h, m)
    } else {
        format!("{}{:02}:{:02}:{:02}", sign, h, m, s)
    }
}

/// Digits (1 to `max`) at the start of `s`, and the rest.
fn number(s: &str, max: usize) -> Option<(i64, &str)> {
    let len = s.bytes().take(max).take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return None;
    }
    Some((s[..len].parse().ok()?, &s[len..]))
}

fn expect(s: &str, c: u8) -> Option<&str> {
    (s.as_bytes().first() == Some(&c)).then(|| &s[1..])
}

/// `Y<sep>m<sep>d` then `T` (or a space, if `t_allowed`) or a space, then `H:M:S`
/// and an optional fraction. Returns the rest.
fn parse_civil(s: &str, sep: u8, t_allowed: bool) -> Option<(civil::DateTime, &str)> {
    let (year, s) = number(s, 4)?;
    let (month, s) = number(expect(s, sep)?, 2)?;
    let (day, s) = number(expect(s, sep)?, 2)?;
    let s = match s.as_bytes().first() {
        Some(b'T' | b't') if t_allowed => &s[1..],
        Some(b' ') => &s[1..],
        _ => return None,
    };
    let (hour, s) = number(s, 2)?;
    let (minute, s) = number(expect(s, b':')?, 2)?;
    let (second, mut s) = number(expect(s, b':')?, 2)?;
    let mut nanos = 0i64;
    if let Some(rest) = expect(s, b'.') {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let kept = &rest[..digits.min(9)];
        nanos = kept.parse::<i64>().ok()? * 10i64.pow(9 - kept.len() as u32);
        s = &rest[digits..];
    }
    // A leap second counts as the next second, as jiff (and HexDB's clock) do.
    let (second, leap) = if second == 60 { (59, true) } else { (second, false) };
    let local = civil::DateTime::new(year as i16, month as i8, day as i8, hour as i8, minute as i8, second as i8, nanos as i32).ok()?;
    let local = if leap { local.checked_add(SignedDuration::from_secs(1)).ok()? } else { local };
    Some((local, s))
}

/// `Z`, `+HH:MM`, `+HHMM` or `+HH`, and nothing after it.
fn parse_offset(s: &str) -> Option<Offset> {
    let s = s.trim_end();
    if s.eq_ignore_ascii_case("z") {
        return Some(Offset::UTC);
    }
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    let (hours, rest) = number(rest, 2)?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let (minutes, rest) = if rest.is_empty() { (0, rest) } else { number(rest, 2)? };
    if !rest.is_empty() || hours > 23 || minutes > 59 {
        return None;
    }
    Offset::from_seconds((sign * (hours * 3600 + minutes * 60)) as i32).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_what_chrono_printed() {
        let east8 = Offset::from_seconds(8 * 3600).unwrap();
        let t = OffsetDateTime::new(civil::date(2024, 2, 2).at(16, 9, 57, 0), east8).unwrap();
        assert_eq!(t.to_rfc3339(), "2024-02-02T16:09:57+08:00");
        assert_eq!(t.with_offset(Offset::UTC).to_rfc3339(), "2024-02-02T08:09:57+00:00");
        assert_eq!(t, t.with_offset(Offset::UTC), "the same instant");
        let ms = OffsetDateTime::new(civil::date(2024, 1, 2).at(3, 4, 5, 120_000_000), Offset::UTC).unwrap();
        assert_eq!(ms.to_rfc3339(), "2024-01-02T03:04:05.120+00:00");
        let us = OffsetDateTime::new(civil::date(2024, 1, 2).at(3, 4, 5, 123_456_000), Offset::UTC).unwrap();
        assert_eq!(us.to_rfc3339(), "2024-01-02T03:04:05.123456+00:00");
        let ns = OffsetDateTime::new(civil::date(2024, 1, 2).at(3, 4, 5, 1), Offset::from_seconds(-(3 * 3600 + 1800)).unwrap()).unwrap();
        assert_eq!(ns.to_rfc3339(), "2024-01-02T03:04:05.000000001-03:30");
    }

    #[test]
    fn parses_what_chrono_parsed() {
        let t = OffsetDateTime::parse_rfc3339("2024-02-02T16:09:57+08:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2024-02-02T16:09:57+08:00");
        assert_eq!(OffsetDateTime::parse_rfc3339("2024-02-02T16:09:57+0800"), Some(t));
        assert_eq!(OffsetDateTime::parse_rfc3339("2024-02-02T08:09:57Z"), Some(t));
        assert_eq!(OffsetDateTime::parse_rfc3339("2008-08-08T08:08:08Z").unwrap().to_rfc3339(), "2008-08-08T08:08:08+00:00");
        assert_eq!(OffsetDateTime::parse_rfc3339("2024-02-02T16:09:57.5+08:00").unwrap().to_rfc3339(), "2024-02-02T16:09:57.500+08:00");
        assert!(OffsetDateTime::parse_rfc3339("2024-02-02T16:09:57").is_none(), "needs an offset");
        assert!(OffsetDateTime::parse_rfc3339("2024-13-02T16:09:57Z").is_none());

        let e = OffsetDateTime::parse_exif("2023:07:09 20:36:33 +08:00").unwrap();
        assert_eq!(e.to_rfc3339(), "2023-07-09T20:36:33+08:00");
        assert_eq!(OffsetDateTime::parse_exif("2023:07:09 20:36:33 +0800"), Some(e));
        assert_eq!(parse_exif_naive("2023:07:09 20:36:33").unwrap().strftime("%Y-%m-%dT%H:%M:%S").to_string(), "2023-07-09T20:36:33");
        assert!(parse_exif_naive("0000:00:00 00:00:00").is_none());
        assert!(parse_exif_naive("2023:07:09 20:36:33 junk").is_none());
    }

    #[test]
    fn container_epochs() {
        // MP4: seconds since 1904-01-01 UTC; Matroska: nanoseconds since 2001-01-01 UTC.
        let mp4 = OffsetDateTime::utc_after(civil::date(1904, 1, 1).at(0, 0, 0, 0), SignedDuration::from_secs(3_789_732_600)).unwrap();
        assert_eq!(mp4.to_rfc3339(), "2024-02-02T15:30:00+00:00");
        let mkv = OffsetDateTime::utc_after(civil::date(2001, 1, 1).at(0, 0, 0, 0), SignedDuration::from_nanos(239_875_688_000_000_000)).unwrap();
        assert_eq!(mkv.to_rfc3339(), "2008-08-08T08:08:08+00:00");
    }
}
