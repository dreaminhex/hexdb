//! The gRPC API: calls run as the matching REST requests, so they get the same
//! authentication, permissions, row filters and field masks; feeds stream as
//! server-streaming calls.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;
use tonic::{transport::Channel, Code};

pub mod pb {
    tonic::include_proto!("hexdb.v1");
}

use pb::hex_db_client::HexDbClient;

fn signed<T>(message: T, credentials: Option<&str>) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    if let Some(key) = credentials {
        request.metadata_mut().insert("authorization", format!("Bearer {}", key).parse().unwrap());
    }
    request
}

fn json_of(reply: &pb::Reply) -> Value {
    serde_json::from_str(&reply.json).unwrap_or_else(|e| panic!("not JSON ({}): {:?}", e, reply))
}

#[test]
fn grpc_calls_run_as_the_caller() -> Result<()> {
    let server = TestServer::start()?;
    let role = json!({ "name": "eu_notes", "permissions": ["read"], "restrictions": { "notes": { "filter": { "region": "eu" }, "hide": ["secret"] } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    server.insert("notes", &json!({ "region": "us", "text": "us note" }))?;
    let reader = server.user_with_roles("wes", json!([{ "name": "eu_notes", "tessellations": ["notes"] }]))?;
    let admin = server.token().to_string();
    let url = server.url("").trim_end_matches('/').to_string();

    tokio::runtime::Runtime::new()?.block_on(async move {
        let channel = Channel::from_shared(url)?.connect().await?;
        let mut client = HexDbClient::new(channel);
        let cookie = format!("{}={}", hexdb_core::SESSION_COOKIE, admin);
        let admin = Some(admin.as_str());
        let reader = Some(reader.as_str());

        // No credentials, or only a cookie: refused.
        let anonymous = client.query(signed(pb::Body { name: "notes".into(), json: "{}".into() }, None)).await.unwrap_err();
        assert_eq!(anonymous.code(), Code::Unauthenticated, "{:?}", anonymous);
        let mut by_cookie = tonic::Request::new(pb::Body { name: "notes".into(), json: "{}".into() });
        by_cookie.metadata_mut().insert("cookie", cookie.parse()?);
        assert_eq!(client.query(by_cookie).await.unwrap_err().code(), Code::Unauthenticated, "cookies are ignored");

        // Documents: insert, get, patch, query, delete.
        let doc = json!({ "region": "eu", "text": "eu note", "secret": "s3", "n": 9007199254740993u64 }).to_string();
        let inserted = client.insert(signed(pb::Write { tessellation: "notes".into(), json: doc, ..Default::default() }, admin)).await?.into_inner();
        assert_eq!(inserted.status, 201);
        let id = json_of(&inserted)["id"].as_str().unwrap().to_string();
        let got = client.get(signed(pb::DocumentRef { tessellation: "notes".into(), id: id.clone(), ..Default::default() }, admin)).await?.into_inner();
        assert!(got.json.contains("9007199254740993"), "integers keep their precision: {}", got.json);
        let patch = pb::Write { tessellation: "notes".into(), id: id.clone(), json: json!({ "text": "edited" }).to_string(), ..Default::default() };
        client.patch(signed(patch, admin)).await?;

        // The reader sees only EU notes, without the hidden field, and can't write.
        let found = client.query(signed(pb::Body { name: "notes".into(), json: "{}".into() }, reader)).await?.into_inner();
        let documents = json_of(&found)["documents"].as_array().cloned().unwrap();
        assert_eq!(documents.len(), 1, "{:?}", documents);
        assert_eq!(documents[0]["text"], "edited");
        assert!(documents[0].get("secret").is_none(), "{:?}", documents[0]);
        let denied = client.insert(signed(pb::Write { tessellation: "notes".into(), json: "{}".into(), ..Default::default() }, reader)).await.unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied, "{:?}", denied);
        let users = client.call(signed(pb::CallRequest { method: "GET".into(), path: "/users".into(), json: String::new() }, reader)).await.unwrap_err();
        assert_eq!(users.code(), Code::PermissionDenied, "Call has the caller's permissions too: {:?}", users);

        // Errors carry the REST error code.
        client.delete(signed(pb::DocumentRef { tessellation: "notes".into(), id: id.clone(), ..Default::default() }, admin)).await?;
        let missing = client.get(signed(pb::DocumentRef { tessellation: "notes".into(), id, ..Default::default() }, admin)).await.unwrap_err();
        assert_eq!(missing.code(), Code::NotFound);
        assert!(missing.metadata().get("hexdb-error").is_some(), "{:?}", missing.metadata());

        // Any REST call.
        let listed = client.call(signed(pb::CallRequest { method: "GET".into(), path: "/tessellations".into(), json: String::new() }, admin)).await?.into_inner();
        assert!(json_of(&listed)["tessellations"].is_array());

        // The change feed, filtered for the reader.
        let mut changes = client.changes(signed(pb::ChangesRequest { after: None, tessellation: "notes".into() }, reader)).await?.into_inner();
        tokio::time::sleep(Duration::from_millis(300)).await;
        for (region, text) in [("us", "not for wes"), ("eu", "for wes")] {
            let write = pb::Write { tessellation: "notes".into(), json: json!({ "region": region, "text": text, "secret": "x" }).to_string(), ..Default::default() };
            client.insert(signed(write, admin)).await?;
        }
        let event = tokio::time::timeout(Duration::from_secs(10), changes.message()).await??.expect("a change");
        assert_eq!(event.event, "change");
        let change: Value = serde_json::from_str(&event.json)?;
        assert_eq!(change["document"]["text"], "for wes", "{}", event.json);
        assert!(change["document"].get("secret").is_none());
        assert!(event.id.parse::<u64>().is_ok());

        // A stream subscription and publishing.
        let created = pb::CallRequest { method: "POST".into(), path: "/streams".into(), json: json!({ "name": "alerts" }).to_string() };
        client.call(signed(created, admin)).await?;
        let mut messages = client.subscribe(signed(pb::SubscribeRequest { stream: "alerts".into(), ..Default::default() }, admin)).await?.into_inner();
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.publish(signed(pb::Body { name: "alerts".into(), json: json!({ "payload": { "level": "high" } }).to_string() }, admin)).await?;
        let event = tokio::time::timeout(Duration::from_secs(10), messages.message()).await??.expect("a message");
        assert_eq!(event.event, "message");
        assert_eq!(serde_json::from_str::<Value>(&event.json)?["payload"], json!({ "level": "high" }));

        // Bad input is refused before reaching REST.
        let bad = client.call(signed(pb::CallRequest { method: "TRACE".into(), path: "/users".into(), json: String::new() }, admin)).await.unwrap_err();
        assert_eq!(bad.code(), Code::InvalidArgument);
        let bad = client.call(signed(pb::CallRequest { method: "GET".into(), path: "http://elsewhere/".into(), json: String::new() }, admin)).await.unwrap_err();
        assert_eq!(bad.code(), Code::InvalidArgument);
        anyhow::Ok(())
    })?;

    // REST still works on the same port.
    assert!(server.request(Method::GET, "/tessellations", None, &[])?.status.is_success());
    Ok(())
}
