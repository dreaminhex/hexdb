// HexDB API: A2A push notifications
//
// A client can give a task a webhook (`CreateTaskPushNotificationConfig`, or
// `configuration.taskPushNotificationConfig` on SendMessage), and HexDB POSTs
// the task there once it finishes: `{"task": ...}` (a 1.0 StreamResponse,
// `application/a2a+json`), or the bare task in 0.3's form for configs made
// through 0.3's methods. The config's `token` goes in `X-A2A-Notification-Token`
// and its `authentication` in `Authorization: <scheme> <credentials>`, so the
// receiver can tell the call came from this task's agent.
//
// Webhooks are checked against server-side request forgery: HTTPS only, and
// every address the host resolves to must be public (not loopback, private,
// link-local, CGNAT, multicast or reserved), checked when the config is made
// and again when sending, with the connection pinned to the checked address
// so DNS can't change in between. Redirects aren't followed.
// `a2a.push_private_networks` allows plain HTTP and private addresses, for
// agents on the same network. Deliveries time out after 15 seconds and are
// tried three times (after 1 and 4 seconds) on network errors, 429 and 5xx.

use reqwest::Url;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tracing::{info, warn};

/// Longest webhook URL, and longest config ID or token.
const MAX_URL: usize = 2048;
const MAX_TEXT: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_AFTER: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// One webhook for a task.
#[derive(Clone, Debug)]
pub struct PushConfig {
    pub id: String,
    pub url: String,
    pub token: Option<String>,
    /// HTTP authentication scheme and credentials for `Authorization`.
    pub authentication: Option<(String, Option<String>)>,
    /// Made through 0.3's methods: notifications carry the 0.3 task.
    pub v03: bool,
    /// `a2a.push_private_networks` when the config was made.
    pub private_networks: bool,
}

fn text(value: &Value, what: &str) -> Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) if !s.is_empty() && s.len() <= MAX_TEXT => Ok(Some(s.clone())),
        _ => Err(format!("{} must be a string of 1-{} characters.", what, MAX_TEXT)),
    }
}

/// A config from a request: 1.0's flat TaskPushNotificationConfig, or 0.3's
/// `{"pushNotificationConfig": {...}}` (also accepted from 1.0 clients).
pub fn parse(value: &Value, v03: bool, private_networks: bool) -> Result<PushConfig, String> {
    let config = if value["pushNotificationConfig"].is_object() { &value["pushNotificationConfig"] } else { value };
    let url = config["url"].as_str().filter(|u| !u.is_empty() && u.len() <= MAX_URL).ok_or("A push notification config needs a url.")?;
    let authentication = match &config["authentication"] {
        Value::Null => None,
        auth => {
            // 1.0: {"scheme", "credentials"}; 0.3: {"schemes": [...], "credentials"}.
            let scheme = auth["scheme"].as_str().or_else(|| auth["schemes"].get(0).and_then(Value::as_str));
            let scheme = scheme.filter(|s| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)));
            let scheme = scheme.ok_or("authentication.scheme must be an HTTP authentication scheme, such as Bearer.")?;
            Some((scheme.to_string(), text(&auth["credentials"], "authentication.credentials")?))
        }
    };
    let id = text(&config["id"], "The config's id")?.unwrap_or_else(|| ulid::Ulid::generate().to_string());
    Ok(PushConfig { id, url: url.to_string(), token: text(&config["token"], "token")?, authentication, v03, private_networks })
}

/// A config as answers show it: credentials are never echoed back.
pub fn view(config: &PushConfig, task_id: &str) -> Value {
    let mut out = json!({ "id": config.id, "url": config.url });
    if let Some(token) = &config.token {
        out["token"] = json!(token);
    }
    if let Some((scheme, _)) = &config.authentication {
        out["authentication"] = if config.v03 { json!({ "schemes": [scheme] }) } else { json!({ "scheme": scheme }) };
    }
    if config.v03 {
        json!({ "taskId": task_id, "pushNotificationConfig": out })
    } else {
        out["taskId"] = json!(task_id);
        out
    }
}

/// Whether an address is on the public internet.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            // NAT64 (64:ff9b::/96) reaches the IPv4 address inside it.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240)
}

/// Check a webhook URL; returns it and the addresses it may be sent to.
pub async fn check(url: &str, private_networks: bool) -> Result<(Url, Vec<SocketAddr>), String> {
    let parsed = Url::parse(url).map_err(|_| format!("'{}' isn't a URL.", url))?;
    match parsed.scheme() {
        "https" => {}
        "http" if private_networks => {}
        _ => return Err("Webhooks must use https (a2a.push_private_networks allows http).".into()),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Put webhook credentials in authentication, not in the URL.".into());
    }
    let host = parsed.host_str().ok_or("The webhook URL has no host.")?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host.as_str(), port)).await.map_err(|_| format!("Couldn't resolve {}.", host))?.collect(),
    };
    if addrs.is_empty() {
        return Err(format!("{} has no addresses.", host));
    }
    if !private_networks && !addrs.iter().all(|a| is_public(a.ip())) {
        return Err(format!("{} isn't a public address; webhooks on private networks need a2a.push_private_networks.", host));
    }
    Ok((parsed, addrs))
}

/// Send a finished task to its webhooks, in the background.
pub fn notify(configs: Vec<PushConfig>, task: Value) {
    for config in configs {
        let task = task.clone();
        tokio::spawn(async move {
            if let Err(e) = deliver(&config, &task).await {
                warn!("📮 A2A push notification {} for task {} to {} failed: {}", config.id, task["id"].as_str().unwrap_or_default(), config.url, e);
            }
        });
    }
}

async fn deliver(config: &PushConfig, task: &Value) -> Result<(), String> {
    let (url, addrs) = check(&config.url, config.private_networks).await?;
    let host = url.host_str().unwrap_or_default().to_string();
    let client = hexdb_core::crypt::http_client()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        // Connect to the address just checked, whatever DNS says next.
        .resolve(&host, addrs[0])
        .build()
        .map_err(|e| e.to_string())?;
    let (body, content_type) = match config.v03 {
        true => (crate::a2a::to_v03(task.clone()), "application/json"),
        false => (json!({ "task": task }), "application/a2a+json"),
    };
    let body = body.to_string();
    let mut attempt = 0;
    loop {
        let mut request = client.post(url.clone()).header("content-type", content_type).body(body.clone());
        if let Some(token) = &config.token {
            request = request.header("x-a2a-notification-token", token);
        }
        if let Some((scheme, credentials)) = &config.authentication {
            request = request.header("authorization", match credentials {
                Some(c) => format!("{} {}", scheme, c),
                None => scheme.clone(),
            });
        }
        let outcome = request.send().await;
        let retry = match &outcome {
            Ok(r) if r.status().is_success() => {
                info!("📮 A2A push notification for task {} delivered to {}.", task["id"].as_str().unwrap_or_default(), config.url);
                return Ok(());
            }
            Ok(r) => r.status().is_server_error() || r.status().as_u16() == 429,
            Err(_) => true,
        };
        let reason = match outcome {
            Ok(r) => format!("answered {}", r.status()),
            Err(e) => e.to_string(),
        };
        match RETRY_AFTER.get(attempt) {
            Some(wait) if retry => tokio::time::sleep(*wait).await,
            _ => return Err(reason),
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_public() {
        for private in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "64:ff9b::a00:1", "224.0.0.1", "255.255.255.255"] {
            assert!(!is_public(private.parse().unwrap()), "{}", private);
        }
        for public in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public(public.parse().unwrap()), "{}", public);
        }
    }

    #[test]
    fn configs_in_both_shapes() {
        let v1 = parse(&json!({ "url": "https://a.example/hook", "token": "t", "authentication": { "scheme": "Bearer", "credentials": "secret" } }), false, false).unwrap();
        assert_eq!(v1.authentication, Some(("Bearer".into(), Some("secret".into()))));
        let shown = view(&v1, "task-1");
        assert_eq!(shown["taskId"], "task-1");
        assert!(!shown.to_string().contains("secret"), "credentials aren't echoed");
        let old = parse(&json!({ "pushNotificationConfig": { "id": "c1", "url": "https://a.example/hook", "authentication": { "schemes": ["Bearer"] } } }), true, false).unwrap();
        assert_eq!(old.id, "c1");
        assert_eq!(view(&old, "task-1")["pushNotificationConfig"]["authentication"]["schemes"][0], "Bearer");
        assert!(parse(&json!({ "token": "t" }), false, false).is_err(), "a url is required");
        assert!(parse(&json!({ "url": "https://a.example", "authentication": { "scheme": "Bearer x" } }), false, false).is_err());
    }

    #[tokio::test]
    async fn webhooks_must_be_public_https() {
        assert!(check("http://8.8.8.8/hook", false).await.is_err(), "http");
        assert!(check("https://127.0.0.1/hook", false).await.is_err(), "loopback");
        assert!(check("https://[::1]/hook", false).await.is_err(), "loopback v6");
        assert!(check("https://user:pw@8.8.8.8/hook", false).await.is_err(), "credentials in the URL");
        assert!(check("https://8.8.8.8/hook", false).await.is_ok());
        assert!(check("http://127.0.0.1:9000/hook", true).await.is_ok(), "allowed on private networks");
    }
}
