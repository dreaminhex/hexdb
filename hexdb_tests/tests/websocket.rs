//! Live feeds over WebSocket: the change feed and stream subscriptions send the
//! same events as their Server-Sent Events forms, filtered for the reader, and
//! a cookie-signed upgrade from another site is refused.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::{client::IntoClientRequest, http::HeaderValue, stream::MaybeTlsStream, Message, WebSocket};

fn connect(server: &TestServer, path: &str, headers: &[(&str, &str)]) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, tungstenite::Error> {
    let url = server.url(path).replacen("http://", "ws://", 1);
    let mut request = url.into_client_request()?;
    for (name, value) in headers {
        request.headers_mut().insert(tungstenite::http::HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
    }
    let (socket, _) = tungstenite::connect(request)?;
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    }
    Ok(socket)
}

/// The next event frame (pings are answered by the library).
fn next_event(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<Value> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Close(_) => anyhow::bail!("the server closed the socket"),
            _ => {}
        }
    }
}

#[test]
fn feeds_arrive_over_websocket_filtered_for_the_reader() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({ "region": "eu", "text": "first" }))?;
    let role = json!({ "name": "eu_notes", "permissions": ["read"], "restrictions": { "notes": { "filter": { "region": "eu" }, "hide": ["secret"] } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let key = server.user_with_roles("wes", json!([{ "name": "eu_notes", "tessellations": ["notes"] }]))?;

    let auth = format!("Bearer {}", key);
    let mut feed = connect(&server, "/changes/ws?tessellation=notes", &[("Authorization", &auth)])?;
    std::thread::sleep(Duration::from_millis(300));
    server.insert("notes", &json!({ "region": "us", "text": "not for wes" }))?;
    server.insert("notes", &json!({ "region": "eu", "text": "for wes", "secret": "hidden" }))?;
    let event = next_event(&mut feed)?;
    assert_eq!(event["event"], "change", "{}", event);
    assert_eq!(event["data"]["document"]["text"], "for wes", "the US note is outside the row filter: {}", event);
    assert!(event["data"]["document"].get("secret").is_none(), "the hidden field is masked: {}", event);
    assert!(event["id"].as_str().is_some());
    feed.close(None).ok();

    // A stream subscription.
    let res = server.request(Method::POST, "/streams", Some(&json!({ "name": "alerts" })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let admin = server.token().to_string();
    let mut sub = connect(&server, "/streams/alerts/ws", &[("Authorization", &format!("Bearer {}", admin))])?;
    std::thread::sleep(Duration::from_millis(300));
    assert!(server.request(Method::POST, "/streams/alerts/messages", Some(&json!({ "payload": { "level": "high" } })), &[])?.status.is_success());
    let event = next_event(&mut sub)?;
    assert_eq!(event["event"], "message");
    assert_eq!(event["data"]["payload"], json!({ "level": "high" }), "{}", event);

    // No credentials: refused. A cookie from another site's page: refused.
    assert!(connect(&server, "/changes/ws", &[]).is_err());
    let cookie = format!("{}={}", hexdb_core::SESSION_COOKIE, server.token());
    assert!(connect(&server, "/changes/ws", &[("Cookie", &cookie), ("Origin", "https://evil.example")]).is_err(), "cross-site WebSocket hijacking");
    let origin = server.url("").trim_end_matches('/').to_string();
    assert!(connect(&server, "/changes/ws", &[("Cookie", &cookie), ("Origin", &origin)]).is_ok(), "the admin UI's own origin");
    Ok(())
}
