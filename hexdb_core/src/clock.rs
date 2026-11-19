// Timestamps on jiff, in chrono's formats.
//
// HexDB used chrono until 2.6. Its timestamps are stored in documents and
// files, exchanged between hexes and returned by the HTTP API, so the text
// forms and parsing rules here copy chrono's exactly:
//
// - `rfc3339_millis` / `rfc3339_secs` are chrono's `to_rfc3339_opts(Millis |
//   Secs, true)`: exactly three fractional digits (truncated) or none, and `Z`.
// - `UtcTime` serializes like chrono's `DateTime<Utc>`: RFC 3339 with `Z` and
//   0, 3, 6 or 9 fractional digits, and reads what chrono's `FromStr` read
//   (`+00:00` and other offsets, a space for `T`, a trailing `UTC`).
// - `parse_rfc3339` accepts exactly what chrono's `parse_from_rfc3339` did.
// - `Civil::parse_naive` and `Civil::format` back the SQL date functions with
//   chrono's `NaiveDateTime::parse_from_str` rules and strftime output.
//
// Leap seconds: chrono read a seconds field of 60 as a leap second. `Civil`
// keeps that flag (so SQL output is unchanged); as a `Timestamp` it becomes
// the next second, which is what chrono's `timestamp_millis` gave.
//
// Range: jiff covers the years -9999 to 9999, chrono about ±262,000, so
// conversions of times beyond year 9999 give `None` here where chrono gave a
// value. No real timestamp is affected.

use std::fmt::Write;

use jiff::civil;
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// Whole seconds since the Unix epoch, now.
pub fn now_secs() -> i64 {
    Timestamp::now().as_second()
}

/// Whole seconds since the Unix epoch, rounded down (chrono's `timestamp()`;
/// jiff's `as_second` rounds toward zero, which differs before 1970).
pub fn unix_secs(t: Timestamp) -> i64 {
    t.as_second() - i64::from(t.subsec_nanosecond() < 0)
}

/// Whole milliseconds since the Unix epoch, rounded down (chrono's `timestamp_millis()`).
pub fn unix_millis(t: Timestamp) -> i64 {
    let ms = t.as_millisecond();
    if t.subsec_nanosecond() % 1_000_000 < 0 {
        ms - 1
    } else {
        ms
    }
}

/// The time `ms` milliseconds after the Unix epoch (`None` out of range).
pub fn from_millis(ms: i64) -> Option<Timestamp> {
    Timestamp::from_millisecond(ms).ok()
}

/// The time `secs` seconds after the Unix epoch (`None` out of range).
pub fn from_secs(secs: i64) -> Option<Timestamp> {
    Timestamp::from_second(secs).ok()
}

/// The date and time in UTC.
pub fn utc(t: Timestamp) -> civil::DateTime {
    Offset::UTC.to_datetime(t)
}

/// A UTC date and time back as a timestamp (`None` out of range).
pub fn from_utc(dt: civil::DateTime) -> Option<Timestamp> {
    Offset::UTC.to_timestamp(dt).ok()
}

/// RFC 3339 with exactly three fractional digits and `Z`, like `2026-11-19T10:00:00.120Z`.
pub fn rfc3339_millis(t: Timestamp) -> String {
    write_rfc3339(Civil::from(t), Digits::Millis)
}

/// RFC 3339 in whole seconds with `Z`, like `2026-11-19T10:00:00Z`.
pub fn rfc3339_secs(t: Timestamp) -> String {
    write_rfc3339(Civil::from(t), Digits::Secs)
}

/// RFC 3339 with `Z` and 0, 3, 6 or 9 fractional digits, as chrono serialized a `DateTime<Utc>`.
pub fn rfc3339_auto(t: Timestamp) -> String {
    write_rfc3339(Civil::from(t), Digits::Auto)
}

/// `t` formatted with a strftime-style pattern, in UTC (see `Civil::format`).
pub fn format(t: Timestamp, pattern: &str) -> String {
    Civil::from(t).format(pattern)
}

/// Reads an RFC 3339 date and time, accepting exactly what chrono's `DateTime::parse_from_rfc3339` did.
pub fn parse_rfc3339(s: &str) -> Option<Timestamp> {
    let (civil, offset) = rfc3339_parts(s)?;
    civil.to_timestamp(offset)
}

/// Reads an RFC 3339 date and time (chrono's `parse_from_rfc3339`) as a
/// civil time in UTC, keeping a leap second (chrono's `.naive_utc()`).
pub fn parse_rfc3339_utc(s: &str) -> Option<Civil> {
    let (civil, offset) = rfc3339_parts(s)?;
    let datetime = civil.datetime.checked_sub(SignedDuration::from_secs(i64::from(offset))).ok()?;
    Some(Civil { datetime, leap: civil.leap })
}

/// Whether `s` is an RFC 3339 date and time (chrono's `parse_from_rfc3339(s).is_ok()`).
pub fn is_rfc3339(s: &str) -> bool {
    rfc3339_parts(s).is_some()
}

/// Reads a date and time the way chrono's `DateTime<FixedOffset>::from_str`
/// (and so its serde) did: RFC 3339, relaxed.
pub fn parse_relaxed(s: &str) -> Option<Timestamp> {
    let (civil, offset) = relaxed_parts(s)?;
    civil.to_timestamp(offset)
}

// ---------------------------------------------------------------------------
// UtcTime: a timestamp that serializes like chrono's DateTime<Utc>
// ---------------------------------------------------------------------------

/// A point in time, serialized as chrono serialized `DateTime<Utc>` (see the top of this file).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UtcTime(pub Timestamp);

impl UtcTime {
    pub fn now() -> UtcTime {
        UtcTime(Timestamp::now())
    }

    pub fn from_millis(ms: i64) -> Option<UtcTime> {
        from_millis(ms).map(UtcTime)
    }

    pub fn from_secs(secs: i64) -> Option<UtcTime> {
        from_secs(secs).map(UtcTime)
    }

    /// Whole milliseconds since the Unix epoch, rounded down.
    pub fn millis(self) -> i64 {
        unix_millis(self.0)
    }

    /// Whole seconds since the Unix epoch, rounded down.
    pub fn secs(self) -> i64 {
        unix_secs(self.0)
    }

    /// `self - earlier` (negative when `earlier` is later).
    pub fn since(self, earlier: UtcTime) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }

    /// `self + d`, saturating at the ends of the range.
    pub fn plus(self, d: SignedDuration) -> UtcTime {
        UtcTime(self.0.saturating_add(d).unwrap_or(self.0))
    }

    /// `self - d`, saturating at the ends of the range.
    pub fn minus(self, d: SignedDuration) -> UtcTime {
        UtcTime(self.0.saturating_sub(d).unwrap_or(self.0))
    }
}

impl From<Timestamp> for UtcTime {
    fn from(t: Timestamp) -> UtcTime {
        UtcTime(t)
    }
}

/// Like chrono's `Debug` for `DateTime<Utc>` (which log lines and `{:?}` showed).
impl std::fmt::Debug for UtcTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&rfc3339_auto(self.0))
    }
}

/// Like chrono's `Display` for `DateTime<Utc>`: `2026-11-19 10:00:00.120 UTC`.
impl std::fmt::Display for UtcTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = rfc3339_auto(self.0);
        let (date, time) = text.trim_end_matches('Z').split_once('T').unwrap_or((&text, ""));
        write!(f, "{} {} UTC", date, time)
    }
}

impl Serialize for UtcTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&rfc3339_auto(self.0))
    }
}

impl<'de> Deserialize<'de> for UtcTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<UtcTime, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcTime;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an RFC 3339 formatted date and time string")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<UtcTime, E> {
                parse_relaxed(value).map(UtcTime).ok_or_else(|| E::custom(format!("'{}' isn't an RFC 3339 date and time", value)))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

// ---------------------------------------------------------------------------
// Civil: a date and time without a zone, as chrono's NaiveDateTime
// ---------------------------------------------------------------------------

/// A date and time (UTC or without a zone). `leap` marks a seconds field of
/// 60 (chrono's leap second), kept as :59 here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub datetime: civil::DateTime,
    pub leap: bool,
}

impl From<Timestamp> for Civil {
    fn from(t: Timestamp) -> Civil {
        Civil { datetime: utc(t), leap: false }
    }
}

const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

impl Civil {
    /// The seconds field as written (60 for a leap second).
    pub fn second(&self) -> u32 {
        self.datetime.second() as u32 + u32::from(self.leap)
    }

    /// Nanoseconds within the second.
    pub fn nanosecond(&self) -> u32 {
        self.datetime.subsec_nanosecond() as u32
    }

    /// Days since Sunday (0-6).
    pub fn weekday_from_sunday(&self) -> u32 {
        self.datetime.weekday().to_sunday_zero_offset() as u32
    }

    /// Seconds since the Unix epoch as UTC, rounded down, ignoring a leap second (chrono's `and_utc().timestamp()`).
    pub fn unix_secs(&self) -> i64 {
        let d = self.datetime.duration_since(civil::DateTime::constant(1970, 1, 1, 0, 0, 0, 0));
        d.as_secs() - i64::from(d.subsec_nanos() < 0)
    }

    /// As a timestamp, this being the time at `offset` seconds east of UTC (a leap second is the next second).
    fn to_timestamp(self, offset: i32) -> Option<Timestamp> {
        let t = Offset::from_seconds(offset).ok()?.to_timestamp(self.datetime).ok()?;
        if self.leap {
            t.checked_add(SignedDuration::from_secs(1)).ok()
        } else {
            Some(t)
        }
    }

    /// Formats with chrono's strftime rules, for the specifiers HexDB uses:
    /// `%Y %m %d %-d %H %M %S %a %A %b %B %f %.f %.3f %%`. `%S` shows a leap second as 60.
    pub fn format(&self, pattern: &str) -> String {
        let dt = &self.datetime;
        let mut out = String::with_capacity(pattern.len() + 16);
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let spec: String = match chars.next() {
                Some(c @ ('-' | '.')) => {
                    let mut s = String::from(c);
                    for next in chars.by_ref() {
                        s.push(next);
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    s
                }
                Some(c) => c.to_string(),
                None => String::new(),
            };
            let nano = self.nanosecond();
            let _ = match spec.as_str() {
                "Y" => write_year(&mut out, dt.year() as i32),
                "m" => write!(out, "{:02}", dt.month()),
                "d" => write!(out, "{:02}", dt.day()),
                "-d" => write!(out, "{}", dt.day()),
                "H" => write!(out, "{:02}", dt.hour()),
                "M" => write!(out, "{:02}", dt.minute()),
                "S" => write!(out, "{:02}", self.second()),
                "a" => out.write_str(&WEEKDAYS[self.weekday_from_sunday() as usize][..3]),
                "A" => out.write_str(WEEKDAYS[self.weekday_from_sunday() as usize]),
                "b" => out.write_str(&MONTHS[dt.month() as usize - 1][..3]),
                "B" => out.write_str(MONTHS[dt.month() as usize - 1]),
                "f" => write!(out, "{:09}", nano),
                ".3f" => write!(out, ".{:03}", nano / 1_000_000),
                ".f" => write_auto_fraction(&mut out, nano),
                "%" => out.write_str("%"),
                other => panic!("clock::format doesn't support %{}", other),
            };
        }
        out
    }

    /// Reads `s` with one of HexDB's SQL date patterns, following chrono's
    /// `NaiveDateTime::parse_from_str` (and `NaiveDate::parse_from_str`)
    /// rules: numbers may have leading whitespace and fewer digits, `%Y` may
    /// carry a sign, a space in the pattern matches any whitespace, `%.f` is optional.
    pub fn parse_naive(s: &str, pattern: &[Item]) -> Option<Civil> {
        let mut fields = Fields::default();
        let rest = parse_items(&mut fields, s, pattern)?;
        if !rest.is_empty() {
            return None;
        }
        fields.civil()
    }

    /// Reads a date (no time) with chrono's `NaiveDate::parse_from_str` rules: midnight.
    pub fn parse_naive_date(s: &str, pattern: &[Item]) -> Option<Civil> {
        let mut fields = Fields::default();
        let rest = parse_items(&mut fields, s, pattern)?;
        if !rest.is_empty() {
            return None;
        }
        fields.date_only()
    }
}

fn write_year(out: &mut String, year: i32) -> std::fmt::Result {
    if (0..=9999).contains(&year) {
        write!(out, "{:04}", year)
    } else {
        write!(out, "{:+05}", year)
    }
}

fn write_auto_fraction(out: &mut String, nano: u32) -> std::fmt::Result {
    if nano == 0 {
        Ok(())
    } else if nano.is_multiple_of(1_000_000) {
        write!(out, ".{:03}", nano / 1_000_000)
    } else if nano.is_multiple_of(1_000) {
        write!(out, ".{:06}", nano / 1_000)
    } else {
        write!(out, ".{:09}", nano)
    }
}

enum Digits {
    Secs,
    Millis,
    Auto,
}

fn write_rfc3339(c: Civil, digits: Digits) -> String {
    let dt = c.datetime;
    let mut out = String::with_capacity(32);
    let _ = write_year(&mut out, dt.year() as i32);
    let _ = write!(out, "-{:02}-{:02}T{:02}:{:02}:{:02}", dt.month(), dt.day(), dt.hour(), dt.minute(), c.second());
    let nano = c.nanosecond();
    let _ = match digits {
        Digits::Secs => Ok(()),
        Digits::Millis => write!(out, ".{:03}", nano / 1_000_000),
        Digits::Auto => write_auto_fraction(&mut out, nano),
    };
    out.push('Z');
    out
}

// ---------------------------------------------------------------------------
// Parsing, after chrono's format::parse and format::scan
// ---------------------------------------------------------------------------

/// One element of a parse pattern (chrono's `Item`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// `%Y`: up to 4 digits, or a sign and any number of digits.
    Year,
    /// `%m`
    Month,
    /// `%d`
    Day,
    /// `%H`
    Hour,
    /// `%M`
    Minute,
    /// `%S` (0-60)
    Second,
    /// `%.f`: an optional `.` and fractional seconds.
    Fraction,
    /// A literal character, matched exactly.
    Literal(char),
    /// Whitespace in the pattern: skips any whitespace.
    Space,
}

/// What a parse found (chrono's `Parsed`, for the fields HexDB uses).
#[derive(Default)]
struct Fields {
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
    nanosecond: Option<u32>,
}

/// Sets a field once; a different second value is an error (chrono's `set_if_consistent`).
fn set<T: PartialEq + Copy>(slot: &mut Option<T>, value: T) -> Option<()> {
    match slot {
        Some(old) if *old != value => None,
        _ => {
            *slot = Some(value);
            Some(())
        }
    }
}

impl Fields {
    fn civil(&self) -> Option<Civil> {
        let date = civil::Date::new(i16::try_from(self.year?).ok()?, self.month? as i8, self.day? as i8).ok()?;
        let hour = self.hour?;
        let minute = self.minute?;
        let (second, leap) = match self.second.unwrap_or(0) {
            s @ 0..=59 => (s, false),
            60 => (59, true),
            _ => return None,
        };
        let nano = match self.nanosecond {
            Some(n) if self.second.is_some() => n,
            Some(_) => return None,
            None => 0,
        };
        let time = civil::Time::new(hour as i8, minute as i8, second as i8, nano as i32).ok()?;
        Some(Civil { datetime: date.to_datetime(time), leap })
    }

    /// Only a date was asked for (chrono's `NaiveDate::parse_from_str`): midnight.
    fn date_only(&self) -> Option<Civil> {
        let date = civil::Date::new(i16::try_from(self.year?).ok()?, self.month? as i8, self.day? as i8).ok()?;
        Some(Civil { datetime: date.to_datetime(civil::Time::midnight()), leap: false })
    }
}

/// chrono's `scan::number`: `min` to `max` ASCII digits (more are left unread).
fn number(s: &str, min: usize, max: usize) -> Option<(&str, i64)> {
    let bytes = s.as_bytes();
    if bytes.len() < min {
        return None;
    }
    let mut n = 0i64;
    for (i, c) in bytes.iter().take(max).copied().enumerate() {
        if !c.is_ascii_digit() {
            return if i < min { None } else { Some((&s[i..], n)) };
        }
        n = n.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
    }
    Some((&s[max.min(bytes.len())..], n))
}

/// chrono's `scan::nanosecond`: 1 to 9 digits read, more skipped.
fn nanosecond(s: &str) -> Option<(&str, u32)> {
    const SCALE: [u32; 10] = [0, 100_000_000, 10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
    let before = s.len();
    let (s, v) = number(s, 1, 9)?;
    let v = (v as u32).checked_mul(SCALE[before - s.len()])?;
    Some((s.trim_start_matches(|c: char| c.is_ascii_digit()), v))
}

/// chrono's `scan::timezone_offset` (seconds east of UTC). `relaxed` takes
/// any run of colons and whitespace after the hours (chrono's
/// `colon_or_space`); otherwise exactly one colon. `Z` is UTC, minutes are
/// required, and U+2212 MINUS SIGN counts as `-`.
fn timezone_offset(mut s: &str, relaxed: bool) -> Option<(&str, i32)> {
    if let Some(b'Z' | b'z') = s.as_bytes().first() {
        return Some((&s[1..], 0));
    }
    let negative = match s.chars().next()? {
        '+' => false,
        '-' => true,
        '\u{2212}' => true,
        _ => return None,
    };
    s = &s[s.chars().next()?.len_utf8()..];
    let b = s.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    let hours = i32::from((b[0] - b'0') * 10 + (b[1] - b'0'));
    s = &s[2..];
    s = if relaxed {
        s.trim_start_matches(|c: char| c == ':' || c.is_whitespace())
    } else {
        s.strip_prefix(':')?
    };
    let b = s.as_bytes();
    if b.len() < 2 || !(b'0'..=b'5').contains(&b[0]) || !b[1].is_ascii_digit() {
        return None;
    }
    let minutes = i32::from((b[0] - b'0') * 10 + (b[1] - b'0'));
    let seconds = hours * 3600 + minutes * 60;
    Some((&s[2..], if negative { -seconds } else { seconds }))
}

/// chrono's `parse_internal` for the items above.
fn parse_items<'a>(fields: &mut Fields, mut s: &'a str, items: &[Item]) -> Option<&'a str> {
    for item in items {
        match *item {
            Item::Literal(c) => s = s.strip_prefix(c)?,
            Item::Space => s = s.trim_start(),
            Item::Fraction => {
                if let Some(rest) = s.strip_prefix('.') {
                    let (rest, nano) = nanosecond(rest)?;
                    set(&mut fields.nanosecond, nano)?;
                    s = rest;
                }
            }
            numeric => {
                s = s.trim_start();
                let (rest, v) = match numeric {
                    Item::Year => {
                        if let Some(digits) = s.strip_prefix('-') {
                            let (rest, v) = number(digits, 1, usize::MAX)?;
                            (rest, 0i64.checked_sub(v)?)
                        } else if let Some(digits) = s.strip_prefix('+') {
                            number(digits, 1, usize::MAX)?
                        } else {
                            number(s, 1, 4)?
                        }
                    }
                    _ => number(s, 1, 2)?,
                };
                s = rest;
                match numeric {
                    Item::Year => set(&mut fields.year, i32::try_from(v).ok()?)?,
                    Item::Month if (1..=12).contains(&v) => set(&mut fields.month, v as u32)?,
                    Item::Day if (1..=31).contains(&v) => set(&mut fields.day, v as u32)?,
                    Item::Hour if (0..=23).contains(&v) => set(&mut fields.hour, v as u32)?,
                    Item::Minute if (0..=59).contains(&v) => set(&mut fields.minute, v as u32)?,
                    Item::Second if (0..=60).contains(&v) => set(&mut fields.second, v as u32)?,
                    _ => return None,
                }
            }
        }
    }
    Some(s)
}

/// chrono's strict `parse_rfc3339`: the time (UTC or not) and its offset in seconds.
fn rfc3339_parts(s: &str) -> Option<(Civil, i32)> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let d = |i: usize| b[i].is_ascii_digit().then(|| i32::from(b[i] - b'0'));
    let year = d(0)? * 1000 + d(1)? * 100 + d(2)? * 10 + d(3)?;
    if b[4] != b'-' {
        return None;
    }
    let month = d(5)? * 10 + d(6)?;
    if b[7] != b'-' {
        return None;
    }
    let day = d(8)? * 10 + d(9)?;
    let date = civil::Date::new(year as i16, month as i8, day as i8).ok()?;
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let hour = d(11)? * 10 + d(12)?;
    if b[13] != b':' {
        return None;
    }
    let minute = d(14)? * 10 + d(15)?;
    if b[16] != b':' {
        return None;
    }
    let second = d(17)? * 10 + d(18)?;
    let (second, leap) = if second == 60 { (59, true) } else { (second, false) };
    let (rest, nano) = if b.get(19) == Some(&b'.') { nanosecond(&s[20..])? } else { (&s[19..], 0) };
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let time = civil::Time::new(hour as i8, minute as i8, second as i8, nano as i32).ok()?;
    let (rest, offset) = timezone_offset(rest, false)?;
    if !rest.is_empty() || offset.abs() >= 86_400 {
        return None;
    }
    Some((Civil { datetime: date.to_datetime(time), leap }, offset))
}

/// chrono's `parse_rfc3339_relaxed` followed by `Parsed::to_datetime` (its `FromStr`).
fn relaxed_parts(s: &str) -> Option<(Civil, i32)> {
    use Item::*;
    let mut fields = Fields::default();
    let mut s = parse_items(&mut fields, s, &[Year, Space, Literal('-'), Month, Space, Literal('-'), Day])?;
    s = match s.as_bytes().first()? {
        b't' | b'T' | b' ' => &s[1..],
        _ => return None,
    };
    s = parse_items(&mut fields, s, &[Hour, Space, Literal(':'), Minute, Space, Literal(':'), Second, Fraction, Space])?;
    s = s.trim_start();
    let (s, offset) = if s.len() >= 3 && s.as_bytes()[..3].eq_ignore_ascii_case(b"UTC") { (&s[3..], 0) } else { timezone_offset(s, true)? };
    if !s.trim_start().is_empty() || offset.abs() >= 86_400 {
        return None;
    }
    Some((fields.civil()?, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn formats_match_chrono() {
        let t = ts("2026-11-19T10:00:00.12Z");
        assert_eq!(rfc3339_millis(t), "2026-11-19T10:00:00.120Z");
        assert_eq!(rfc3339_secs(t), "2026-11-19T10:00:00Z");
        assert_eq!(rfc3339_auto(t), "2026-11-19T10:00:00.120Z");
        assert_eq!(rfc3339_auto(ts("2026-11-19T10:00:00Z")), "2026-11-19T10:00:00Z");
        assert_eq!(rfc3339_auto(ts("2026-11-19T10:00:00.1234567Z")), "2026-11-19T10:00:00.123456700Z");
        assert_eq!(rfc3339_auto(ts("2026-11-19T10:00:00.000123Z")), "2026-11-19T10:00:00.000123Z");
        assert_eq!(rfc3339_millis(ts("2026-11-19T10:00:00.9999Z")), "2026-11-19T10:00:00.999Z", "truncated, not rounded");
        assert_eq!(rfc3339_auto(ts("0001-01-01T00:00:00Z")), "0001-01-01T00:00:00Z");
        assert_eq!(rfc3339_auto(from_utc(civil::date(-1, 1, 1).at(0, 0, 0, 0)).unwrap()), "-0001-01-01T00:00:00Z");
        assert_eq!(format(t, "%a, %d %b %Y %H:%M:%S GMT"), "Thu, 19 Nov 2026 10:00:00 GMT");
        assert_eq!(format(t, "%B %-d / %A / %Y%m%d-%H%M%S / %Y-%m-%dT%H:%M:%S%.3fZ"), "November 19 / Thursday / 20261119-100000 / 2026-11-19T10:00:00.120Z");
    }

    #[test]
    fn utc_time_serializes_like_chrono() {
        let t = UtcTime(ts("2026-11-19T10:00:00.5Z"));
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"2026-11-19T10:00:00.500Z\"");
        for old in ["\"2026-11-19T10:00:00.500Z\"", "\"2026-11-19T10:00:00.5+00:00\"", "\"2026-11-19T11:00:00.500+01:00\"", "\"2026-11-19 10:00:00.5 UTC\""] {
            assert_eq!(serde_json::from_str::<UtcTime>(old).unwrap(), t, "{}", old);
        }
        assert!(serde_json::from_str::<UtcTime>("\"2026-11-19\"").is_err());
        assert_eq!(serde_json::from_str::<Option<UtcTime>>("null").unwrap(), None);
    }

    #[test]
    fn rfc3339_is_strict_like_chrono() {
        for good in ["2026-10-19T14:30:00Z", "2026-10-19t14:30:00z", "2026-10-19 14:30:00+05:30", "2026-10-19T14:30:00.1234567891234-00:00", "2026-10-19T14:30:00\u{2212}01:00"] {
            assert!(is_rfc3339(good), "{}", good);
        }
        for bad in ["2026-10-19", "2026-10-19T14:30Z", "2026-10-19T14:30:00", "2026-10-19T14:30:00+0100", "2026-10-19T14:30:00+01", "2026-10-19T14:30:00.Z", "2026-02-30T14:30:00Z", "2026-10-19T24:00:00Z", "2026-10-19T14:30:00+24:00", "2026-10-19T14:30:00Z ", "+2026-10-19T14:30:00Z"] {
            assert!(!is_rfc3339(bad), "{}", bad);
        }
        assert_eq!(parse_rfc3339("2026-10-19T14:30:00+01:00"), Some(ts("2026-10-19T13:30:00Z")));
        assert_eq!(parse_rfc3339("2016-12-31T23:59:60.5Z"), Some(ts("2017-01-01T00:00:00.5Z")), "a leap second is the next second");
    }

    #[test]
    fn naive_parsing_follows_chrono() {
        use Item::*;
        let pattern = [Year, Literal('-'), Month, Literal('-'), Day, Space, Hour, Literal(':'), Minute, Literal(':'), Second, Fraction];
        let civil = |s: &str| Civil::parse_naive(s, &pattern).map(|c| c.format("%Y-%m-%dT%H:%M:%S%.fZ"));
        assert_eq!(civil("2026-1-5   7:3:9.25").as_deref(), Some("2026-01-05T07:03:09.250Z"));
        assert_eq!(civil("26-10-19 14:30:00").as_deref(), Some("0026-10-19T14:30:00Z"));
        assert_eq!(civil("2026-10-19 23:59:60").as_deref(), Some("2026-10-19T23:59:60Z"));
        assert_eq!(civil("2026-10-19 14:30:00."), None);
        assert_eq!(civil("2026-10-19 14:30"), None);
        assert_eq!(civil("2026-13-19 14:30:00"), None);
    }

    #[test]
    fn rounding_is_down_like_chrono() {
        let t = ts("1969-12-31T23:59:59.9995Z");
        assert_eq!((unix_secs(t), unix_millis(t)), (-1, -1));
        let t = ts("1969-12-31T23:59:58.999Z");
        assert_eq!((unix_secs(t), unix_millis(t)), (-2, -1001));
        let t = ts("2026-11-19T10:00:00.9999Z");
        assert_eq!((unix_secs(t), unix_millis(t)), (1_795_082_400, 1_795_082_400_999));
    }
}
