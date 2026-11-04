// Connection settings (connection string and DSN) and the HTTP client that
// talks to HexDB: POST /sql, GET /sql/tables, GET /sql/columns.

use crate::text::{connection_value, parse_connection_string};
use crate::OdbcError;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// Connection settings, from the connection string over the DSN's.
#[derive(Debug, Clone)]
pub struct Settings {
    pub dsn: Option<String>,
    pub driver: Option<String>,
    /// Base URL, e.g. http://127.0.0.1:7700.
    pub server: String,
    /// An API key or session token (sent as a bearer token).
    pub api_key: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub page_size: usize,
    /// Column size reported for string columns.
    pub max_string_length: usize,
    pub timeout: Duration,
    /// PEM file of certificates to trust instead of the system's.
    pub ca_file: Option<String>,
}

/// Settings keys read from a DSN (ODBC.INI) when the connection string names one.
const DSN_KEYS: &[&str] = &["SERVER", "APIKEY", "UID", "PWD", "PAGESIZE", "MAXSTRINGLENGTH", "TIMEOUT", "CAFILE"];

impl Settings {
    /// Combine a connection string with its DSN (connection string wins) and the
    /// separate user name and password SQLConnect takes.
    pub fn resolve(connection: &str, user: Option<String>, password: Option<String>) -> Result<Settings, OdbcError> {
        let mut values = parse_connection_string(connection);
        if let Some(dsn) = values.get("DSN").cloned().filter(|d| !d.is_empty()) {
            for key in DSN_KEYS {
                if !values.contains_key(*key) {
                    if let Some(v) = crate::dsn::read(&dsn, key) {
                        values.insert((*key).to_string(), v);
                    }
                }
            }
        }
        if let Some(u) = user.filter(|u| !u.is_empty()) {
            values.insert("UID".into(), u);
        }
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            values.insert("PWD".into(), p);
        }
        Settings::from_values(&values)
    }

    fn from_values(values: &BTreeMap<String, String>) -> Result<Settings, OdbcError> {
        let get = |k: &str| values.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let server = get("SERVER").or_else(|| get("URL")).ok_or_else(|| {
            OdbcError::new("08001", "No server given: add Server=http://host:7700 to the connection string or the DSN.")
        })?;
        // A hexdb:// or hexdbs:// URI fills in the keys given neither here nor in the DSN.
        let uri = if crate::uri::has_scheme(&server) {
            Some(crate::uri::parse(&server).map_err(|e| OdbcError::new("08001", format!("Invalid Server URI: {}.", e)))?)
        } else {
            None
        };
        let server = match &uri {
            Some(u) => u.url.clone(),
            None if server.contains("://") => server,
            None => format!("http://{}", server),
        };
        // Explicit keys win. An explicit API key replaces the URI's login, and an
        // explicit UID or PWD the URI's API key; the URI's password goes only
        // with the URI's login.
        let (explicit_key, explicit_user, explicit_password) = (get("APIKEY").or_else(|| get("TOKEN")), get("UID"), get("PWD"));
        let api_key = match (&explicit_key, &uri) {
            (Some(k), _) => Some(k.clone()),
            (None, Some(u)) if explicit_user.is_none() && explicit_password.is_none() => u.api_key.clone(),
            _ => None,
        };
        let uri_login = uri.as_ref().filter(|_| explicit_key.is_none()).and_then(|u| u.login.clone());
        let same_login = explicit_user.is_none() || explicit_user == uri_login;
        let password = explicit_password.or_else(|| uri.as_ref().filter(|_| explicit_key.is_none() && same_login).and_then(|u| u.password.clone()));
        let user = explicit_user.or(uri_login);
        let uri_number = |f: fn(&crate::uri::ConnectionUri) -> Option<u32>, default: usize| uri.as_ref().and_then(f).map_or(default, |n| n as usize);
        let number = |k: &str, default: usize, min: usize, max: usize| -> Result<usize, OdbcError> {
            match get(k) {
                None => Ok(default),
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| OdbcError::new("HY024", format!("{} must be a number from {} to {}.", k, min, max))),
            }
        };
        Ok(Settings {
            dsn: get("DSN"),
            driver: get("DRIVER"),
            server: server.trim_end_matches('/').to_string(),
            api_key,
            user,
            password,
            page_size: number("PAGESIZE", uri_number(|u| u.page_size, 1000), 1, 10_000)?,
            max_string_length: number("MAXSTRINGLENGTH", 4000, 1, 1 << 30)?,
            timeout: Duration::from_secs(number("TIMEOUT", uri_number(|u| u.timeout_seconds, 60), 1, u32::MAX as usize)? as u64),
            ca_file: get("CAFILE").or_else(|| uri.as_ref().and_then(|u| u.ca_file.clone())),
        })
    }

    /// The completed connection string SQLDriverConnect returns. Secrets (API
    /// key, password) are left out so applications don't save them.
    pub fn connection_string(&self) -> String {
        let mut parts = Vec::new();
        if let Some(dsn) = &self.dsn {
            parts.push(format!("DSN={}", connection_value(dsn)));
        } else if let Some(driver) = &self.driver {
            parts.push(format!("DRIVER={}", connection_value(driver)));
        }
        parts.push(format!("SERVER={}", connection_value(&self.server)));
        if let Some(user) = &self.user {
            parts.push(format!("UID={}", connection_value(user)));
        }
        if self.page_size != 1000 {
            parts.push(format!("PAGESIZE={}", self.page_size));
        }
        if self.max_string_length != 4000 {
            parts.push(format!("MAXSTRINGLENGTH={}", self.max_string_length));
        }
        if self.timeout != Duration::from_secs(60) {
            parts.push(format!("TIMEOUT={}", self.timeout.as_secs()));
        }
        if let Some(ca) = &self.ca_file {
            parts.push(format!("CAFILE={}", connection_value(ca)));
        }
        parts.join(";") + ";"
    }
}

/// An authenticated connection to one HexDB server.
pub struct Client {
    agent: ureq::Agent,
    pub base: String,
    token: String,
    pub settings: Settings,
    /// The signed-in user's login, for SQL_USER_NAME.
    pub login: String,
    /// The server's name and version, for SQL_SERVER_NAME and SQL_DBMS_VER.
    pub server_name: String,
    pub server_version: String,
}

impl Client {
    /// Connect: sign in (or use the API key) and check the credentials.
    pub fn connect(settings: Settings) -> Result<Client, OdbcError> {
        let mut config = ureq::Agent::config_builder().timeout_global(Some(settings.timeout)).http_status_as_error(false);
        let roots = match &settings.ca_file {
            Some(path) => {
                let pem = std::fs::read(path).map_err(|e| OdbcError::new("08001", format!("Couldn't read CAFile {}: {}", path, e)))?;
                let certs: Vec<ureq::tls::Certificate<'static>> = ureq::tls::parse_pem(&pem)
                    .filter_map(|item| match item {
                        Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                if certs.is_empty() {
                    return Err(OdbcError::new("08001", format!("CAFile {} has no certificates.", path)));
                }
                ureq::tls::RootCerts::Specific(std::sync::Arc::new(certs))
            }
            None => ureq::tls::RootCerts::PlatformVerifier,
        };
        config = config.tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build());
        let agent: ureq::Agent = config.build().into();
        let mut client = Client {
            agent,
            base: settings.server.clone(),
            token: String::new(),
            settings,
            login: String::new(),
            server_name: String::new(),
            server_version: String::new(),
        };

        client.token = match (&client.settings.api_key, &client.settings.user, &client.settings.password) {
            (Some(key), _, _) => key.clone(),
            (None, Some(user), Some(password)) => {
                let body = json!({ "login": user, "password": password, "return_token": true });
                let answer = client.request("POST", "/auth/login", Some(&body)).map_err(|e| e.at_connect())?;
                answer["token"].as_str().map(String::from).ok_or_else(|| {
                    OdbcError::new("28000", "The server didn't return a session token. If the account uses MFA, connect with an API key (ApiKey=...).")
                })?
            }
            _ => {
                return Err(OdbcError::new(
                    "28000",
                    "No credentials: give ApiKey=... (create one on the admin UI's Account page), or UID and PWD.",
                ))
            }
        };
        let me = client.request("GET", "/auth/me", None).map_err(|e| e.at_connect())?;
        client.login = me["login"].as_str().unwrap_or_default().to_string();
        if let Ok(health) = client.request("GET", "/health", None) {
            client.server_name = health["name"].as_str().unwrap_or_default().to_string();
            client.server_version = health["version"].as_str().unwrap_or_default().to_string();
        }
        Ok(client)
    }

    /// One request; a non-2xx answer becomes an error with HexDB's message.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, OdbcError> {
        let url = format!("{}{}", self.base, path);
        let auth = format!("Bearer {}", self.token);
        let result = match (method, body) {
            ("POST", Some(body)) => {
                let mut request = self.agent.post(&url).header("Content-Type", "application/json");
                if !self.token.is_empty() {
                    request = request.header("Authorization", &auth);
                }
                request.send_json(body)
            }
            _ => {
                let mut request = self.agent.get(&url);
                if !self.token.is_empty() {
                    request = request.header("Authorization", &auth);
                }
                request.call()
            }
        };
        let mut response = result.map_err(|e| OdbcError::new("08S01", format!("Couldn't reach HexDB at {}: {}", self.base, e)))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(json);
        }
        let message = json["error"]["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {}: {}", status, text.chars().take(200).collect::<String>()));
        let state = match status {
            401 => "28000",
            403 => "42000",
            404 => "42S02",
            400 | 422 => "42000",
            408 | 504 => "HYT00",
            _ => "HY000",
        };
        Err(OdbcError::new(state, message).with_native(status as i32))
    }

    /// One page of a statement's rows.
    pub fn sql(&self, sql: &str, params: &[Value], cursor: Option<&str>) -> Result<Value, OdbcError> {
        let mut body = json!({ "sql": sql, "params": params, "page_size": self.settings.page_size });
        if let Some(c) = cursor {
            body["cursor"] = json!(c);
        }
        self.request("POST", "/sql", Some(&body))
    }

    pub fn tables(&self) -> Result<Vec<String>, OdbcError> {
        let answer = self.request("GET", "/sql/tables", None)?;
        Ok(answer["tables"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(String::from)).collect())
    }

    pub fn columns(&self, table: &str) -> Result<Vec<Value>, OdbcError> {
        let path = format!("/sql/columns?table={}", percent_encode(table));
        let answer = self.request("GET", &path, None)?;
        Ok(answer["columns"].as_array().cloned().unwrap_or_default())
    }
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_takes_a_connection_uri() {
        let s = Settings::resolve("Driver={HexDB};Server=hexdbs://db.example.com/hexdb?apiKey=hxk_1&timeout=10&pageSize=500&caFile=C%3A%5Cca.pem", None, None).unwrap();
        assert_eq!(s.server, "https://db.example.com/hexdb");
        assert_eq!(s.api_key.as_deref(), Some("hxk_1"));
        assert_eq!(s.timeout, Duration::from_secs(10));
        assert_eq!(s.page_size, 500);
        assert_eq!(s.ca_file.as_deref(), Some(r"C:\ca.pem"));
        assert_eq!(s.user, None);
        // The completed connection string carries the HTTP URL, not the URI's credentials.
        assert!(s.connection_string().contains("SERVER=https://db.example.com/hexdb;"));
        assert!(!s.connection_string().contains("hxk_1"));

        let s = Settings::resolve("Server=hexdb://ada:p%40ss@127.0.0.1", None, None).unwrap();
        assert_eq!(s.server, "http://127.0.0.1:7700");
        assert_eq!((s.user.as_deref(), s.password.as_deref()), (Some("ada"), Some("p@ss")));

        let e = Settings::resolve("Server=hexdb://localhost?pageSize=0", None, None).unwrap_err();
        assert_eq!(e.state, "08001");
        assert!(e.message.contains("pageSize"), "{}", e.message);
    }

    #[test]
    fn explicit_keys_win_over_the_uri() {
        let uri = "Server=hexdbs://db.example.com?apiKey=hxk_uri&timeout=10&pageSize=500&caFile=uri.pem";
        let s = Settings::resolve(&format!("{};ApiKey=hxk_mine;Timeout=30;PageSize=50;CAFile=mine.pem", uri), None, None).unwrap();
        assert_eq!(s.api_key.as_deref(), Some("hxk_mine"));
        assert_eq!(s.timeout, Duration::from_secs(30));
        assert_eq!(s.page_size, 50);
        assert_eq!(s.ca_file.as_deref(), Some("mine.pem"));

        // A login given explicitly replaces the URI's API key, and an API key the URI's login.
        let s = Settings::resolve(uri, Some("grace".into()), Some("pw".into())).unwrap();
        assert_eq!((s.api_key.as_deref(), s.user.as_deref(), s.password.as_deref()), (None, Some("grace"), Some("pw")));
        let s = Settings::resolve("Server=hexdb://ada:secret@localhost;ApiKey=hxk_mine", None, None).unwrap();
        assert_eq!((s.api_key.as_deref(), s.user.as_deref(), s.password.as_deref()), (Some("hxk_mine"), None, None));

        // The URI's password goes with its login only.
        let s = Settings::resolve("Server=hexdb://ada:secret@localhost;UID=grace", None, None).unwrap();
        assert_eq!((s.user.as_deref(), s.password.as_deref()), (Some("grace"), None));
        let s = Settings::resolve("Server=hexdb://ada@localhost", None, Some("typed".into())).unwrap();
        assert_eq!((s.user.as_deref(), s.password.as_deref()), (Some("ada"), Some("typed")));
    }

    #[test]
    fn plain_servers_still_work() {
        assert_eq!(Settings::resolve("Server=127.0.0.1:7700", None, None).unwrap().server, "http://127.0.0.1:7700");
        assert_eq!(Settings::resolve("Server=https://db.example.com/;UID=a;PWD=b", None, None).unwrap().server, "https://db.example.com");
        assert_eq!(Settings::resolve("URL=http://h:7710", None, None).unwrap().server, "http://h:7710");
        let s = Settings::resolve("Server=http://h:7700;Timeout=5;PageSize=20", None, None).unwrap();
        assert_eq!((s.timeout, s.page_size), (Duration::from_secs(5), 20));
    }
}
