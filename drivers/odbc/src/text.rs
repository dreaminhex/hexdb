// Text helpers: UTF-16 strings in and out of application buffers,
// connection strings, ODBC escape sequences, parameter markers and catalog
// search patterns.

use crate::ffi::*;
use std::collections::BTreeMap;

/// A UTF-16 string from the application: `len` characters, or up to a NUL
/// with SQL_NTS. `None` for a null pointer.
///
/// # Safety
/// `ptr` must be null or point to `len` readable characters (or a NUL-terminated string).
pub unsafe fn wide_in(ptr: *const SqlWChar, len: isize) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let len = if len == SQL_NTS as isize || len < 0 {
        let mut n = 0;
        while *ptr.add(n) != 0 {
            n += 1;
        }
        n
    } else {
        len as usize
    };
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)))
}

/// Write `text` as NUL-terminated UTF-16 into a buffer of `capacity`
/// characters; report the full length in characters. Returns true if it was
/// truncated.
///
/// # Safety
/// `out` must be null or writable for `capacity` characters; `len_out` null or writable.
pub unsafe fn wide_out_chars<L: TryFrom<usize>>(text: &str, out: *mut SqlWChar, capacity: isize, len_out: *mut L) -> bool {
    let units: Vec<u16> = text.encode_utf16().collect();
    if !len_out.is_null() {
        if let Ok(n) = L::try_from(units.len()) {
            *len_out = n;
        }
    }
    copy_wide(&units, out, capacity)
}

/// Like `wide_out_chars`, with the buffer size and reported length in bytes.
///
/// # Safety
/// `out` must be null or writable for `capacity_bytes` bytes; `len_out` null or writable.
pub unsafe fn wide_out_bytes<L: TryFrom<usize>>(text: &str, out: *mut SqlWChar, capacity_bytes: isize, len_out: *mut L) -> bool {
    let units: Vec<u16> = text.encode_utf16().collect();
    if !len_out.is_null() {
        if let Ok(n) = L::try_from(units.len() * 2) {
            *len_out = n;
        }
    }
    copy_wide(&units, out, capacity_bytes / 2)
}

unsafe fn copy_wide(units: &[u16], out: *mut SqlWChar, capacity: isize) -> bool {
    if out.is_null() || capacity <= 0 {
        return !units.is_empty() && !out.is_null();
    }
    let room = capacity as usize - 1;
    let n = units.len().min(room);
    std::ptr::copy_nonoverlapping(units.as_ptr(), out, n);
    *out.add(n) = 0;
    n < units.len()
}

/// Parse `KEY=value;KEY={value; with braces}` into upper-cased keys.
pub fn parse_connection_string(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' {
            i += 1;
        }
        let key = chars[start..i].iter().collect::<String>().trim().to_ascii_uppercase();
        if i >= chars.len() || chars[i] == ';' {
            i += 1;
            continue;
        }
        i += 1; // '='
        let mut value = String::new();
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        if i < chars.len() && chars[i] == '{' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '}' {
                    if i + 1 < chars.len() && chars[i + 1] == '}' {
                        value.push('}');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                value.push(chars[i]);
                i += 1;
            }
            while i < chars.len() && chars[i] != ';' {
                i += 1;
            }
        } else {
            while i < chars.len() && chars[i] != ';' {
                value.push(chars[i]);
                i += 1;
            }
            value = value.trim_end().to_string();
        }
        i += 1; // ';'
        if !key.is_empty() && !out.contains_key(&key) {
            out.insert(key, value);
        }
    }
    out
}

/// A connection string value, braced when it needs to be.
pub fn connection_value(value: &str) -> String {
    if value.contains([';', '{', '}', '=']) || value.starts_with(' ') || value.ends_with(' ') {
        format!("{{{}}}", value.replace('}', "}}"))
    } else {
        value.to_string()
    }
}

/// Walk SQL text, calling `f` for each character outside string literals,
/// quoted identifiers and comments (with its byte index).
fn scan(sql: &str, mut f: impl FnMut(usize, char)) {
    let bytes: Vec<(usize, char)> = sql.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (at, c) = bytes[i];
        match c {
            '\'' | '"' | '`' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i].1 == c {
                        if i + 1 < bytes.len() && bytes[i + 1].1 == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            '-' if i + 1 < bytes.len() && bytes[i + 1].1 == '-' => {
                while i < bytes.len() && bytes[i].1 != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < bytes.len() && bytes[i + 1].1 == '*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i].1 == '*' && bytes[i + 1].1 == '/') {
                    i += 1;
                }
                i += 1;
            }
            _ => f(at, c),
        }
        i += 1;
    }
}

/// Number of `?` parameter markers.
pub fn count_parameters(sql: &str) -> usize {
    let mut n = 0;
    scan(sql, |_, c| {
        if c == '?' {
            n += 1;
        }
    });
    n
}

/// Rewrite ODBC escape sequences into HexDB SQL:
/// `{d 'x'}`, `{t 'x'}`, `{ts 'x'}` become `'x'` (HexDB stores dates as text),
/// `{fn f(...)}` becomes `f(...)`, `{escape 'c'}` becomes `ESCAPE 'c'`, and
/// `{oj ...}` becomes `...`.
pub fn rewrite_escapes(sql: &str) -> String {
    // Find brace positions outside literals, then rewrite innermost first.
    let mut opens = Vec::new();
    let mut pairs = Vec::new();
    scan(sql, |at, c| match c {
        '{' => opens.push(at),
        '}' => {
            if let Some(open) = opens.pop() {
                pairs.push((open, at));
            }
        }
        _ => {}
    });
    if pairs.is_empty() {
        return sql.to_string();
    }
    // Rewrite from the outermost: replace each pair's braces and keyword.
    let mut out = String::with_capacity(sql.len());
    let mut skip_until = 0usize;
    let mut edits: Vec<(usize, usize, String)> = Vec::new(); // (start, end_exclusive, replacement)
    for (open, close) in &pairs {
        let inner = &sql[open + 1..*close];
        let trimmed = inner.trim_start();
        let lead = inner.len() - trimmed.len();
        let word: String = trimmed.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let keyword = word.to_ascii_lowercase();
        let replacement_prefix = match keyword.as_str() {
            "d" | "t" | "ts" | "fn" | "oj" => "",
            "escape" => "ESCAPE",
            _ => continue,
        };
        edits.push((*open, open + 1 + lead + word.len(), replacement_prefix.to_string()));
        edits.push((*close, close + 1, String::new()));
    }
    edits.sort_by_key(|e| e.0);
    let mut i = 0;
    for (start, end, replacement) in edits {
        if start < skip_until {
            continue;
        }
        out.push_str(&sql[i..start]);
        out.push_str(&replacement);
        i = end;
        skip_until = end;
    }
    out.push_str(&sql[i..]);
    out
}

/// A catalog search pattern (`%` any run, `_` one character, `\` escapes).
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    fn matches(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('%') => (0..=t.len()).any(|i| matches(&p[1..], &t[i..])),
            Some('_') => !t.is_empty() && matches(&p[1..], &t[1..]),
            Some('\\') if p.len() > 1 => t.first() == Some(&p[1]) && matches(&p[2..], &t[1..]),
            Some(c) => t.first() == Some(c) && matches(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    matches(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings_parse() {
        let c = parse_connection_string("Driver={HexDB ODBC};Server=http://h:7700; ApiKey={a;b}}c};uid=ada;Empty=;");
        assert_eq!(c["DRIVER"], "HexDB ODBC");
        assert_eq!(c["SERVER"], "http://h:7700");
        assert_eq!(c["APIKEY"], "a;b}c");
        assert_eq!(c["UID"], "ada");
        assert_eq!(c["EMPTY"], "");
        assert_eq!(connection_value("a;b}c"), "{a;b}}c}");
    }

    #[test]
    fn parameters_are_counted_outside_literals() {
        assert_eq!(count_parameters("SELECT * FROM t WHERE a = ? AND b = '?' AND \"c?\" = ? -- ?\n"), 2);
        assert_eq!(count_parameters("SELECT '?'''"), 0);
    }

    #[test]
    fn escapes_are_rewritten() {
        assert_eq!(rewrite_escapes("SELECT * FROM t WHERE d >= {d '2026-01-01'}"), "SELECT * FROM t WHERE d >=  '2026-01-01'");
        assert_eq!(rewrite_escapes("SELECT {fn UCASE(a)} FROM t"), "SELECT  UCASE(a) FROM t");
        assert_eq!(rewrite_escapes("a LIKE 'x!_%' {escape '!'}"), "a LIKE 'x!_%' ESCAPE '!'");
        assert_eq!(rewrite_escapes("SELECT '{d}' FROM t"), "SELECT '{d}' FROM t");
    }

    #[test]
    fn patterns_match() {
        assert!(pattern_matches("ord%", "orders"));
        assert!(pattern_matches("%", ""));
        assert!(pattern_matches("o_ders", "orders"));
        assert!(!pattern_matches("o\\_ders", "orders"));
        assert!(pattern_matches("my\\_table", "my_table"));
    }
}
