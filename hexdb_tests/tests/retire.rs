//! Retiring hexes (`POST /retire/{hex}`): a retiring replica's slices move to
//! the others before it's removable; a retiring Overseer hands its role to a
//! successor that first copies every slice; nothing is lost either way.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check().unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn hex(ports: &[u16], i: usize, ram_mb: u64) -> TestOptions {
    TestOptions {
        lattice: Some(format!("retire-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: ports.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| format!("127.0.0.1:{}", p)).collect(),
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        extra_toml: "\n[replication]\nslicing = true\n".into(),
        ..Default::default()
    }
}

fn status(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body)
}

/// This hex's ID, as the lattice knows it.
fn id_of(server: &TestServer) -> Result<String> {
    let s = status(server)?;
    let hexes = s["network"]["lattice"]["hexes"].as_array().cloned().unwrap_or_default();
    Ok(hexes.iter().find(|h| h["is_self"] == true).and_then(|h| h["id"].as_str()).unwrap_or_default().to_string())
}

/// (slices kept, user documents kept).
fn kept(server: &TestServer) -> Result<(u64, u64)> {
    let r = status(server)?["replication"].clone();
    Ok((r["slices_held_count"].as_u64().unwrap_or(0), r["sliced_documents"].as_u64().unwrap_or(0)))
}

fn retiring(server: &TestServer) -> Result<Vec<Value>> {
    Ok(server.request(Method::GET, "/retire", None, &[])?.body["hexes"].as_array().cloned().unwrap_or_default())
}

fn removable(server: &TestServer, id: &str) -> Result<bool> {
    Ok(retiring(server)?.iter().any(|h| h["id"] == id && h["removable"] == true))
}

fn is_overseer(server: &TestServer) -> bool {
    status(server).is_ok_and(|s| s["hex_type"] == "Overseer")
}

#[test]
fn retiring_hexes_hand_their_data_and_role_over_first() -> Result<()> {
    let ports: Vec<u16> = (0..4).map(|_| TestServer::free_port()).collect::<Result<_>>()?;
    let mut overseer = TestServer::start_with(hex(&ports, 0, 4096))?;
    let mut replicas: Vec<TestServer> = (1..4).map(|i| TestServer::start_with(hex(&ports, i, 1024))).collect::<Result<_>>()?;
    let docs: Vec<Value> = (0..300).map(|n| json!({ "n": n })).collect();
    assert!(overseer.request(Method::POST, "/items/_bulk", Some(&json!(docs)), &[])?.status.is_success());
    wait_until(Duration::from_secs(120), "three replicas to split the slices", || {
        let k: Vec<(u64, u64)> = replicas.iter().map(kept).collect::<Result<_>>()?;
        Ok(k.iter().map(|x| x.0).sum::<u64>() == 64 && k.iter().all(|x| x.0 > 0) && k.iter().map(|x| x.1).sum::<u64>() == 300)
    })?;

    // Unknown hexes and non-administrators are refused.
    assert_eq!(overseer.request(Method::POST, "/retire/nobody", None, &[])?.status, 404);
    let reader = overseer.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["items"] }]))?;
    assert_eq!(overseer.request_as(Some(&reader), Method::GET, "/retire", None, &[])?.status, 403);

    // A replica retires: the other two take its slices, then it lets them go and can be removed.
    let leaving = replicas.remove(0);
    let leaving_id = id_of(&leaving)?;
    let res = overseer.request(Method::POST, &format!("/retire/{}", leaving_id), None, &[])?;
    assert_eq!(res.status, 202, "{}", res.body);
    wait_until(Duration::from_secs(120), "the retiring replica's slices to move", || {
        let k: Vec<(u64, u64)> = replicas.iter().map(kept).collect::<Result<_>>()?;
        Ok(k.iter().map(|x| x.0).sum::<u64>() == 64 && kept(&leaving)? == (0, 0) && removable(&overseer, &leaving_id)?)
    })?;
    let listed = status(&overseer)?["network"]["lattice"]["hexes"].as_array().cloned().unwrap_or_default();
    assert!(listed.iter().any(|h| h["id"] == leaving_id.as_str() && h["retiring"] == true && h["removable"] == true), "{:?}", listed);
    drop(leaving);

    // A cancelled retirement leaves the hex as it was.
    let other_id = id_of(&replicas[1])?;
    assert_eq!(overseer.request(Method::POST, &format!("/retire/{}", other_id), None, &[])?.status, 202);
    assert_eq!(overseer.request(Method::DELETE, &format!("/retire/{}", other_id), None, &[])?.status, 204);
    assert!(retiring(&overseer)?.iter().all(|h| h["id"] != other_id.as_str()));

    // The Overseer retires: a successor copies every slice, then takes the role over.
    let overseer_id = id_of(&overseer)?;
    assert_eq!(overseer.request(Method::POST, &format!("/retire/{}", overseer_id), None, &[])?.status, 202);
    let mut leader: Option<usize> = None;
    wait_until(Duration::from_secs(180), "a successor to take the Overseer role", || {
        leader = replicas.iter().position(is_overseer);
        Ok(leader.is_some() && !is_overseer(&overseer))
    })?;
    let leader = &replicas[leader.unwrap()];
    assert_eq!(kept(leader)?, (64, 300), "the successor took over with every slice");
    wait_until(Duration::from_secs(120), "the old Overseer to be removable", || removable(leader, &overseer_id))?;
    let audit = leader.request(Method::GET, "/audit?action=lattice.handover", None, &[])?.body;
    assert!(audit["events"].as_array().is_some_and(|e| e.iter().any(|e| e["details"]["reason"] == "retiring")), "{}", audit);
    overseer.kill();

    // Two hexes left, nothing lost, and writes work.
    let res = leader.request(Method::GET, "/items/count", None, &[])?;
    assert_eq!(res.body["count"], 300, "{}", res.body);
    assert!(leader.request(Method::POST, "/items", Some(&json!({ "n": 300 })), &[])?.status.is_success());
    let follower = replicas.iter().find(|r| !std::ptr::eq(*r, leader)).unwrap();
    wait_until(Duration::from_secs(90), "the last replica to keep everything", || Ok(kept(follower)? == (64, 301)))?;
    Ok(())
}
