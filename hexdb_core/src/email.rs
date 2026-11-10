// HexDB Core Email: outbound mail over SMTP
//
// `email.smtp_url` (a secret in the lattice configuration) names the server
// and its credentials, `email.from` the sender. TLS as the URL says:
// `smtps://user:pass@host:465` (TLS from the start) or
// `smtp://user:pass@host:587?tls=required` (STARTTLS); plain `smtp://` only
// for a server on the same machine. Used by the security reviewer
// (`crate::review`) and the test email.

use crate::HexDBEngine;
use anyhow::{anyhow, bail, Context, Result};
use lettre::{message::header::ContentType, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

/// How long sending one message may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// True once `email.smtp_url` and `email.from` are set.
pub fn configured(engine: &HexDBEngine) -> bool {
    engine.config_secret("email.smtp_url").is_some() && !from(engine).is_empty()
}

fn from(engine: &HexDBEngine) -> String {
    engine.config_value("email.from").as_str().map(str::trim).unwrap_or_default().to_string()
}

/// Send a plain-text message to each recipient (one message, every address in To).
pub async fn send(engine: &HexDBEngine, to: &[String], subject: &str, text: &str) -> Result<()> {
    let url = engine.config_secret("email.smtp_url").ok_or_else(|| anyhow!("Outbound email isn't set up: set email.smtp_url and email.from."))?;
    let sender = from(engine);
    if sender.is_empty() {
        bail!("Outbound email isn't set up: set email.from (e.g. \"HexDB <hexdb@example.com>\").");
    }
    if to.is_empty() {
        bail!("There's no one to send it to.");
    }
    let mut message = Message::builder().from(sender.parse().with_context(|| format!("email.from '{}' isn't an address", sender))?).subject(subject).header(ContentType::TEXT_PLAIN);
    for address in to {
        message = message.to(address.parse().with_context(|| format!("'{}' isn't an email address", address))?);
    }
    let message = message.body(text.to_string()).context("building the message")?;
    // The URL holds the password: errors name the server only.
    let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url.expose())
        .map_err(|e| anyhow!("email.smtp_url isn't a valid SMTP URL ({})", e.to_string().replace(url.expose(), "[url]")))?
        .timeout(Some(SEND_TIMEOUT))
        .build();
    tokio::time::timeout(SEND_TIMEOUT * 2, transport.send(message))
        .await
        .map_err(|_| anyhow!("The SMTP server didn't answer in time."))?
        .map_err(|e| anyhow!("The SMTP server refused the message: {}", e.to_string().replace(url.expose(), "[url]")))?;
    Ok(())
}
