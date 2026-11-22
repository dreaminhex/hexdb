//! Scenarios for `compare` beyond the single-hex workload: a three-hex lattice with
//! synchronous acknowledgements, with file downloads (straight from the Overseer and
//! through a sliced replica) during document writes; and queue receives with and
//! without a dashboard polling the queue's counts. Every scenario counts errors
//! instead of timing them: a comparison with any error is reported as invalid.

use crate::compare::{concurrent, memory, Samples};
use crate::{options, Http, Rng, LIMITS};
use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Files uploaded to the lattice, and their size.
const FILES: usize = 8;
const FILE_BYTES: usize = 8 * 1024 * 1024;
/// Writers and the inserts each makes, in each lattice phase.
const WRITERS: usize = 8;
const WRITES_EACH: usize = 250;
/// Messages per queue phase, and consumers taking them.
const MESSAGES: usize = 10_000;
const CONSUMERS: usize = 4;

/// Wait for `check`, or fail after `timeout`.
fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !check() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The version the hex reports, if no scenario noted it yet.
fn note_version(http: &Http, out: &mut Samples) {
    if out.version.is_empty() {
        out.version = http.json(reqwest::Method::GET, "/health", None).ok().and_then(|h| h["version"].as_str().map(String::from)).unwrap_or_else(|| "unknown".into());
    }
}

/// A document write that counts as done only with its 201; anything else is an error.
fn write_one(http: &Http, doc: &[u8], samples: &Mutex<Vec<Duration>>, errors: &AtomicUsize) {
    let started = Instant::now();
    match http.send(reqwest::Method::POST, "/concurrent", Some(doc.to_vec())) {
        Ok((201, _)) => samples.lock().unwrap().push(started.elapsed()),
        _ => {
            errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Download a file in full: the time to its first byte, and whether every byte matches.
fn download(client: &reqwest::blocking::Client, url: &str, token: &str, expected: &[u8]) -> Result<(Duration, bool)> {
    let started = Instant::now();
    let mut response = client.get(url).bearer_auth(token).send()?;
    if response.status().as_u16() != 200 {
        return Ok((started.elapsed(), false));
    }
    let mut first = [0u8; 1];
    let n = response.read(&mut first)?;
    let first_byte = started.elapsed();
    let mut body = first[..n].to_vec();
    response.read_to_end(&mut body)?;
    Ok((first_byte, body == expected))
}

/// Three hexes from `bin` on this machine, writes acknowledged by two replicas
/// (`replication.min_acks = 2`). Writes alone, then the same writes while files
/// download from the Overseer and through a sliced replica.
pub fn lattice(bin: &Path, orders: &[Vec<u8>], out: &mut Samples) -> Result<()> {
    let ports = (0..3).map(|_| TestServer::free_port()).collect::<Result<Vec<_>>>()?;
    let opts = |i: usize, ram: u64| TestOptions {
        lattice: Some(format!("compare-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: ports.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| format!("127.0.0.1:{}", p)).collect(),
        ram_mb: Some(ram),
        discovery_interval_seconds: Some(1),
        extra_toml: LIMITS.into(),
        server_bin: Some(bin.to_path_buf()),
        ..TestOptions::default()
    };
    let overseer = TestServer::start_with(opts(0, 4096))?;
    let first = TestServer::start_with(opts(1, 1024))?;
    let second = TestServer::start_with(opts(2, 1024))?;
    let ready = |s: &TestServer| s.request(reqwest::Method::GET, "/ready", None, &[]).map(|r| r.status.as_u16() == 200).unwrap_or(false);
    let role = |s: &TestServer| s.request(reqwest::Method::GET, "/status", None, &[]).map(|r| r.body["hex_type"] == "Overseer").unwrap_or(false);
    wait_until(Duration::from_secs(180), "the lattice to form", || role(&overseer) && ready(&first) && ready(&second))?;

    let leader = Http::new(&overseer)?;
    note_version(&leader, out);
    // Synchronous from here on (set at runtime: an Overseer starting alone with it would wait
    // on its own first writes for replicas that aren't there yet).
    leader.json(reqwest::Method::PUT, "/settings", Some(&json!({ "replication.min_acks": 2, "replication.ack_timeout_ms": 10000 })))?;
    // The files, in memory to check every downloaded byte against.
    leader.json(reqwest::Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })))?;
    let mut rng = Rng(7);
    let files: Vec<Vec<u8>> = (0..FILES).map(|_| (0..FILE_BYTES).map(|_| rng.next() as u8).collect()).collect();
    for (i, bytes) in files.iter().enumerate() {
        let (status, body) = leader.send_raw(reqwest::Method::PUT, &format!("/media/_files/bench/file-{}.bin", i), bytes.clone())?;
        if !(200..300).contains(&status) {
            bail!("upload returned {}: {}", status, String::from_utf8_lossy(&body));
        }
    }

    // Writes alone.
    let errors = AtomicUsize::new(0);
    let latencies = Mutex::new(Vec::new());
    let started = Instant::now();
    concurrent(WRITERS, |t, _| {
        for doc in orders.iter().skip(t * WRITES_EACH).take(WRITES_EACH) {
            write_one(&leader, doc, &latencies, &errors);
        }
        Ok(())
    })?;
    let took = started.elapsed();
    let done = latencies.lock().unwrap().len();
    out.rate("Lattice (min_acks = 2): writes/s, 8 clients", done, took);
    out.latency.entry("Lattice (min_acks = 2): insert, 8 clients").or_default().extend(latencies.into_inner().unwrap());
    *out.errors.entry("Lattice: writes").or_default() += errors.load(Ordering::Relaxed);

    // The same writes while four clients download every file: two from the Overseer, two
    // through a sliced replica (which passes the download on from the Overseer).
    let replica_pid = first.pid().unwrap_or(0);
    let downloading = AtomicBool::new(true);
    let replica_peak = Mutex::new(0f64);
    let errors = AtomicUsize::new(0);
    let download_errors = AtomicUsize::new(0);
    let latencies = Mutex::new(Vec::new());
    let first_bytes: Mutex<[Vec<Duration>; 2]> = Mutex::new([Vec::new(), Vec::new()]);
    let bytes_done = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
    let started = Instant::now();
    let mut download_took = [Duration::ZERO; 2];
    std::thread::scope(|scope| -> Result<()> {
        let watcher = scope.spawn(|| {
            while downloading.load(Ordering::Relaxed) {
                let (rss, _) = memory(replica_pid);
                let mut peak = replica_peak.lock().unwrap();
                *peak = peak.max(rss);
                drop(peak);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let writers = scope.spawn(|| {
            concurrent(WRITERS, |t, _| {
                for doc in orders.iter().skip((WRITERS + t) * WRITES_EACH).take(WRITES_EACH) {
                    write_one(&leader, doc, &latencies, &errors);
                }
                Ok(())
            })
        });
        let mut downloaders = Vec::new();
        for (class, server) in [(0usize, &overseer), (0, &overseer), (1, &first), (1, &first)] {
            let (client, files, first_bytes, bytes_done, download_errors) = (&client, &files, &first_bytes, &bytes_done, &download_errors);
            let (base, token) = (server.url(""), server.token().to_string());
            downloaders.push((
                class,
                scope.spawn(move || -> Result<Duration> {
                    let started = Instant::now();
                    for (i, expected) in files.iter().enumerate() {
                        let (first_byte, ok) = download(client, &format!("{}/media/_files/bench/file-{}.bin", base, i), &token, expected)?;
                        if ok {
                            first_bytes.lock().unwrap()[class].push(first_byte);
                            bytes_done[class].fetch_add(expected.len(), Ordering::Relaxed);
                        } else {
                            download_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(started.elapsed())
                }),
            ));
        }
        for (class, handle) in downloaders {
            let took = handle.join().map_err(|_| anyhow::anyhow!("a download thread panicked"))??;
            download_took[class] = download_took[class].max(took);
        }
        downloading.store(false, Ordering::Relaxed);
        writers.join().map_err(|_| anyhow::anyhow!("a writer panicked"))??;
        watcher.join().map_err(|_| anyhow::anyhow!("the memory watcher panicked"))?;
        Ok(())
    })?;
    let took = started.elapsed();
    let done = latencies.lock().unwrap().len();
    out.rate("Lattice: writes/s while files download", done, took);
    out.latency.entry("Lattice: insert while files download").or_default().extend(latencies.into_inner().unwrap());
    let [direct, relayed] = first_bytes.into_inner().unwrap();
    out.latency.entry("File download, first byte: from the Overseer").or_default().extend(direct);
    out.latency.entry("File download, first byte: through a sliced replica").or_default().extend(relayed);
    // MB/s, as a rate of megabytes.
    for (class, label) in [(0, "File downloads, MB/s: from the Overseer (2 clients)"), (1, "File downloads, MB/s: through a sliced replica (2 clients)")] {
        out.rate(label, bytes_done[class].load(Ordering::Relaxed) / (1024 * 1024), download_took[class]);
    }
    out.peaks.entry("Replica's peak resident set while relaying downloads").or_default().push(*replica_peak.lock().unwrap());
    *out.errors.entry("Lattice: writes during downloads").or_default() += errors.load(Ordering::Relaxed);
    *out.errors.entry("Lattice: downloads (status or bytes wrong)").or_default() += download_errors.load(Ordering::Relaxed);
    Ok(())
}

/// One hex from `bin`: consumers drain a queue, timing each receive, first alone and then
/// while a dashboard polls the queue's counts 20 times a second.
pub fn queues(bin: &Path, out: &mut Samples) -> Result<()> {
    let mut opts = options(LIMITS);
    opts.server_bin = Some(bin.to_path_buf());
    let server = TestServer::start_with(opts)?;
    let http = Http::new(&server)?;
    note_version(&http, out);
    http.json(reqwest::Method::POST, "/queues", Some(&json!({ "name": "jobs", "visibility_seconds": 60, "max_receives": 5, "retention_hours": 1 })))?;
    for (label, polled) in [("Queue receive (up to 10), no one watching", false), ("Queue receive (up to 10), counts polled 20/s", true)] {
        for batch in 0..MESSAGES / 100 {
            let messages: Vec<Value> = (0..100).map(|i| json!({ "body": { "n": batch * 100 + i } })).collect();
            http.json(reqwest::Method::POST, "/queues/jobs/messages", Some(&json!({ "messages": messages })))?;
        }
        let errors = AtomicUsize::new(0);
        let draining = AtomicBool::new(true);
        let taken = AtomicUsize::new(0);
        let started = Instant::now();
        let latencies = std::thread::scope(|scope| -> Result<Vec<Duration>> {
            let poller = scope.spawn(|| {
                while polled && draining.load(Ordering::Relaxed) {
                    if !matches!(http.send(reqwest::Method::GET, "/queues/jobs", None), Ok((200, _))) {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let latencies = concurrent(CONSUMERS, |_, lat| {
                while taken.load(Ordering::Relaxed) < MESSAGES {
                    let started = Instant::now();
                    let Ok((200, body)) = http.send(reqwest::Method::POST, "/queues/jobs/receive", Some(br#"{"max":10}"#.to_vec())) else {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    lat.push(started.elapsed());
                    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let receipts: Vec<Value> = body["messages"].as_array().map(|m| m.iter().map(|m| m["receipt"].clone()).collect()).unwrap_or_default();
                    if receipts.is_empty() {
                        continue;
                    }
                    if !matches!(http.send(reqwest::Method::POST, "/queues/jobs/ack", Some(serde_json::to_vec(&json!({ "receipts": receipts }))?)), Ok((200, _))) {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    taken.fetch_add(receipts.len(), Ordering::Relaxed);
                }
                Ok(())
            });
            draining.store(false, Ordering::Relaxed);
            poller.join().map_err(|_| anyhow::anyhow!("the poller panicked"))?;
            latencies
        })?;
        out.rate(if polled { "Queue messages/s, counts polled 20/s" } else { "Queue messages/s, no one watching" }, taken.load(Ordering::Relaxed), started.elapsed());
        out.latency.entry(label).or_default().extend(latencies);
        *out.errors.entry("Queues").or_default() += errors.load(Ordering::Relaxed);
    }
    Ok(())
}
