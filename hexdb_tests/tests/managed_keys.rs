//! Customer-managed keys: the storage key kept only wrapped by AWS KMS or Azure
//! Key Vault. A mock of each (and of Entra ID's token endpoint) wraps, unwraps,
//! and can be told to refuse (a revoked key) or to fail (an outage).

use anyhow::{bail, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use hexdb_tests::{cli_bin, TestOptions, TestServer};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const OK: u8 = 0;
const REFUSE: u8 = 1;
const DOWN: u8 = 2;
const ARN: &str = "arn:aws:kms:us-east-1:111122223333:key/hexdb-test";

/// The mock's "wrapping": a marker and the key's bytes reversed.
fn wrap(key: &[u8]) -> Vec<u8> {
    let mut out = b"MOCKWRAP".to_vec();
    out.extend(key.iter().rev());
    out
}

fn unwrap(blob: &[u8]) -> Option<Vec<u8>> {
    blob.strip_prefix(b"MOCKWRAP").map(|k| k.iter().rev().copied().collect())
}

struct Mock {
    port: u16,
    mode: Arc<AtomicU8>,
    unwraps: Arc<AtomicUsize>,
}

impl Mock {
    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn azure_key(&self) -> String {
        format!("{}/keys/hexdb/1", self.url())
    }
}

/// Answers AWS KMS (POST / with X-Amz-Target), Entra ID tokens and Key Vault key operations.
fn mock() -> Result<Mock> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let mode = Arc::new(AtomicU8::new(OK));
    let unwraps = Arc::new(AtomicUsize::new(0));
    let (m, u) = (mode.clone(), unwraps.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (mut length, mut target, mut auth) = (0, String::new(), String::new());
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap_or(0);
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => length = v.trim().parse().unwrap_or(0),
                        "x-amz-target" => target = v.trim().to_string(),
                        "authorization" => auth = v.trim().to_string(),
                        _ => {}
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok();
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let mode = m.load(Ordering::SeqCst);
            let (status, reply) = if mode == DOWN {
                (503, json!({ "message": "service unavailable" }))
            } else if path.contains("/oauth2/v2.0/token") {
                let form = String::from_utf8_lossy(&body);
                if form.contains("client_secret=test-secret") && form.contains("grant_type=client_credentials") {
                    (200, json!({ "access_token": "mock-token", "token_type": "Bearer", "expires_in": 3600 }))
                } else {
                    (401, json!({ "error": "invalid_client", "error_description": "bad secret" }))
                }
            } else if path.starts_with("/keys/") {
                let operation = path.split('?').next().unwrap_or_default().rsplit('/').next().unwrap_or_default().to_string();
                if auth != "Bearer mock-token" {
                    (401, json!({ "error": { "code": "Unauthorized", "message": "no token" } }))
                } else if mode == REFUSE {
                    (403, json!({ "error": { "code": "Forbidden", "message": "Operation unwrapKey is not allowed on a disabled key." } }))
                } else {
                    let value = URL_SAFE_NO_PAD.decode(json["value"].as_str().unwrap_or_default()).unwrap_or_default();
                    match operation.as_str() {
                        "wrapkey" => (200, json!({ "kid": format!("http://127.0.0.1:{}/keys/hexdb/1", port), "value": URL_SAFE_NO_PAD.encode(wrap(&value)) })),
                        "unwrapkey" => {
                            u.fetch_add(1, Ordering::SeqCst);
                            match unwrap(&value) {
                                Some(k) => (200, json!({ "kid": "k", "value": URL_SAFE_NO_PAD.encode(k) })),
                                None => (400, json!({ "error": { "code": "BadParameter", "message": "not this key's" } })),
                            }
                        }
                        _ => (404, json!({ "error": { "code": "NotFound", "message": operation } })),
                    }
                }
            } else if !auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/") || !auth.contains("/us-east-1/kms/aws4_request") {
                (400, json!({ "__type": "UnrecognizedClientException", "message": format!("bad signature: {}", auth) }))
            } else if mode == REFUSE {
                (400, json!({ "__type": "DisabledException", "message": format!("{} is disabled.", ARN) }))
            } else {
                match target.as_str() {
                    "TrentService.Encrypt" => {
                        let plain = STANDARD.decode(json["Plaintext"].as_str().unwrap_or_default()).unwrap_or_default();
                        (200, json!({ "KeyId": ARN, "CiphertextBlob": STANDARD.encode(wrap(&plain)) }))
                    }
                    "TrentService.Decrypt" => {
                        u.fetch_add(1, Ordering::SeqCst);
                        let blob = STANDARD.decode(json["CiphertextBlob"].as_str().unwrap_or_default()).unwrap_or_default();
                        match unwrap(&blob) {
                            Some(k) => (200, json!({ "KeyId": ARN, "Plaintext": STANDARD.encode(k) })),
                            None => (400, json!({ "__type": "InvalidCiphertextException", "message": "" })),
                        }
                    }
                    _ => (400, json!({ "__type": "UnknownOperationException", "message": target })),
                }
            };
            let text = reply.to_string();
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, text.len(), text);
        }
    });
    Ok(Mock { port, mode, unwraps })
}

fn aws_env(mock: &Mock) -> Vec<(String, String)> {
    vec![
        ("HEXDB_AWS_KMS_ENDPOINT".into(), mock.url()),
        ("AWS_ACCESS_KEY_ID".into(), "AKIDTEST".into()),
        ("AWS_SECRET_ACCESS_KEY".into(), "test-aws-secret".into()),
    ]
}

fn azure_env(mock: &Mock) -> Vec<(String, String)> {
    vec![
        ("AZURE_AUTHORITY_HOST".into(), mock.url()),
        ("AZURE_TENANT_ID".into(), "tenant".into()),
        ("AZURE_CLIENT_ID".into(), "client".into()),
        ("AZURE_CLIENT_SECRET".into(), "test-secret".into()),
    ]
}

/// Runs the CLI with the key service's environment and returns stdout.
fn cli(env: &[(String, String)], dir: Option<&std::path::Path>, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new(cli_bin()?);
    if let Some(dir) = dir {
        cmd.arg("--config").arg(dir.join("hexdb.toml"));
    }
    cmd.args(args).envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("hexdb {:?} failed: {}{}", args, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn options(section: &str, env: Vec<(String, String)>) -> TestOptions {
    let mut env = env;
    env.push(("HEXDB_DEBUG_KEY_CHECK_SECONDS".into(), "2".into()));
    TestOptions { encryption_key: Some(String::new()), extra_toml: format!("\n{}", section), env, ..Default::default() }
}

/// The unwrapped storage key, base64 (the mock's wrapping is reversible), to make
/// sure nothing hands it out.
fn settings_plain_key(server: &TestServer) -> String {
    let toml = std::fs::read_to_string(server.dir().join("hexdb.toml")).unwrap_or_default();
    let wrapped = toml.lines().find_map(|l| l.trim().strip_prefix("wrapped = \"")).map(|v| v.trim_end_matches('"').to_string()).unwrap_or_default();
    let blob = STANDARD.decode(wrapped.trim_start_matches("base64:")).unwrap_or_default();
    STANDARD.encode(unwrap(&blob).unwrap_or_default())
}

fn wait_exit(server: &mut TestServer, within: Duration) -> Result<()> {
    let deadline = Instant::now() + within;
    while !server.has_exited() {
        if Instant::now() > deadline {
            bail!("the server kept running. Log tail:\n{}", server.log_tail(30));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[test]
fn aws_kms_unwraps_at_start_and_a_revoked_key_stops_the_hex() -> Result<()> {
    let mock = mock()?;
    let env = aws_env(&mock);
    let section = cli(&env, None, &["keys", "new", "--provider", "aws_kms", "--key-id", ARN])?;
    assert!(section.contains("[storage.key]") && section.contains("provider = \"aws_kms\"") && section.contains(ARN), "{}", section);

    let mut server = TestServer::start_with(options(&section, env.clone()))?;
    server.insert("orders", &json!({ "n": 1 }))?;
    server.flush()?;
    assert!(server.log_tail(400).contains("AWS KMS unwrapped the storage key"), "{}", server.log_tail(40));
    // The key never reaches the configuration listing or the join snippet.
    let settings = server.request(reqwest::Method::GET, "/settings", None, &[])?;
    assert_eq!(settings.body["config"]["storage"]["encryption_key"], "(set)", "{}", settings.body);
    let join = server.request(reqwest::Method::POST, "/join", Some(&json!({ "password": hexdb_tests::TEST_ADMIN_PASSWORD })), &[])?;
    assert!(join.status.is_success(), "{}", join.body);
    let text = join.body.to_string();
    let plain = settings_plain_key(&server);
    assert!(plain.len() > 40, "no wrapped key in hexdb.toml");
    assert!(text.contains("[storage.key]") && !text.contains(&plain), "{}", text);
    let check = cli(&env, Some(server.dir()), &["keys", "check"])?;
    assert!(check.contains("AWS KMS unwrapped the storage key"), "{}", check);

    // A key service outage is only a warning.
    mock.set(DOWN);
    let before = mock.unwraps.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(5));
    assert!(server.is_running() && !server.has_exited(), "an outage stopped the hex");
    assert!(server.log_tail(200).contains("can't be reached to check the storage key"), "{}", server.log_tail(40));
    assert_eq!(mock.unwraps.load(Ordering::SeqCst), before);
    mock.set(OK);

    // Revoked: the hex stops within a check, and won't start until the key is usable.
    mock.set(REFUSE);
    wait_exit(&mut server, Duration::from_secs(30))?;
    let log = std::fs::read_to_string(server.log_path())?;
    assert!(log.contains("refused to unwrap the storage key"), "{}", server.log_tail(40));
    let err = server.launch().expect_err("started with a revoked key");
    assert!(format!("{:#}", err).contains("didn't unwrap the storage key"), "{:#}", err);

    // Usable again: it starts, data intact.
    mock.set(OK);
    server.launch()?;
    assert_eq!(server.count("orders")?, 1);
    let audit = server.request(reqwest::Method::GET, "/audit?action=storage.key_revoked", None, &[])?;
    assert!(audit.body.to_string().contains("storage.key_revoked"), "{}", audit.body);
    Ok(())
}

#[test]
fn azure_key_vault_wraps_rewraps_and_refuses() -> Result<()> {
    let mock = mock()?;
    let env = azure_env(&mock);
    let section = cli(&env, None, &["keys", "new", "--provider", "azure_key_vault", "--key-id", &mock.azure_key()])?;
    assert!(section.contains("provider = \"azure_key_vault\"") && section.contains("/keys/hexdb/1"), "{}", section);

    let mut server = TestServer::start_with(options(&section, env.clone()))?;
    server.insert("orders", &json!({ "n": 1 }))?;
    server.restart()?;
    assert_eq!(server.count("orders")?, 1);

    // Rewrap under "another" key: the same data key, so the hex still reads its data.
    let rewrapped = cli(&env, Some(server.dir()), &["keys", "rewrap", "--key-id", &mock.azure_key()])?;
    assert!(rewrapped.contains("[storage.key]"), "{}", rewrapped);

    mock.set(REFUSE);
    wait_exit(&mut server, Duration::from_secs(30))?;
    let err = server.launch().expect_err("started with a disabled key");
    assert!(format!("{:#}", err).contains("Key Vault"), "{:#}", err);
    Ok(())
}

#[test]
fn a_plain_key_and_a_managed_key_together_are_refused() -> Result<()> {
    let mock = mock()?;
    let env = aws_env(&mock);
    let section = cli(&env, None, &["keys", "new", "--provider", "aws_kms", "--key-id", ARN])?;
    let mut opts = options(&section, env);
    opts.encryption_key = None;
    let err = TestServer::start_with(opts).map(|_| ()).expect_err("started with both keys");
    assert!(format!("{:#}", err).contains("both set"), "{:#}", err);
    Ok(())
}
