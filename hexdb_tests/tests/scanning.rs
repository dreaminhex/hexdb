//! Malware scanning of uploads: verdicts recorded on files, infected uploads
//! quarantined (never stored in their tessellation, never on another hex), release
//! and deletion by administrators, and uploads held while the scanner can't answer.
//!
//! ClamAV is played by a fake clamd that flags a marker string; YARA-X runs for
//! real with HexDB's built-in rules (the EICAR test file).

use anyhow::{bail, Result};
use hexdb_core::scan::eicar_test_file;
use hexdb_tests::{TestOptions, TestServer};
use reqwest::{Method, StatusCode};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Content the fake clamd reports as infected.
const MARKER: &[u8] = b"HEXDB-TEST-MALWARE";

/// A fake clamd on a free port (PING, VERSION, INSTREAM) that flags MARKER; returns its address.
fn fake_clamd() -> String {
    fake_clamd_flagging(Arc::new(Mutex::new(vec![MARKER.to_vec()])))
}

/// A fake clamd that flags any of `markers` (which a test can change, like a signature update).
fn fake_clamd_flagging(markers: Arc<Mutex<Vec<Vec<u8>>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let markers = markers.clone();
            std::thread::spawn(move || {
                let mut socket = socket;
                let mut command = Vec::new();
                let mut byte = [0u8; 1];
                while socket.read_exact(&mut byte).is_ok() && byte[0] != 0 {
                    command.push(byte[0]);
                }
                let answer: &[u8] = match command.as_slice() {
                    b"zPING" => b"PONG\0",
                    b"zVERSION" => b"ClamAV 1.4.1/27450/Mon Nov  4 08:00:00 2026\0",
                    b"zINSTREAM" => {
                        let mut data = Vec::new();
                        loop {
                            let mut len = [0u8; 4];
                            if socket.read_exact(&mut len).is_err() {
                                return;
                            }
                            let len = u32::from_be_bytes(len) as usize;
                            if len == 0 {
                                break;
                            }
                            let mut piece = vec![0u8; len];
                            if socket.read_exact(&mut piece).is_err() {
                                return;
                            }
                            data.extend_from_slice(&piece);
                        }
                        if markers.lock().unwrap().iter().any(|m| data.windows(m.len()).any(|w| w == m.as_slice())) {
                            b"stream: Hexdb.Test.Marker FOUND\0"
                        } else {
                            b"stream: OK\0"
                        }
                    }
                    _ => b"UNKNOWN COMMAND\0",
                };
                let _ = socket.write_all(answer);
            });
        }
    });
    address
}

fn scanning(clamd: &str) -> String {
    format!("\n[files.scan]\nclamd = \"{}\"\n", clamd)
}

fn files_server(options: TestOptions) -> Result<TestServer> {
    let server = TestServer::start_with(options)?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    Ok(server)
}

fn put(server: &TestServer, path: &str, bytes: &[u8]) -> Result<hexdb_tests::ApiResponse> {
    server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), bytes.to_vec(), &[])
}

fn infected_with_marker() -> Vec<u8> {
    let mut bytes = b"an innocent-looking report ".to_vec();
    bytes.extend_from_slice(MARKER);
    bytes
}

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn uploads_are_scanned_and_infected_ones_quarantined() -> Result<()> {
    let clamd = fake_clamd();
    let server = files_server(TestOptions { extra_toml: scanning(&format!("tcp://{}", clamd)), ..Default::default() })?;

    // A clean file records its verdict and the engines that gave it.
    let clean = put(&server, "docs/clean.txt", b"hello, world")?;
    assert_eq!(clean.status, StatusCode::CREATED, "{}", clean.body);
    assert_eq!(clean.body["scan"]["status"], "clean", "{}", clean.body);
    let engines = clean.body["scan"]["engines"].as_array().cloned().unwrap_or_default();
    assert!(engines.iter().any(|e| e.as_str().unwrap_or("").starts_with("ClamAV 1.4.1/27450")), "{:?}", engines);
    assert!(engines.iter().any(|e| e.as_str().unwrap_or("").starts_with("YARA-X")), "{:?}", engines);

    // ClamAV's verdict and YARA-X's (the EICAR test file) both quarantine the upload.
    let bad = put(&server, "docs/report.bin", &infected_with_marker())?;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.body);
    assert_eq!(bad.error_code(), Some("file_quarantined"));
    assert!(bad.body["error"]["message"].as_str().unwrap().contains("Hexdb.Test.Marker"), "{}", bad.body);
    let eicar = put(&server, "docs/eicar.com", &eicar_test_file())?;
    assert_eq!(eicar.error_code(), Some("file_quarantined"), "{}", eicar.body);
    assert!(eicar.body["error"]["message"].as_str().unwrap().contains("EICAR_Test_File"), "{}", eicar.body);

    // Neither is a file.
    for path in ["docs/report.bin", "docs/eicar.com"] {
        let (status, _, _) = server.get_bytes(&format!("/media/_files/{}", path), &[])?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{}", path);
    }

    // Administrators see them, with what matched; others can't.
    let quarantine = server.request(Method::GET, "/quarantine", None, &[])?;
    let files = quarantine.body["files"].as_array().cloned().unwrap_or_default();
    assert_eq!(files.len(), 2, "{}", quarantine.body);
    let report = files.iter().find(|f| f["name"] == "report.bin").expect("report.bin quarantined").clone();
    assert_eq!(report["tessellation"], "media");
    assert_eq!(report["scan"]["detections"][0], json!({ "engine": "ClamAV", "name": "Hexdb.Test.Marker" }));
    let eicar = files.iter().find(|f| f["name"] == "eicar.com").expect("eicar.com quarantined").clone();
    let viewer = server.user_with_roles("viewer", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    assert_eq!(server.request_as(Some(&viewer), Method::GET, "/quarantine", None, &[])?.status, StatusCode::FORBIDDEN);

    let status = server.request(Method::GET, "/status", None, &[])?.body;
    assert_eq!(status["storage"]["scan"]["quarantined"], 2, "{}", status["storage"]["scan"]);
    assert!(status["storage"]["scan"]["warning"].is_null(), "ClamAV is configured: {}", status["storage"]["scan"]);

    // A false positive is released into its tessellation, as a normal file.
    let released = server.request(Method::POST, &format!("/quarantine/{}/release", report["id"].as_str().unwrap()), None, &[])?;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    assert_eq!(released.body["document"]["scan"]["released_by"], "admin", "{}", released.body);
    let (status, _, bytes) = server.get_bytes("/media/_files/docs/report.bin", &[])?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, infected_with_marker());

    // The other is deleted for good.
    let deleted = server.request(Method::DELETE, &format!("/quarantine/{}", eicar["id"].as_str().unwrap()), None, &[])?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
    assert_eq!(server.request(Method::GET, "/quarantine", None, &[])?.body["files"], json!([]));

    // All of it is audited.
    let audit = server.request(Method::GET, "/audit?action=file.&limit=100", None, &[])?.body;
    let actions: Vec<&str> = audit["events"].as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    for action in ["file.quarantined", "file.released", "file.quarantine_deleted"] {
        assert!(actions.contains(&action), "{} in {:?}", action, actions);
    }
    Ok(())
}

#[test]
fn a_scanner_that_cant_answer_holds_uploads() -> Result<()> {
    // A port nothing listens on.
    let unused = TcpListener::bind("127.0.0.1:0")?.local_addr()?.to_string();
    let server = files_server(TestOptions { extra_toml: scanning(&format!("tcp://{}", unused)), ..Default::default() })?;
    let res = put(&server, "docs/a.txt", b"hello")?;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert_eq!(res.error_code(), Some("scanner_unavailable"));
    assert!(res.headers.get("retry-after").is_some());
    let (status, _, _) = server.get_bytes("/media/_files/docs/a.txt", &[])?;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing was stored");
    Ok(())
}

#[test]
fn without_clamav_the_built_in_rules_scan_alone_and_say_so() -> Result<()> {
    let server = files_server(TestOptions { extra_toml: scanning("off"), ..Default::default() })?;
    let clean = put(&server, "docs/a.txt", b"hello")?;
    assert_eq!(clean.status, StatusCode::CREATED, "{}", clean.body);
    assert_eq!(clean.body["scan"]["engines"].as_array().map(Vec::len), Some(1), "{}", clean.body);
    assert_eq!(put(&server, "docs/eicar.com", &eicar_test_file())?.error_code(), Some("file_quarantined"));
    let warning = server.request(Method::GET, "/status", None, &[])?.body["storage"]["scan"]["warning"].clone();
    assert!(warning.as_str().unwrap_or("").contains("ClamAV isn't configured"), "{}", warning);
    Ok(())
}

#[test]
fn quarantine_never_reaches_a_replica() -> Result<()> {
    let clamd = fake_clamd();
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let hex = |i: usize, ram_mb: u64| TestOptions {
        lattice: Some(format!("scanning-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        extra_toml: format!("{}\n[replication]\nslicing = false\n", scanning(&format!("tcp://{}", clamd))),
        ..Default::default()
    };
    let overseer = files_server(hex(0, 4096))?;
    let replica = TestServer::start_with(hex(1, 1024))?;
    let first = put(&overseer, "docs/first.txt", b"first")?;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    wait_until(Duration::from_secs(30), "the replica's first sync", || Ok(replica.get_bytes("/media/_files/docs/first.txt", &[])?.0 == StatusCode::OK))?;

    let bad = put(&overseer, "docs/bad.bin", &infected_with_marker())?;
    assert_eq!(bad.error_code(), Some("file_quarantined"));
    // A later clean upload reaching the replica shows replication went past the quarantine.
    put(&overseer, "docs/later.txt", b"later")?;
    wait_until(Duration::from_secs(30), "the later file on the replica", || Ok(replica.get_bytes("/media/_files/docs/later.txt", &[])?.0 == StatusCode::OK))?;
    let replica_scan = replica.request(Method::GET, "/status", None, &[])?.body["storage"]["scan"].clone();
    assert_eq!(replica_scan["quarantined"], 0, "the quarantine stays on the Overseer: {}", replica_scan);
    assert_eq!(replica.get_bytes("/media/_files/docs/bad.bin", &[])?.0, StatusCode::NOT_FOUND);
    // Asked on the replica, the quarantine is the Overseer's.
    let listed = replica.request(Method::GET, "/quarantine", None, &[])?;
    assert_eq!(listed.body["files"].as_array().map(Vec::len), Some(1), "{}", listed.body);

    // Released, it's a file everywhere.
    let id = listed.body["files"][0]["id"].as_str().unwrap().to_string();
    let released = overseer.request(Method::POST, &format!("/quarantine/{}/release", id), None, &[])?;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    wait_until(Duration::from_secs(30), "the released file on the replica", || Ok(replica.get_bytes("/media/_files/docs/bad.bin", &[])?.0 == StatusCode::OK))?;
    Ok(())
}

/// Against a real ClamAV, when HEXDB_TEST_CLAMD names one (tcp://host:3310), e.g.
/// docker run -p 3310:3310 -v ./deploy/clamav/clamd.conf:/etc/clamav/clamd.conf clamav/clamav:1.5
#[test]
fn a_real_clamav_finds_the_eicar_test_file() -> Result<()> {
    let Ok(clamd) = std::env::var("HEXDB_TEST_CLAMD") else {
        eprintln!("HEXDB_TEST_CLAMD isn't set; skipping the test against a real ClamAV");
        return Ok(());
    };
    let server = files_server(TestOptions { extra_toml: scanning(&clamd), ..Default::default() })?;
    let clean = put(&server, "docs/a.txt", b"hello")?;
    assert_eq!(clean.body["scan"]["status"], "clean", "{}", clean.body);
    let eicar = put(&server, "docs/eicar.com", &eicar_test_file())?;
    assert_eq!(eicar.error_code(), Some("file_quarantined"), "{}", eicar.body);
    let files = server.request(Method::GET, "/quarantine", None, &[])?.body["files"].clone();
    let detections = files[0]["scan"]["detections"].as_array().cloned().unwrap_or_default();
    assert!(detections.iter().any(|d| d["engine"] == "ClamAV" && d["name"].as_str().unwrap_or("").to_lowercase().contains("eicar")), "{:?}", detections);
    // A larger file streams in many pieces.
    let big: Vec<u8> = (0..20_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    assert_eq!(put(&server, "docs/big.bin", &big)?.body["scan"]["status"], "clean");
    Ok(())
}

#[test]
fn a_rescan_quarantines_files_new_signatures_find() -> Result<()> {
    let markers = Arc::new(Mutex::new(vec![MARKER.to_vec()]));
    let clamd = fake_clamd_flagging(markers.clone());
    let server = files_server(TestOptions { extra_toml: scanning(&format!("tcp://{}", clamd)), ..Default::default() })?;
    // Clean under today's signatures.
    let later = b"a payload only tomorrow's signatures know".to_vec();
    assert_eq!(put(&server, "docs/later.bin", &later)?.status, StatusCode::CREATED);
    assert_eq!(put(&server, "docs/fine.txt", b"fine")?.status, StatusCode::CREATED);

    // The signatures update; an administrator rescans (otherwise it runs on its own).
    markers.lock().unwrap().push(b"tomorrow's signatures".to_vec());
    let viewer = server.user_with_roles("viewer", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    assert_eq!(server.request_as(Some(&viewer), Method::POST, "/quarantine/rescan", None, &[])?.status, StatusCode::FORBIDDEN);
    assert_eq!(server.request(Method::POST, "/quarantine/rescan", None, &[])?.status, StatusCode::ACCEPTED);
    wait_until(Duration::from_secs(30), "the rescan to finish", || {
        let rescan = server.request(Method::GET, "/status", None, &[])?.body["storage"]["scan"]["rescan"].clone();
        Ok(rescan["running"] == false && rescan["finished_at"].is_string())
    })?;

    // The newly detected file left its tessellation for quarantine; the other stayed.
    assert_eq!(server.get_bytes("/media/_files/docs/later.bin", &[])?.0, StatusCode::NOT_FOUND);
    assert_eq!(server.get_bytes("/media/_files/docs/fine.txt", &[])?.0, StatusCode::OK);
    let files = server.request(Method::GET, "/quarantine", None, &[])?.body["files"].clone();
    assert_eq!(files.as_array().map(Vec::len), Some(1), "{}", files);
    assert_eq!(files[0]["name"], "later.bin");
    assert_eq!(files[0]["rescan"], true);
    let rescan = server.request(Method::GET, "/status", None, &[])?.body["storage"]["scan"]["rescan"].clone();
    assert_eq!(rescan["scanned"], 2, "{}", rescan);
    assert_eq!(rescan["found"], 1, "{}", rescan);
    assert!(rescan["fingerprint"].as_str().unwrap_or("").contains("ClamAV"), "{}", rescan);
    Ok(())
}
