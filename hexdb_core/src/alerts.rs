// HexDB Core Alerts: flagged security reviews sent beyond email
//
// Besides email (`crate::email`), a review at or above
// `security.review.notify_severity` goes to every alert channel that's set
// up in the lattice configuration (all of it editable on the admin UI's
// Security reviews page):
//
//   PagerDuty   alerts.pagerduty.routing_key (an Events API v2 integration
//               key): triggers an incident, deduplicated per review.
//   SMS         alerts.twilio.account_sid, .auth_token, .from, .to (numbers
//               in E.164, separated by commas): a short text through Twilio.
//   Chat        alerts.chat.webhook_urls: incoming webhook URLs, one or more;
//               each gets the format its host expects: Slack (and anything
//               unrecognized, which Mattermost and Rocket.Chat accept),
//               Microsoft Teams (Workflows or the older connectors: an
//               Adaptive Card), or Discord.
//   Webhook     alerts.webhook.url: the review as JSON, signed with
//               HMAC-SHA256 when alerts.webhook.signing_secret is set
//               (`X-HexDB-Signature: sha256=<hex>` over
//               "<X-HexDB-Timestamp>.<body>").
//   Opsgenie    alerts.opsgenie.api_key (an API integration key) and
//               alerts.opsgenie.region (us or eu): an alert, deduplicated
//               per review.
//
// The hex that ran the review sends them (the Oracle, or the Overseer), one
// channel at a time, each with its own timeout; a channel that fails doesn't
// stop the others, and the review records what each one did. Every request
// is built by a function that doesn't touch the network, so the formats are
// unit-tested. Credentials never appear in errors.

use crate::review::Review;
use crate::HexDBEngine;
use anyhow::{anyhow, bail, Result};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::Duration;

/// How long one channel may take.
const TIMEOUT: Duration = Duration::from_secs(15);
const PAGERDUTY_URL: &str = "https://events.pagerduty.com/v2/enqueue";
/// Twilio splits longer texts into several messages (and bills each).
const SMS_CHARS: usize = 300;
/// Discord refuses longer messages.
const DISCORD_CHARS: usize = 2000;
/// Slack and Teams take more, but an alert should be short.
const CHAT_CHARS: usize = 3500;
pub const CHANNELS: &[&str] = &["pagerduty", "sms", "chat", "webhook", "opsgenie"];

/// What one channel did with an alert.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delivery {
    /// pagerduty, sms, chat, webhook or opsgenie.
    pub channel: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An alert, in the words every channel uses.
#[derive(Debug, Clone)]
pub struct Alert {
    /// Deduplicates retries in PagerDuty and Opsgenie.
    pub key: String,
    /// low, medium or high.
    pub severity: String,
    /// One line.
    pub title: String,
    /// A paragraph or two: the summary and each finding.
    pub text: String,
    /// The admin UI's Security reviews page, when oauth.resource names the lattice's URL.
    pub link: Option<String>,
    pub lattice: String,
    pub hex: String,
    /// The whole review, for the generic webhook and as details elsewhere.
    pub review: Value,
}

impl Alert {
    pub fn from_review(engine: &HexDBEngine, review: &Review) -> Alert {
        let iso = |ms: i64| Utc.timestamp_millis_opt(ms).single().map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default();
        let count = review.findings.len();
        let title = format!("HexDB security review of '{}': {} severity, {} finding{}", engine.config.network.lattice_name, review.severity, count, if count == 1 { "" } else { "s" });
        let mut text = format!("{} to {}, {} audit events. {}", iso(review.window_from), iso(review.window_to), review.events, review.summary.trim());
        for (i, f) in review.findings.iter().enumerate() {
            text.push_str(&format!("\n{}. [{}] {}: {}", i + 1, f.severity, f.title, f.evidence));
        }
        Alert {
            key: format!("hexdb-security-review-{}", review.id),
            severity: review.severity.clone(),
            title,
            text,
            link: link(engine),
            lattice: engine.config.network.lattice_name.clone(),
            hex: review.hex.clone(),
            review: serde_json::to_value(review).unwrap_or(Value::Null),
        }
    }

    /// The alert behind "Send a test alert".
    pub fn test(engine: &HexDBEngine, by: &str) -> Alert {
        Alert {
            key: format!("hexdb-alert-test-{}", ulid::Ulid::generate()),
            severity: "low".into(),
            title: format!("HexDB test alert from '{}'", engine.config.network.lattice_name),
            text: format!("Sent by {} from the Security reviews page. Alerts for flagged security reviews will arrive here.", by),
            link: link(engine),
            lattice: engine.config.network.lattice_name.clone(),
            hex: engine.name.clone(),
            review: json!({ "test": true, "sent_by": by }),
        }
    }

    /// Title, text and link, cut to `max` characters.
    fn plain(&self, max: usize) -> String {
        let mut out = format!("{}\n{}", self.title, self.text);
        if let Some(link) = &self.link {
            out.push_str(&format!("\n{}", link));
        }
        cut(&out, max)
    }
}

fn link(engine: &HexDBEngine) -> Option<String> {
    let base = engine.config_value("oauth.resource");
    let base = base.as_str().map(|s| s.trim().trim_end_matches('/')).filter(|s| !s.is_empty())?;
    Some(format!("{}/ui/security-reviews", base))
}

/// At most `max` characters, with an ellipsis when cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One HTTP request, ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Json(Value),
    /// application/x-www-form-urlencoded (Twilio).
    Form(Vec<(String, String)>),
}

/// Which chat service a webhook URL belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Chat {
    Slack,
    Teams,
    Discord,
}

pub fn chat_kind(url: &str) -> Chat {
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{}", domain));
    if under("discord.com") || under("discordapp.com") {
        Chat::Discord
    } else if under("webhook.office.com") || under("logic.azure.com") || under("powerplatform.com") || under("office.com") {
        Chat::Teams
    } else {
        Chat::Slack
    }
}

pub fn pagerduty(alert: &Alert, routing_key: &str) -> Request {
    let severity = match alert.severity.as_str() {
        "high" => "critical",
        "medium" => "warning",
        _ => "info",
    };
    let mut body = json!({
        "routing_key": routing_key,
        "event_action": "trigger",
        "dedup_key": alert.key,
        "payload": {
            "summary": cut(&alert.title, 1024),
            "source": format!("{} ({})", alert.lattice, alert.hex),
            "severity": severity,
            "component": "hexdb",
            "group": alert.lattice,
            "class": "security.review",
            "custom_details": { "text": alert.text, "review": alert.review },
        },
    });
    if let Some(link) = &alert.link {
        body["links"] = json!([{ "href": link, "text": "Security reviews" }]);
    }
    Request { url: PAGERDUTY_URL.into(), headers: Vec::new(), body: Body::Json(body) }
}

/// One request per recipient number.
pub fn twilio(alert: &Alert, account_sid: &str, auth_token: &str, from: &str, to: &[String]) -> Vec<Request> {
    use base64::Engine as _;
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", account_sid, auth_token)));
    let mut text = format!("{}. {}", alert.title, alert.review["summary"].as_str().unwrap_or(&alert.text));
    if let Some(link) = &alert.link {
        // The link matters more than the end of the summary.
        text = format!("{} {}", cut(&text, SMS_CHARS.saturating_sub(link.chars().count() + 1)), link);
    }
    let text = cut(&text, SMS_CHARS);
    to.iter()
        .map(|number| Request {
            url: format!("https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json", account_sid),
            headers: vec![("authorization".into(), auth.clone())],
            body: Body::Form(vec![("To".into(), number.clone()), ("From".into(), from.into()), ("Body".into(), text.clone())]),
        })
        .collect()
}

pub fn chat(alert: &Alert, url: &str) -> Request {
    let body = match chat_kind(url) {
        Chat::Slack => json!({ "text": alert.plain(CHAT_CHARS) }),
        Chat::Discord => json!({ "content": alert.plain(DISCORD_CHARS), "allowed_mentions": { "parse": [] } }),
        Chat::Teams => {
            let mut card = vec![
                json!({ "type": "TextBlock", "text": alert.title, "weight": "Bolder", "size": "Medium", "wrap": true }),
                json!({ "type": "TextBlock", "text": cut(&alert.text, CHAT_CHARS), "wrap": true }),
            ];
            if let Some(link) = &alert.link {
                card.push(json!({ "type": "ActionSet", "actions": [{ "type": "Action.OpenUrl", "title": "Security reviews", "url": link }] }));
            }
            json!({
                "type": "message",
                "attachments": [{
                    "contentType": "application/vnd.microsoft.card.adaptive",
                    "contentUrl": null,
                    "content": { "$schema": "http://adaptivecards.io/schemas/adaptive-card.json", "type": "AdaptiveCard", "version": "1.4", "body": card },
                }],
            })
        }
    };
    Request { url: url.into(), headers: Vec::new(), body: Body::Json(body) }
}

/// The generic webhook: `{"event", "lattice", "hex", "severity", "title", "text", "link", "review"}`,
/// signed over "<timestamp>.<body>" when there's a secret. `now` is in seconds.
pub fn webhook(alert: &Alert, url: &str, signing_secret: Option<&str>, now: i64) -> Request {
    let event = if alert.review.get("test").is_some() { "alert.test" } else { "security.review" };
    let body = json!({
        "event": event,
        "lattice": alert.lattice,
        "hex": alert.hex,
        "severity": alert.severity,
        "title": alert.title,
        "text": alert.text,
        "link": alert.link,
        "review": alert.review,
    });
    let mut headers = vec![("x-hexdb-event".to_string(), event.to_string()), ("x-hexdb-timestamp".to_string(), now.to_string())];
    if let Some(secret) = signing_secret {
        headers.push(("x-hexdb-signature".into(), format!("sha256={}", sign(secret, now, &body.to_string()))));
    }
    Request { url: url.into(), headers, body: Body::Json(body) }
}

/// HMAC-SHA256 of "<timestamp>.<body>", in hex.
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(format!("{}.{}", timestamp, body).as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn opsgenie(alert: &Alert, api_key: &str, region: &str) -> Request {
    let priority = match alert.severity.as_str() {
        "high" => "P1",
        "medium" => "P3",
        _ => "P5",
    };
    let host = if region == "eu" { "api.eu.opsgenie.com" } else { "api.opsgenie.com" };
    let mut details = json!({ "lattice": alert.lattice, "hex": alert.hex, "severity": alert.severity });
    if let Some(link) = &alert.link {
        details["link"] = json!(link);
    }
    Request {
        url: format!("https://{}/v2/alerts", host),
        headers: vec![("authorization".into(), format!("GenieKey {}", api_key))],
        body: Body::Json(json!({
            "message": cut(&alert.title, 130),
            "alias": cut(&alert.key, 512),
            "description": cut(&alert.text, 15_000),
            "priority": priority,
            "source": "HexDB",
            "tags": ["hexdb", "security-review"],
            "details": details,
        })),
    }
}

/// Every number in a list, checked as E.164 (+ and 8 to 15 digits).
pub fn phone_numbers(list: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list.split([',', ';', '\n']).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let number: String = item.chars().filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.')).collect();
        let digits = number.strip_prefix('+').unwrap_or_default();
        if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
            bail!("'{}' isn't a phone number in international format (+ and the country code, e.g. +15551234567)", item);
        }
        out.push(number);
    }
    Ok(out)
}

/// Webhook URLs in a list (spaces, commas or lines between them), each http(s).
pub fn webhook_urls(list: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (i, item) in list.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).enumerate() {
        let ok = reqwest::Url::parse(item).is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some());
        if !ok || crate::lattice_config::is_metadata_host(item) {
            // The URL is the credential: name it by position only.
            bail!("chat webhook URL {} isn't an http(s) URL", i + 1);
        }
        out.push(item.to_string());
    }
    Ok(out)
}

impl HexDBEngine {
    fn alert_text(&self, key: &str) -> String {
        self.config_value(key).as_str().map(str::trim).unwrap_or_default().to_string()
    }

    /// The channels set up, by name (for the admin UI; no credentials).
    pub fn alert_channels(&self) -> Vec<&'static str> {
        let has = |key: &str| self.config_secret(key).is_some();
        let mut out = Vec::new();
        if has("alerts.pagerduty.routing_key") {
            out.push("pagerduty");
        }
        if has("alerts.twilio.auth_token") && !self.alert_text("alerts.twilio.account_sid").is_empty() && !self.alert_text("alerts.twilio.to").is_empty() {
            out.push("sms");
        }
        if has("alerts.chat.webhook_urls") {
            out.push("chat");
        }
        if !self.alert_text("alerts.webhook.url").is_empty() {
            out.push("webhook");
        }
        if has("alerts.opsgenie.api_key") {
            out.push("opsgenie");
        }
        out
    }

    /// The requests a channel makes for an alert (an error when it's set up wrong).
    fn alert_requests(&self, channel: &str, alert: &Alert) -> Result<Vec<Request>> {
        let secret = |key: &str| self.config_secret(key).map(|s| s.expose().to_string());
        Ok(match channel {
            "pagerduty" => vec![pagerduty(alert, &secret("alerts.pagerduty.routing_key").ok_or_else(|| anyhow!("alerts.pagerduty.routing_key isn't set"))?)],
            "sms" => {
                let sid = self.alert_text("alerts.twilio.account_sid");
                if !(sid.starts_with("AC") && sid.len() == 34 && sid.chars().all(|c| c.is_ascii_alphanumeric())) {
                    bail!("alerts.twilio.account_sid must be a Twilio account SID (AC and 32 characters)");
                }
                let token = secret("alerts.twilio.auth_token").ok_or_else(|| anyhow!("alerts.twilio.auth_token isn't set"))?;
                let from = self.alert_text("alerts.twilio.from");
                if from.is_empty() {
                    bail!("alerts.twilio.from isn't set (a Twilio number, or a messaging service SID)");
                }
                let to = phone_numbers(&self.alert_text("alerts.twilio.to"))?;
                if to.is_empty() {
                    bail!("alerts.twilio.to has no numbers");
                }
                twilio(alert, &sid, &token, &from, &to)
            }
            "chat" => webhook_urls(&secret("alerts.chat.webhook_urls").unwrap_or_default())?.iter().map(|url| chat(alert, url)).collect(),
            "webhook" => {
                let url = self.alert_text("alerts.webhook.url");
                vec![webhook(alert, &url, secret("alerts.webhook.signing_secret").as_deref(), Utc::now().timestamp())]
            }
            "opsgenie" => {
                let key = secret("alerts.opsgenie.api_key").ok_or_else(|| anyhow!("alerts.opsgenie.api_key isn't set"))?;
                vec![opsgenie(alert, &key, self.config_value("alerts.opsgenie.region").as_str().unwrap_or("us"))]
            }
            other => bail!("unknown alert channel '{}' (one of {})", other, CHANNELS.join(", ")),
        })
    }

    /// Send an alert to every channel set up (or just `only`), and say how each went.
    pub async fn send_alert(&self, alert: &Alert, only: Option<&str>) -> Vec<Delivery> {
        let channels: Vec<&str> = match only {
            Some(channel) => vec![channel],
            None => self.alert_channels(),
        };
        let client = crate::crypt::http_client().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).build();
        let mut out = Vec::new();
        for channel in channels {
            let result = match &client {
                Ok(client) => match self.alert_requests(channel, alert) {
                    Ok(requests) => send_all(client, &requests).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(anyhow!("couldn't build the HTTP client: {}", e)),
            };
            out.push(Delivery { channel: channel.to_string(), ok: result.is_ok(), error: result.err().map(|e| format!("{:#}", e)) });
        }
        out
    }
}

/// Send each request; the first failure is the channel's.
async fn send_all(client: &reqwest::Client, requests: &[Request]) -> Result<()> {
    for (i, r) in requests.iter().enumerate() {
        let mut request = client.post(&r.url);
        for (k, v) in &r.headers {
            request = request.header(k, v);
        }
        request = match &r.body {
            Body::Json(body) => request.json(body),
            Body::Form(pairs) => {
                let mut encoded = reqwest::Url::parse("http://form/").expect("a valid URL");
                encoded.query_pairs_mut().extend_pairs(pairs);
                request.header("content-type", "application/x-www-form-urlencoded").body(encoded.query().unwrap_or_default().to_string())
            }
        };
        // Errors name the host only: URLs and headers can hold credentials.
        let host = reqwest::Url::parse(&r.url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| "the server".into());
        let which = if requests.len() > 1 { format!(" (request {} of {})", i + 1, requests.len()) } else { String::new() };
        let response = request.send().await.map_err(|e| anyhow!("{} didn't answer{}: {}", host, which, if e.is_timeout() { "timed out".to_string() } else { e.without_url().to_string() }))?;
        let status = response.status();
        if !status.is_success() {
            let text: String = response.text().await.unwrap_or_default().chars().take(300).collect();
            bail!("{} answered {}{}{}", host, status, which, if text.trim().is_empty() { String::new() } else { format!(": {}", text.trim()) });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(link: bool) -> Alert {
        Alert {
            key: "hexdb-security-review-01J".into(),
            severity: "high".into(),
            title: "HexDB security review of 'prod': high severity, 1 finding".into(),
            text: "Window. Password guessing.\n1. [high] Guessing: 40 failures from 203.0.113.9".into(),
            link: link.then(|| "https://db.example.com/ui/security-reviews".into()),
            lattice: "prod".into(),
            hex: "hex-1".into(),
            review: json!({ "id": "01J", "summary": "Password guessing." }),
        }
    }

    #[test]
    fn pagerduty_triggers_a_deduplicated_incident() {
        let r = pagerduty(&alert(true), "R0UTING");
        assert_eq!(r.url, PAGERDUTY_URL);
        let Body::Json(b) = r.body else { panic!() };
        assert_eq!((b["routing_key"].as_str(), b["event_action"].as_str(), b["dedup_key"].as_str()), (Some("R0UTING"), Some("trigger"), Some("hexdb-security-review-01J")));
        assert_eq!(b["payload"]["severity"], "critical");
        assert_eq!(b["payload"]["source"], "prod (hex-1)");
        assert_eq!(b["links"][0]["href"], "https://db.example.com/ui/security-reviews");
        let Body::Json(b) = pagerduty(&Alert { severity: "medium".into(), ..alert(false) }, "k").body else { panic!() };
        assert_eq!(b["payload"]["severity"], "warning");
        assert!(b.get("links").is_none());
    }

    #[test]
    fn twilio_sends_one_short_text_per_number() {
        let to = vec!["+15551230001".to_string(), "+447700900123".to_string()];
        let mut a = alert(true);
        a.review["summary"] = json!("x".repeat(1000));
        let rs = twilio(&a, "AC0123", "tok", "+15550000000", &to);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://api.twilio.com/2010-04-01/Accounts/AC0123/Messages.json");
        assert_eq!(rs[0].headers[0].1, "Basic QUMwMTIzOnRvaw==");
        let Body::Form(form) = &rs[1].body else { panic!() };
        assert_eq!(form[0], ("To".to_string(), "+447700900123".to_string()));
        let text = &form[2].1;
        assert!(text.chars().count() <= SMS_CHARS);
        assert!(text.ends_with("https://db.example.com/ui/security-reviews"), "the link survives the cut: {}", text);
    }

    #[test]
    fn chat_webhooks_get_their_service_format() {
        assert_eq!(chat_kind("https://hooks.slack.com/services/T/B/x"), Chat::Slack);
        assert_eq!(chat_kind("https://discord.com/api/webhooks/1/abc"), Chat::Discord);
        assert_eq!(chat_kind("https://prod-01.westus.logic.azure.com:443/workflows/x"), Chat::Teams);
        assert_eq!(chat_kind("https://contoso.webhook.office.com/webhookb2/x"), Chat::Teams);
        assert_eq!(chat_kind("https://chat.example.com/hooks/x"), Chat::Slack);
        assert_eq!(chat_kind("https://notdiscord.com/x"), Chat::Slack);

        let Body::Json(b) = chat(&alert(true), "https://hooks.slack.com/services/x").body else { panic!() };
        assert!(b["text"].as_str().unwrap().contains("40 failures") && b["text"].as_str().unwrap().ends_with("/ui/security-reviews"));
        let mut long = alert(false);
        long.text = "y".repeat(5000);
        let Body::Json(b) = chat(&long, "https://discord.com/api/webhooks/1/x").body else { panic!() };
        assert_eq!(b["content"].as_str().unwrap().chars().count(), DISCORD_CHARS);
        assert_eq!(b["allowed_mentions"]["parse"], json!([]), "no @everyone from an AI's text");
        let Body::Json(b) = chat(&alert(true), "https://x.logic.azure.com/workflows/1").body else { panic!() };
        assert_eq!(b["attachments"][0]["contentType"], "application/vnd.microsoft.card.adaptive");
        assert_eq!(b["attachments"][0]["content"]["body"][2]["actions"][0]["url"], "https://db.example.com/ui/security-reviews");
    }

    #[test]
    fn webhooks_are_signed_over_timestamp_and_body() {
        let r = webhook(&alert(false), "https://hooks.example.com/hexdb", Some("s3cret"), 1_800_000_000);
        let Body::Json(b) = &r.body else { panic!() };
        assert_eq!(b["event"], "security.review");
        assert_eq!(b["review"]["id"], "01J");
        let signature = r.headers.iter().find(|(k, _)| k == "x-hexdb-signature").map(|(_, v)| v.clone()).unwrap();
        assert_eq!(signature, format!("sha256={}", sign("s3cret", 1_800_000_000, &b.to_string())));
        assert_eq!(sign("key", 1, "{}"), {
            let mut mac = Hmac::<Sha256>::new_from_slice(b"key").unwrap();
            mac.update(b"1.{}");
            hex::encode(mac.finalize().into_bytes())
        });
        let unsigned = webhook(&alert(false), "https://h", None, 1);
        assert!(!unsigned.headers.iter().any(|(k, _)| k == "x-hexdb-signature"));
    }

    #[test]
    fn opsgenie_by_region_with_priority() {
        let r = opsgenie(&alert(true), "gk", "eu");
        assert_eq!(r.url, "https://api.eu.opsgenie.com/v2/alerts");
        assert_eq!(r.headers[0].1, "GenieKey gk");
        let Body::Json(b) = r.body else { panic!() };
        assert_eq!((b["priority"].as_str(), b["alias"].as_str()), (Some("P1"), Some("hexdb-security-review-01J")));
        assert!(b["message"].as_str().unwrap().chars().count() <= 130);
        assert_eq!(opsgenie(&alert(false), "gk", "us").url, "https://api.opsgenie.com/v2/alerts");
    }

    #[test]
    fn lists_are_checked() {
        assert_eq!(phone_numbers(" +1 (555) 123-0001, +44 7700 900123\n").unwrap(), vec!["+15551230001", "+447700900123"]);
        assert!(phone_numbers("5551230001").is_err());
        assert!(phone_numbers("+12").is_err());
        assert_eq!(webhook_urls("https://a.example/x\nhttps://b.example/y, ").unwrap().len(), 2);
        let e = webhook_urls("https://ok.example/x ftp://secret-token@host").unwrap_err().to_string();
        assert!(e.contains("URL 2") && !e.contains("secret-token"), "{}", e);
        assert!(webhook_urls("http://169.254.169.254/latest").is_err());
    }
}
