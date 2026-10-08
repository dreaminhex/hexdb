// Conversions between HexDB's JSON values and the application's C buffers:
// result values into bound columns and SQLGetData targets (in pieces for
// long text), and bound parameters into JSON.

use crate::ffi::*;
use crate::OdbcError;
use serde_json::{Number, Value};
use std::ffi::c_void;

/// What `put` did.
#[derive(Debug)]
pub enum Put {
    Done,
    /// Written, with a warning (truncation).
    Warn(OdbcError),
    /// Everything was already returned by earlier SQLGetData calls.
    NoData,
}

/// Progress through one column's value across SQLGetData calls.
pub const DONE: usize = usize::MAX;

/// The C type SQL_C_DEFAULT means for a column of this SQL type.
pub fn default_c_type(sql_type: SqlSmallInt) -> SqlSmallInt {
    match sql_type {
        SQL_BIT => SQL_C_BIT,
        SQL_TINYINT => SQL_C_STINYINT,
        SQL_SMALLINT => SQL_C_SSHORT,
        SQL_INTEGER => SQL_C_SLONG,
        SQL_BIGINT => SQL_C_SBIGINT,
        SQL_REAL => SQL_C_FLOAT,
        SQL_FLOAT | SQL_DOUBLE => SQL_C_DOUBLE,
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR => SQL_C_CHAR,
        SQL_TYPE_DATE => SQL_C_TYPE_DATE,
        SQL_TYPE_TIME => SQL_C_TYPE_TIME,
        SQL_TYPE_TIMESTAMP => SQL_C_TYPE_TIMESTAMP,
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => SQL_C_BINARY,
        _ => SQL_C_WCHAR,
    }
}

/// A value as text: strings as they are, booleans as 1/0 in BIT columns (and
/// true/false elsewhere), numbers in JSON form, objects and arrays as JSON.
pub fn text(value: &Value, sql_type: SqlSmallInt) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) if sql_type == SQL_BIT => (if *b { "1" } else { "0" }).to_string(),
        other => other.to_string(),
    }
}

fn cast_error(value: &Value) -> OdbcError {
    OdbcError::new("22018", format!("Invalid character value for cast specification: {}", short(value)))
}

fn range_error(value: &Value) -> OdbcError {
    OdbcError::new("22003", format!("Numeric value out of range: {}", short(value)))
}

fn short(value: &Value) -> String {
    let s = value.to_string();
    if s.chars().count() > 60 {
        format!("{}...", s.chars().take(57).collect::<String>())
    } else {
        s
    }
}

/// The value as a number: (as f64, as an integer if it is one).
fn number(value: &Value) -> Result<(f64, Option<i128>), OdbcError> {
    match value {
        Value::Number(n) => Ok((n.as_f64().unwrap_or(0.0), n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from)))),
        Value::Bool(b) => Ok((if *b { 1.0 } else { 0.0 }, Some(*b as i128))),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i128>() {
                return Ok((i as f64, Some(i)));
            }
            match t.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok((f, None)),
                _ => Err(cast_error(value)),
            }
        }
        _ => Err(cast_error(value)),
    }
}

/// The value as an integer in [min, max]; a fractional part is dropped with
/// warning 01S07.
fn integer(value: &Value, min: i128, max: i128) -> Result<(i128, Option<OdbcError>), OdbcError> {
    let (f, exact) = number(value)?;
    if let Some(i) = exact {
        return if (min..=max).contains(&i) { Ok((i, None)) } else { Err(range_error(value)) };
    }
    let t = f.trunc();
    if t < min as f64 || t > max as f64 {
        return Err(range_error(value));
    }
    let warning = (t != f).then(|| OdbcError::new("01S07", "Fractional truncation"));
    Ok((t as i128, warning))
}

/// A date, and a time with nanoseconds, either of which may be missing.
pub type DateTimeParts = (Option<DateStruct>, Option<(TimeStruct, u32)>);

/// Date and time parts of an ISO 8601 string: `YYYY-MM-DD`,
/// `YYYY-MM-DD[T ]HH:MM[:SS[.fraction]][Z|±HH:MM]` or `HH:MM[:SS]`.
/// Time zones are ignored: the value is returned as written.
pub fn parse_datetime(s: &str) -> Option<DateTimeParts> {
    let s = s.trim();
    let num = |t: &str| -> Option<u32> { (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten() };
    let parse_time = |t: &str| -> Option<(TimeStruct, u32)> {
        let t = t.trim_end_matches('Z');
        let t = match t.rfind(['+', '-']) {
            Some(i) if i >= 5 => &t[..i],
            _ => t,
        };
        let (main, fraction) = match t.split_once('.') {
            Some((m, f)) => (m, Some(f)),
            None => (t, None),
        };
        let mut parts = main.split(':');
        let hour = num(parts.next()?)?;
        let minute = num(parts.next()?)?;
        let second = match parts.next() {
            Some(sec) => num(sec)?,
            None => 0,
        };
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        let nanos = match fraction {
            Some(f) => {
                let digits: String = f.chars().take(9).collect();
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                digits.parse::<u32>().ok()? * 10u32.pow(9 - digits.len() as u32)
            }
            None => 0,
        };
        Some((TimeStruct { hour: hour as u16, minute: minute as u16, second: second as u16 }, nanos))
    };
    if s.len() >= 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-') {
        let year: i32 = s[0..4].parse().ok()?;
        let month = num(&s[5..7])?;
        let day = num(&s[8..10])?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let date = DateStruct { year: year as i16, month: month as u16, day: day as u16 };
        let rest = &s[10..];
        if rest.is_empty() {
            return Some((Some(date), None));
        }
        let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
        return Some((Some(date), Some(parse_time(rest)?)));
    }
    Some((None, Some(parse_time(s)?)))
}

/// Write `value` into an application buffer as `c_type`. `progress` tracks
/// how much of a long value earlier SQLGetData calls returned.
///
/// # Safety
/// `ptr` must be null or writable for `buffer_len` bytes (or the fixed size of
/// `c_type`); `ind` null or writable.
pub unsafe fn put(
    value: &Value,
    sql_type: SqlSmallInt,
    c_type: SqlSmallInt,
    ptr: *mut c_void,
    buffer_len: SqlLen,
    ind: *mut SqlLen,
    progress: &mut usize,
) -> Result<Put, OdbcError> {
    if *progress == DONE {
        return Ok(Put::NoData);
    }
    let c_type = match c_type {
        SQL_C_DEFAULT | SQL_ARD_TYPE => default_c_type(sql_type),
        t => t,
    };
    if value.is_null() {
        if ind.is_null() {
            return Err(OdbcError::new("22002", "Indicator variable required but not supplied (the value is NULL)."));
        }
        *ind = SQL_NULL_DATA;
        *progress = DONE;
        return Ok(Put::Done);
    }
    let set_len = |n: usize| {
        if !ind.is_null() {
            *ind = n as SqlLen;
        }
    };
    macro_rules! fixed {
        ($t:ty, $v:expr) => {{
            let v: $t = $v;
            if !ptr.is_null() {
                std::ptr::write_unaligned(ptr as *mut $t, v);
            }
            set_len(std::mem::size_of::<$t>());
        }};
    }
    let mut warning = None;
    match c_type {
        SQL_C_WCHAR => {
            let units: Vec<u16> = text(value, sql_type).encode_utf16().collect();
            return Ok(chunk(&units, 2, true, ptr, buffer_len, ind, progress));
        }
        SQL_C_CHAR => {
            let bytes = text(value, sql_type).into_bytes();
            return Ok(chunk(&bytes, 1, true, ptr, buffer_len, ind, progress));
        }
        SQL_C_BINARY => {
            let bytes = text(value, sql_type).into_bytes();
            return Ok(chunk(&bytes, 1, false, ptr, buffer_len, ind, progress));
        }
        SQL_C_BIT => {
            let bit = match value {
                Value::Bool(b) => *b as u8,
                Value::String(s) if s.eq_ignore_ascii_case("true") => 1,
                Value::String(s) if s.eq_ignore_ascii_case("false") => 0,
                other => match integer(other, 0, 1) {
                    Ok((i, w)) => {
                        warning = w;
                        i as u8
                    }
                    Err(e) => return Err(e),
                },
            };
            fixed!(u8, bit)
        }
        SQL_C_STINYINT | SQL_C_TINYINT => {
            let (i, w) = integer(value, i8::MIN as i128, i8::MAX as i128)?;
            warning = w;
            fixed!(i8, i as i8)
        }
        SQL_C_UTINYINT => {
            let (i, w) = integer(value, 0, u8::MAX as i128)?;
            warning = w;
            fixed!(u8, i as u8)
        }
        SQL_C_SSHORT | SQL_C_SHORT => {
            let (i, w) = integer(value, i16::MIN as i128, i16::MAX as i128)?;
            warning = w;
            fixed!(i16, i as i16)
        }
        SQL_C_USHORT => {
            let (i, w) = integer(value, 0, u16::MAX as i128)?;
            warning = w;
            fixed!(u16, i as u16)
        }
        SQL_C_SLONG | SQL_C_LONG => {
            let (i, w) = integer(value, i32::MIN as i128, i32::MAX as i128)?;
            warning = w;
            fixed!(i32, i as i32)
        }
        SQL_C_ULONG => {
            let (i, w) = integer(value, 0, u32::MAX as i128)?;
            warning = w;
            fixed!(u32, i as u32)
        }
        SQL_C_SBIGINT => {
            let (i, w) = integer(value, i64::MIN as i128, i64::MAX as i128)?;
            warning = w;
            fixed!(i64, i as i64)
        }
        SQL_C_UBIGINT => {
            let (i, w) = integer(value, 0, u64::MAX as i128)?;
            warning = w;
            fixed!(u64, i as u64)
        }
        SQL_C_DOUBLE => fixed!(f64, number(value)?.0),
        SQL_C_FLOAT => {
            let f = number(value)?.0;
            if f.abs() > f32::MAX as f64 {
                return Err(range_error(value));
            }
            fixed!(f32, f as f32)
        }
        SQL_C_TYPE_DATE | SQL_C_DATE => {
            let Value::String(s) = value else { return Err(cast_error(value)) };
            let (Some(date), _) = parse_datetime(s).ok_or_else(|| cast_error(value))? else { return Err(cast_error(value)) };
            fixed!(DateStruct, date)
        }
        SQL_C_TYPE_TIME | SQL_C_TIME => {
            let Value::String(s) = value else { return Err(cast_error(value)) };
            let (_, Some((time, _))) = parse_datetime(s).ok_or_else(|| cast_error(value))? else { return Err(cast_error(value)) };
            fixed!(TimeStruct, time)
        }
        SQL_C_TYPE_TIMESTAMP | SQL_C_TIMESTAMP => {
            let Value::String(s) = value else { return Err(cast_error(value)) };
            let (date, time) = parse_datetime(s).ok_or_else(|| cast_error(value))?;
            let Some(date) = date else { return Err(cast_error(value)) };
            let (t, nanos) = time.unwrap_or_default();
            fixed!(
                TimestampStruct,
                TimestampStruct { year: date.year, month: date.month, day: date.day, hour: t.hour, minute: t.minute, second: t.second, fraction: nanos }
            )
        }
        other => return Err(OdbcError::new("HYC00", format!("Converting to C type {} isn't supported.", other))),
    }
    *progress = DONE;
    Ok(match warning {
        Some(w) => Put::Warn(w),
        None => Put::Done,
    })
}

/// Copy the next piece of text or bytes (`unit` bytes each), NUL-terminated
/// for text. The indicator gets the length still to return, in bytes.
unsafe fn chunk<T: Copy + Default>(
    data: &[T],
    unit: usize,
    terminate: bool,
    ptr: *mut c_void,
    buffer_len: SqlLen,
    ind: *mut SqlLen,
    progress: &mut usize,
) -> Put {
    let start = (*progress).min(data.len());
    let rest = &data[start..];
    if !ind.is_null() {
        *ind = (rest.len() * unit) as SqlLen;
    }
    let room = if buffer_len <= 0 { 0 } else { buffer_len as usize / unit };
    let room = if terminate { room.saturating_sub(1) } else { room };
    if ptr.is_null() || (buffer_len <= 0 && !rest.is_empty()) {
        // Only the length was asked for.
        return if rest.is_empty() {
            *progress = DONE;
            Put::Done
        } else {
            Put::Warn(OdbcError::new("01004", "String data, right truncated"))
        };
    }
    let n = rest.len().min(room);
    std::ptr::copy_nonoverlapping(rest.as_ptr(), ptr as *mut T, n);
    if terminate && buffer_len as usize >= unit {
        std::ptr::write_unaligned((ptr as *mut T).add(n), T::default());
    }
    if n < rest.len() {
        *progress = start + n;
        Put::Warn(OdbcError::new("01004", "String data, right truncated"))
    } else {
        *progress = DONE;
        Put::Done
    }
}

/// A bound parameter.
#[derive(Debug, Clone, Copy)]
pub struct ParamBinding {
    pub c_type: SqlSmallInt,
    pub sql_type: SqlSmallInt,
    pub ptr: *mut c_void,
    pub buffer_len: SqlLen,
    pub ind: *mut SqlLen,
}

fn is_numeric_sql(sql_type: SqlSmallInt) -> bool {
    matches!(sql_type, SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER | SQL_BIGINT | SQL_REAL | SQL_FLOAT | SQL_DOUBLE | SQL_NUMERIC | SQL_DECIMAL)
}

/// A bound parameter's current value as JSON.
///
/// # Safety
/// The binding's pointers must be valid as the application promised in SQLBindParameter.
pub unsafe fn param_value(p: &ParamBinding) -> Result<Value, OdbcError> {
    let ind = if p.ind.is_null() { None } else { Some(*p.ind) };
    match ind {
        Some(SQL_NULL_DATA) => return Ok(Value::Null),
        Some(n) if n == SQL_DATA_AT_EXEC || n <= SQL_LEN_DATA_AT_EXEC_OFFSET => {
            return Err(OdbcError::new("HYC00", "Data-at-execution parameters aren't supported; bind the value directly."))
        }
        _ => {}
    }
    if p.ptr.is_null() {
        return Ok(Value::Null);
    }
    let c_type = if p.c_type == SQL_C_DEFAULT { default_c_type(p.sql_type) } else { p.c_type };
    let read = |ptr: *mut c_void| ptr;
    let ptr = read(p.ptr);
    macro_rules! get {
        ($t:ty) => {
            std::ptr::read_unaligned(ptr as *const $t)
        };
    }
    let value = match c_type {
        SQL_C_CHAR | SQL_C_BINARY => {
            let len = match ind {
                Some(n) if n >= 0 => n as usize,
                _ if c_type == SQL_C_CHAR => {
                    let mut n = 0;
                    let bytes = ptr as *const u8;
                    while (p.buffer_len <= 0 || (n as SqlLen) < p.buffer_len) && *bytes.add(n) != 0 {
                        n += 1;
                    }
                    n
                }
                _ => p.buffer_len.max(0) as usize,
            };
            Value::String(String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len)).into_owned())
        }
        SQL_C_WCHAR => {
            let units = ptr as *const u16;
            let len = match ind {
                Some(n) if n >= 0 => n as usize / 2,
                _ => {
                    let mut n = 0;
                    while (p.buffer_len <= 0 || ((n * 2) as SqlLen) < p.buffer_len) && *units.add(n) != 0 {
                        n += 1;
                    }
                    n
                }
            };
            Value::String(String::from_utf16_lossy(std::slice::from_raw_parts(units, len)))
        }
        SQL_C_BIT => Value::Bool(get!(u8) != 0),
        SQL_C_STINYINT | SQL_C_TINYINT => Value::from(get!(i8)),
        SQL_C_UTINYINT => Value::from(get!(u8)),
        SQL_C_SSHORT | SQL_C_SHORT => Value::from(get!(i16)),
        SQL_C_USHORT => Value::from(get!(u16)),
        SQL_C_SLONG | SQL_C_LONG => Value::from(get!(i32)),
        SQL_C_ULONG => Value::from(get!(u32)),
        SQL_C_SBIGINT => Value::from(get!(i64)),
        SQL_C_UBIGINT => Value::from(get!(u64)),
        SQL_C_DOUBLE => Number::from_f64(get!(f64)).map(Value::Number).unwrap_or(Value::Null),
        SQL_C_FLOAT => Number::from_f64(get!(f32) as f64).map(Value::Number).unwrap_or(Value::Null),
        SQL_C_TYPE_DATE | SQL_C_DATE => {
            let d = get!(DateStruct);
            Value::String(format!("{:04}-{:02}-{:02}", d.year, d.month, d.day))
        }
        SQL_C_TYPE_TIME | SQL_C_TIME => {
            let t = get!(TimeStruct);
            Value::String(format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second))
        }
        SQL_C_TYPE_TIMESTAMP | SQL_C_TIMESTAMP => {
            let t = get!(TimestampStruct);
            let mut s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
            if t.fraction > 0 {
                s.push_str(format!(".{:09}", t.fraction).trim_end_matches('0'));
            }
            Value::String(s)
        }
        other => return Err(OdbcError::new("HYC00", format!("Parameters of C type {} aren't supported.", other))),
    };
    // Applications often bind text for numeric and bit parameters.
    Ok(match (&value, p.sql_type) {
        (Value::String(s), t) if is_numeric_sql(t) => match number(&Value::String(s.clone())) {
            Ok((_, Some(i))) if i64::try_from(i).is_ok() => Value::from(i as i64),
            Ok((f, _)) => Number::from_f64(f).map(Value::Number).unwrap_or(value),
            Err(_) => value,
        },
        (Value::String(s), SQL_BIT) => match s.trim() {
            "1" => Value::Bool(true),
            "0" => Value::Bool(false),
            t if t.eq_ignore_ascii_case("true") => Value::Bool(true),
            t if t.eq_ignore_ascii_case("false") => Value::Bool(false),
            _ => value,
        },
        _ => value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn datetimes_parse() {
        let (d, t) = parse_datetime("2026-03-04T05:06:07.25Z").unwrap();
        assert_eq!(d, Some(DateStruct { year: 2026, month: 3, day: 4 }));
        assert_eq!(t, Some((TimeStruct { hour: 5, minute: 6, second: 7 }, 250_000_000)));
        assert_eq!(parse_datetime("2026-03-04"), Some((Some(DateStruct { year: 2026, month: 3, day: 4 }), None)));
        assert!(parse_datetime("2026-03-04 10:00:00+02:00").unwrap().1.is_some());
        assert_eq!(parse_datetime("12:30").unwrap().1.unwrap().0, TimeStruct { hour: 12, minute: 30, second: 0 });
        assert!(parse_datetime("hello").is_none());
        assert!(parse_datetime("2026-13-01").is_none());
    }

    #[test]
    fn values_convert_into_buffers() {
        unsafe {
            let mut progress = 0;
            let mut out = 0i64;
            let mut ind: SqlLen = 0;
            assert!(matches!(put(&json!(42), SQL_BIGINT, SQL_C_DEFAULT, &mut out as *mut _ as *mut c_void, 8, &mut ind, &mut progress), Ok(Put::Done)));
            assert_eq!((out, ind), (42, 8));

            let mut progress = 0;
            let mut small = 0i32;
            let r = put(&json!(2.75), SQL_DOUBLE, SQL_C_SLONG, &mut small as *mut _ as *mut c_void, 4, &mut ind, &mut progress);
            assert!(matches!(r, Ok(Put::Warn(ref w)) if w.state == "01S07"));
            assert_eq!(small, 2);

            let mut progress = 0;
            assert!(put(&json!("abc"), SQL_WVARCHAR, SQL_C_SLONG, &mut small as *mut _ as *mut c_void, 4, &mut ind, &mut progress).is_err());

            // Long text arrives in pieces, then SQL_NO_DATA.
            let mut progress = 0;
            let mut buf = [0u16; 4];
            let r = put(&json!("hello"), SQL_WVARCHAR, SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, 8, &mut ind, &mut progress);
            assert!(matches!(r, Ok(Put::Warn(_))));
            assert_eq!((String::from_utf16_lossy(&buf[..3]), ind), ("hel".to_string(), 10));
            let r = put(&json!("hello"), SQL_WVARCHAR, SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, 8, &mut ind, &mut progress);
            assert!(matches!(r, Ok(Put::Done)));
            assert_eq!((String::from_utf16_lossy(&buf[..2]), ind), ("lo".to_string(), 4));
            let r = put(&json!("hello"), SQL_WVARCHAR, SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, 8, &mut ind, &mut progress);
            assert!(matches!(r, Ok(Put::NoData)));

            let mut progress = 0;
            let mut ts = TimestampStruct::default();
            put(&json!("2026-01-02T03:04:05Z"), SQL_WVARCHAR, SQL_C_TYPE_TIMESTAMP, &mut ts as *mut _ as *mut c_void, 16, &mut ind, &mut progress).unwrap();
            assert_eq!((ts.year, ts.month, ts.day, ts.hour, ts.minute, ts.second), (2026, 1, 2, 3, 4, 5));

            let mut progress = 0;
            assert!(put(&Value::Null, SQL_BIGINT, SQL_C_SBIGINT, &mut out as *mut _ as *mut c_void, 8, std::ptr::null_mut(), &mut progress).is_err());
            let mut progress = 0;
            put(&Value::Null, SQL_BIGINT, SQL_C_SBIGINT, &mut out as *mut _ as *mut c_void, 8, &mut ind, &mut progress).unwrap();
            assert_eq!(ind, SQL_NULL_DATA);
        }
    }

    #[test]
    fn parameters_convert_to_json() {
        unsafe {
            let mut text = *b"42\0";
            let mut ind: SqlLen = SQL_NTS as SqlLen;
            let p = ParamBinding { c_type: SQL_C_CHAR, sql_type: SQL_INTEGER, ptr: text.as_mut_ptr() as *mut c_void, buffer_len: 3, ind: &mut ind };
            assert_eq!(param_value(&p).unwrap(), json!(42));
            let p = ParamBinding { sql_type: SQL_VARCHAR, ..p };
            assert_eq!(param_value(&p).unwrap(), json!("42"));
            let mut wide: Vec<u16> = "Ada".encode_utf16().collect();
            let mut wind: SqlLen = 6;
            let p = ParamBinding { c_type: SQL_C_WCHAR, sql_type: SQL_WVARCHAR, ptr: wide.as_mut_ptr() as *mut c_void, buffer_len: 6, ind: &mut wind };
            assert_eq!(param_value(&p).unwrap(), json!("Ada"));
            let mut null_ind: SqlLen = SQL_NULL_DATA;
            let p = ParamBinding { ind: &mut null_ind, ..p };
            assert_eq!(param_value(&p).unwrap(), Value::Null);
            let mut ts = TimestampStruct { year: 2026, month: 1, day: 2, hour: 3, minute: 4, second: 5, fraction: 500_000_000 };
            let p = ParamBinding { c_type: SQL_C_TYPE_TIMESTAMP, sql_type: SQL_TYPE_TIMESTAMP, ptr: &mut ts as *mut _ as *mut c_void, buffer_len: 16, ind: std::ptr::null_mut() };
            assert_eq!(param_value(&p).unwrap(), json!("2026-01-02T03:04:05.5"));
        }
    }
}
