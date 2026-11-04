// HexDB connection URIs (MANUAL.md, Connection URIs):
// hexdb://[login[:password]@]host[:port][/path][?option=value&...], or
// hexdbs://... for HTTPS. Server= takes one, and its credentials and options
// fill in the keys the connection string and DSN leave out.

/// A parsed connection URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionUri {
    /// The HTTP base URL: http(s)://host[:port][/path], no trailing slash.
    pub url: String,
    pub login: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
    pub timeout_seconds: Option<u32>,
    pub page_size: Option<u32>,
    pub ca_file: Option<String>,
}

const HTTP_PORT: u32 = 7700;
const HTTPS_PORT: u32 = 443;
const MAX_PAGE_SIZE: u32 = 10_000;

/// True if the text starts with hexdb: or hexdbs: (any case).
pub fn has_scheme(text: &str) -> bool {
    let lower = text.get(..7).unwrap_or(text).to_ascii_lowercase();
    lower.starts_with("hexdb:") || lower.starts_with("hexdbs:")
}

/// Parse a connection URI; an invalid one is an error naming the problem.
pub fn parse(uri: &str) -> Result<ConnectionUri, String> {
    let (scheme, after) = uri.split_once(':').ok_or("not a hexdb:// or hexdbs:// URI")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "hexdb" && scheme != "hexdbs" {
        return Err("not a hexdb:// or hexdbs:// URI".into());
    }
    let rest = after.strip_prefix("//").ok_or_else(|| format!("no // after {}:", scheme))?;
    let secure = scheme == "hexdbs";
    if rest.contains('#') {
        return Err("a fragment (#...) isn't allowed".into());
    }
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let path = path.trim_end_matches('/');

    let (login, password, host_port) = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            let (login, password) = match userinfo.split_once(':') {
                Some((l, p)) => (decode(l, "the login")?, Some(decode(p, "the password")?)),
                None => (decode(userinfo, "the login")?, None),
            };
            if login.is_empty() {
                return Err("an empty login".into());
            }
            (Some(login), password, host_port)
        }
        None => (None, None, authority),
    };

    if host_port.contains(',') {
        return Err("more than one host; give one name that reaches the lattice's hexes".into());
    }
    let (host, port_text) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let close = bracketed.find(']').ok_or("an IPv6 address without a closing ]")?;
        let host = &host_port[..close + 2];
        let after = &bracketed[close + 1..];
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or("unexpected text after the IPv6 address")?),
        };
        if close == 0 {
            return Err("no host".into());
        }
        (host, port)
    } else {
        if host_port.matches(':').count() > 1 {
            return Err("an IPv6 address must be in brackets, as [::1]".into());
        }
        match host_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return Err("no host".into());
    }
    let host = host.to_ascii_lowercase();
    let port = match port_text {
        None => None,
        Some(p) => {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err("the port isn't a number".into());
            }
            match p.parse::<u32>() {
                Ok(n) if (1..=65_535).contains(&n) => Some(n),
                _ => return Err("the port is outside 1 to 65535".into()),
            }
        }
    };

    let mut api_key = None;
    let mut timeout_seconds = None;
    let mut page_size = None;
    let mut ca_file = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = decode(name, "an option name")?.to_ascii_lowercase();
        let value = decode(value, &format!("option {}", name))?;
        match name.as_str() {
            "apikey" if value.is_empty() => return Err("apiKey is empty".into()),
            "apikey" => api_key = Some(value),
            "timeout" => {
                timeout_seconds = Some(whole(&value).filter(|n| *n >= 1).ok_or("timeout isn't a positive whole number of seconds")?);
            }
            "pagesize" => {
                page_size = Some(whole(&value).filter(|n| (1..=MAX_PAGE_SIZE).contains(n)).ok_or("pageSize is outside 1 to 10,000")?);
            }
            "cafile" if value.is_empty() => return Err("caFile is empty".into()),
            "cafile" => ca_file = Some(value),
            _ => {} // options for other drivers
        }
    }
    if login.is_some() && api_key.is_some() {
        return Err("a login together with apiKey; use one or the other".into());
    }

    let port = port.unwrap_or(if secure { HTTPS_PORT } else { HTTP_PORT });
    let shown = if secure && port == HTTPS_PORT { String::new() } else { format!(":{}", port) };
    let url = format!("{}://{}{}{}", if secure { "https" } else { "http" }, host, shown, path);
    Ok(ConnectionUri { url, login, password, api_key, timeout_seconds, page_size, ca_file })
}

/// A whole number of ASCII digits.
fn whole(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Percent-decode as UTF-8; '+' stays '+'.
fn decode(text: &str, what: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            out.push(hex.ok_or_else(|| format!("bad percent-encoding in {}", what))?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("{} isn't UTF-8 once percent-decoded", what))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const VECTORS: &str = include_str!("../../testing/connection-uris.json");

    fn text(v: &Value) -> Option<String> {
        v.as_str().map(String::from)
    }

    fn number(v: &Value) -> Option<u32> {
        v.as_u64().map(|n| n as u32)
    }

    #[test]
    fn shared_vectors_parse() {
        let vectors: Value = serde_json::from_str(VECTORS).unwrap();
        let valid = vectors["valid"].as_array().unwrap();
        assert!(!valid.is_empty());
        for t in valid {
            let uri = t["uri"].as_str().unwrap();
            assert!(has_scheme(uri), "{}", uri);
            let got = parse(uri).unwrap_or_else(|e| panic!("{}: {}", uri, e));
            let want = ConnectionUri {
                url: t["url"].as_str().unwrap().to_string(),
                login: text(&t["login"]),
                password: text(&t["password"]),
                api_key: text(&t["apiKey"]),
                timeout_seconds: number(&t["timeoutSeconds"]),
                page_size: number(&t["pageSize"]),
                ca_file: text(&t["caFile"]),
            };
            assert_eq!(got, want, "{}", uri);
        }
    }

    #[test]
    fn shared_invalid_vectors_are_refused() {
        let vectors: Value = serde_json::from_str(VECTORS).unwrap();
        let invalid = vectors["invalid"].as_array().unwrap();
        assert!(!invalid.is_empty());
        for t in invalid {
            let uri = t["uri"].as_str().unwrap();
            let reason = t["reason"].as_str().unwrap();
            let error = parse(uri).expect_err(uri);
            // The error names the same thing the vector's reason does.
            let word = ["host", "apiKey", "timeout", "pageSize", "port", "fragment", "//"]
                .into_iter()
                .find(|w| reason.contains(w))
                .unwrap_or("hexdb");
            assert!(error.contains(word), "{}: {} (expected {})", uri, error, word);
        }
    }

    #[test]
    fn bad_encodings_are_refused() {
        assert!(parse("hexdb://ada:p%4@localhost").unwrap_err().contains("password"));
        assert!(parse("hexdb://localhost?apiKey=%ZZ").unwrap_err().contains("apikey"));
        assert!(parse("hexdb://::1").unwrap_err().contains("brackets"));
    }
}
