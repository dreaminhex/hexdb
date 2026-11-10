//! Review regression tests. Assertions describe the intended safe behavior.
use crate::{auth::{Action, Credential, RoleRules}, users::RoleGrant, Principal, RoleDefinitions};
use serde_json::{json, Value};
use std::sync::Arc;

fn principal(restrictions: Vec<Value>) -> Principal {
    let mut defs = RoleDefinitions::builtin();
    let mut grants = Vec::new();
    for (i, restriction) in restrictions.into_iter().enumerate() {
        let name = format!("review-{i}");
        defs.0.insert(name.clone(), RoleRules { permissions: vec![Action::Read, Action::Write], restrictions: serde_json::from_value(json!({"orders": restriction})).unwrap() });
        grants.push(RoleGrant { name, tessellations: vec!["orders".into()] });
    }
    Principal::new("review-user".into(), "review-user".into(), "review@example.test".into(), grants, Credential::ApiKey { key_id: "review-key".into() }, &defs)
}

#[test]
fn review_overlapping_parent_and_child_hides_stay_protected() {
    let p = principal(vec![json!({"hide":["profile"]}), json!({"hide":["profile.ssn"]})]);
    let mut document = json!({"profile":{"ssn":"synthetic-secret", "name":"Ada"}});
    p.scope("orders", Action::Read).mask_json(&mut document);
    assert!(document.pointer("/profile/ssn").is_none(), "Both grants hide the SSN, but combined grants returned: {document}");
}

#[test]
fn review_hidden_fields_inside_arrays_are_removed() {
    let p = principal(vec![json!({"hide":["contacts.ssn"]})]);
    let mut document = json!({"contacts":[{"ssn":"synthetic-secret", "name":"Ada"}]});
    p.scope("orders", Action::Read).mask_json(&mut document);
    assert!(document.pointer("/contacts/0/ssn").is_none(), "An array bypassed a nested field restriction: {document}");
}

#[test]
fn review_integer_tenant_ids_are_compared_exactly() {
    let p = principal(vec![json!({"filter":{"tenant":9007199254740992u64}})]);
    let doc = crate::Document { id: ulid::Ulid::generate(), tessellation: "orders".into(), data: crate::document::infer_fields_from_json(&json!({"tenant":9007199254740993u64})), ttl: None };
    assert!(!p.scope("orders", Action::Read).allows(&doc), "Adjacent 64-bit tenant IDs must not become equal through f64 rounding");
}

#[tokio::test]
async fn review_unique_index_distinguishes_large_integers() {
    let (engine, dir) = crate::engine::test_engine().await;
    engine.insert_json("orders", json!({"n":9007199254740992u64}), None).await.unwrap();
    engine.create_index("orders", crate::IndexDef {fields:vec!["n".into()], unique:true, ..Default::default()}).await.unwrap();
    let result = engine.insert_json("orders", json!({"n":9007199254740993u64}), None).await;
    engine.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(result.is_ok(), "Different integers collided in the unique index: {result:?}");
}

#[tokio::test]
async fn review_canceling_queued_commit_does_not_stall_changes() {
    let (engine, dir) = crate::engine::test_engine().await;
    let engine = Arc::new(engine);
    let before = engine.changes.published_seq();
    let release = engine.wal.pause_for_review().await;
    // Poll the write until it has queued its WAL record and installed its in-memory
    // version, then drop the future exactly as timeout(next.run(request)) does.
    let mut write = Box::pin(engine.insert_json("orders", json!({"payload": "x"}), None));
    let first_poll = futures::poll!(write.as_mut());
    assert!(first_poll.is_pending(), "Test requires an asynchronous WAL acknowledgement; got {first_poll:?}");
    assert!(engine.stats().await.next_seq > before + 1, "Test must reach the post-WAL-queue suspension point");
    drop(write);
    release.send(()).unwrap();
    engine.insert_json("orders", json!({"n":2}), None).await.unwrap();
    let end = engine.stats().await.next_seq - 1;
    let published = engine.changes.published_seq();
    engine.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(published, end, "A dropped commit future left an unfillable gap in the change feed");
}

#[tokio::test]
async fn review_replay_applies_current_field_restrictions() {
    let (engine, dir) = crate::engine::test_engine().await;
    let id = engine.insert_json("orders", json!({"secret":"synthetic-secret", "n":0}), None).await.unwrap().id.to_string();
    let key = crate::IdempotencyKey::new("review-replay", b"same request").unwrap();
    let unrestricted = Arc::new(principal(vec![json!({"filter":{}})]));
    crate::access::as_caller(unrestricted, engine.patch_document("orders", &id, json!({"n":1}), None, Some(key.clone()))).await.unwrap();
    let restricted = Arc::new(principal(vec![json!({"hide":["secret"]})]));
    let result = crate::access::as_caller(restricted, engine.patch_document("orders", &id, json!({"n":1}), None, Some(key))).await.unwrap();
    let leaked = result.value.data_json().get("secret").cloned();
    assert!(result.replayed);
    engine.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(leaked.is_none(), "Replayed result exposed a now-hidden field: {leaked:?}");
}

#[tokio::test]
async fn review_busy_peer_lock_does_not_disable_quorum() {
    let (mut engine, dir) = crate::engine::test_engine().await;
    engine.config.replication.quorum = 2;
    assert!(engine.check_quorum().is_err(), "No peers means no quorum");
    let peers = engine.peers.lock().await;
    let accepted = engine.check_quorum().is_ok();
    drop(peers);
    engine.config.replication.quorum = 0;
    engine.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(!accepted, "Contending on the peer lock must not turn a missing quorum into success");
}

#[test]
fn review_unsliced_failover_prefers_current_replica() {
    fn peer(id: &str, ram: u64, seq: u64) -> crate::network::discovery::PeerHex {
        serde_json::from_value(json!({"id":id,"name":id,"role":"Harvester","lattice":"review","ip":"127.0.0.1:7702","ram_mb":ram,"disk_mb":1000,"preference":"auto","last_seq":seq,"applied_seq":seq,"replication_state":"streaming","replication_source":"same-leader-history"})).unwrap()
    }
    let stale = peer("stale-big", 4096, 10);
    let current = peer("current-small", 1024, 20);
    assert_ne!(crate::elect(&stale, &[current]), crate::network::discovery::ROLE_OVERSEER, "Stale replica must not supersede the only survivor holding acknowledged writes");
}

#[tokio::test]
async fn review_replication_pages_preserve_transaction_boundaries() {
    let (source, source_dir) = crate::engine::test_engine().await;
    let (replica, replica_dir) = crate::engine::test_engine().await;
    let before = source.changes.published_seq();
    source.insert_documents("padding", (0..999).map(|n| json!({"n":n})).collect(), None, None).await.unwrap();
    let ops = crate::parse_transaction(&json!({"operations":[
        {"op":"insert","tessellation":"accounts","data":{"balance":90}},
        {"op":"insert","tessellation":"accounts","data":{"balance":110}}
    ]})).unwrap();
    source.transaction(&ops, None).await.unwrap();
    // This is the production poll limit in replication.rs. The replica applies
    // each response immediately, then asks for the next page.
    let page = source.changes_after(before, 1000).await.unwrap();
    let writes = page.iter().filter_map(|c| c.document.clone()).map(crate::engine::ReplicaWrite::Put).collect();
    replica.apply_replicated(writes, None).await.unwrap();
    let visible = replica.count_documents("accounts").await.unwrap();
    source.shutdown().await.unwrap();
    replica.shutdown().await.unwrap();
    std::fs::remove_dir_all(source_dir).unwrap();
    std::fs::remove_dir_all(replica_dir).unwrap();
    assert!(visible == 0 || visible == 2, "A replica exposed {visible} of two atomic transaction writes at a network page boundary");
}

#[tokio::test]
async fn review_index_does_not_lose_large_array_matches() {
    let (engine, dir) = crate::engine::test_engine().await;
    engine.insert_json("orders", json!({"tags": (0..1001).collect::<Vec<_>>()}), None).await.unwrap();
    let query = crate::DocumentQuery {filter:crate::Filter::parse(&json!({"tags":1000})).unwrap(), ..Default::default()};
    assert_eq!(engine.query_documents("orders", &query).await.unwrap().documents.len(), 1);
    engine.create_index("orders", crate::IndexDef {fields:vec!["tags".into()], ..Default::default()}).await.unwrap();
    let found = engine.query_documents("orders", &query).await.unwrap().documents.len();
    engine.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(found, 1, "Adding an index must not make an existing document stop matching its 1001st array value");
}
