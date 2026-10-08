//! HexDB benchmarks: measures what the README's numbers claim, on the
//! machine it runs on.
//!
//!   cargo run --release -p hexdb_bench            everything (about 5 minutes)
//!   cargo run --release -p hexdb_bench -- quick   smaller data sets, no failover
//!
//! It builds release binaries, runs in-process measurements (encryption,
//! compression, Reed-Solomon encoding and repair) and starts real release
//! servers in temporary directories for the rest (reads, writes, queries,
//! storage, crash recovery, failover). The report is printed and written to
//! bench/results/.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use hexdb_core::hex::{DocKey, Hex, Lookup};
use hexdb_core::KeyRing;
use hexdb_tests::{TestOptions, TestServer};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const MB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// A small deterministic random number generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const CITIES: &[&str] = &["Oslo", "Lima", "Austin", "Lyon", "Osaka", "Pune", "Cork", "Graz", "Perth", "Quito", "Turku", "Leeds"];
const COUNTRIES: &[&str] = &["NO", "PE", "US", "FR", "JP", "IN", "IE", "AT", "AU", "EC", "FI", "GB"];
const STATUSES: &[&str] = &["new", "paid", "shipped", "delivered", "refunded"];
const PRODUCTS: &[&str] = &["Desk lamp", "Notebook", "Water bottle", "Backpack", "Headphones", "Keyboard", "Mug", "Chair mat", "Monitor arm", "Cable kit"];
const WORDS: &[&str] = &["please", "leave", "at", "the", "front", "door", "gift", "wrap", "call", "on", "arrival", "fragile", "items", "inside", "thanks"];
const STREETS: &[&str] = &["Maple", "Harbor", "Station", "Mill", "Church", "Garden", "Bridge", "Park"];

/// An order of roughly 0.7 KB of JSON.
fn order(rng: &mut Rng, i: u64) -> Value {
    let lines: Vec<Value> = (0..1 + rng.below(5))
        .map(|_| {
            let price = (rng.below(20_000) as f64) / 100.0 + 1.0;
            json!({ "sku": format!("SKU-{:05}", rng.below(20_000)), "name": rng.pick(PRODUCTS), "qty": 1 + rng.below(4), "price": price })
        })
        .collect();
    let total: f64 = lines.iter().map(|l| l["price"].as_f64().unwrap_or(0.0) * l["qty"].as_f64().unwrap_or(0.0)).sum();
    let city = rng.below(CITIES.len() as u64) as usize;
    let note: Vec<&str> = (0..rng.below(8)).map(|_| rng.pick(WORDS)).collect();
    json!({
        "number": i,
        "customer": format!("customer-{}", rng.below(5_000)),
        "email": format!("buyer{}@example.com", rng.below(5_000)),
        "status": rng.pick(STATUSES),
        "total": (total * 100.0).round() / 100.0,
        "currency": "USD",
        "placed_at": format!("2026-{:02}-{:02}T{:02}:{:02}:{:02}Z", 1 + rng.below(9), 1 + rng.below(28), rng.below(24), rng.below(60), rng.below(60)),
        "shipping": {
            "street": format!("{} {} St", 1 + rng.below(999), rng.pick(STREETS)),
            "city": CITIES[city],
            "country": COUNTRIES[city],
            "postal": format!("{:05}", rng.below(100_000)),
        },
        "items": lines,
        "note": note.join(" "),
        "gift": rng.below(10) == 0,
    })
}

/// A structured JSON document of about `bytes` bytes: a report with many rows.
fn big_structured(bytes: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut rows = Vec::new();
    let mut size = 64;
    while size < bytes {
        let row = json!({
            "line": rows.len(),
            "sku": format!("SKU-{:05}", rng.below(20_000)),
            "name": rng.pick(PRODUCTS),
            "warehouse": rng.pick(CITIES),
            "qty": rng.below(500),
            "unit_price": (rng.below(50_000) as f64) / 100.0,
            "updated_at": format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", 1 + rng.below(9), 1 + rng.below(28), rng.below(24), rng.below(60)),
        });
        size += row.to_string().len() + 1;
        rows.push(row);
    }
    serde_json::to_vec(&json!({ "report": "inventory snapshot", "rows": rows })).unwrap_or_default()
}

/// A document of about `bytes` bytes of random data, base64-encoded (it barely compresses).
fn big_random(bytes: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let raw: Vec<u8> = (0..bytes * 3 / 4).map(|_| rng.next() as u8).collect();
    serde_json::to_vec(&json!({ "blob": base64::engine::general_purpose::STANDARD.encode(raw) })).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let i = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[i]
}

fn ms(d: Duration) -> String {
    let v = d.as_secs_f64() * 1000.0;
    if v < 10.0 {
        format!("{:.2} ms", v)
    } else if v < 100.0 {
        format!("{:.1} ms", v)
    } else {
        format!("{:.0} ms", v)
    }
}

fn mb(bytes: f64) -> String {
    if bytes >= 1024.0 * MB {
        format!("{:.2} GB", bytes / (1024.0 * MB))
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{:.1} KB", bytes / 1024.0)
    }
}

fn rate(count: f64, d: Duration) -> String {
    let per_second = count / d.as_secs_f64().max(1e-9);
    if per_second >= 10_000.0 {
        format!("{:.0}", per_second)
    } else {
        format!("{:.1}", per_second)
    }
}

fn throughput(bytes: f64, d: Duration) -> String {
    format!("{:.0} MB/s", bytes / MB / d.as_secs_f64().max(1e-9))
}

/// The report, a table at a time.
struct Report {
    text: String,
}

impl Report {
    fn section(&mut self, title: &str, note: &str) {
        let _ = write!(self.text, "\n## {}\n\n", title);
        if !note.is_empty() {
            let _ = write!(self.text, "{}\n\n", note);
        }
        let _ = writeln!(self.text, "| Measurement | Result |\n| --- | --- |");
        println!("\n== {}", title);
    }
    fn row(&mut self, what: &str, value: String) {
        println!("  {:<60} {}", what, value);
        let _ = writeln!(self.text, "| {} | {} |", what, value);
    }
}

/// An HTTP client for timing requests (the harness client parses bodies, which would add to the times).
struct Http {
    client: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl Http {
    fn new(server: &TestServer) -> Result<Http> {
        Ok(Http {
            client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(300)).build()?,
            base: server.url(""),
            token: server.token().to_string(),
        })
    }
    fn send(&self, method: reqwest::Method, path: &str, body: Option<Vec<u8>>) -> Result<(u16, Vec<u8>)> {
        let mut request = self.client.request(method, format!("{}{}", self.base, path)).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.header("Content-Type", "application/json").body(body);
        }
        let response = request.send()?;
        let status = response.status().as_u16();
        Ok((status, response.bytes()?.to_vec()))
    }
    fn json(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let (status, bytes) = self.send(method, path, body.map(|b| serde_json::to_vec(b).unwrap_or_default()))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            bail!("{} returned {}: {}", path, status, value);
        }
        Ok(value)
    }
    fn status(&self) -> Result<Value> {
        self.json(reqwest::Method::GET, "/status", None)
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_default()
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size(&p)
            } else {
                file_size(&p)
            }
        })
        .sum()
}

fn machine() -> String {
    let cpu = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "(Get-CimInstance Win32_Processor | Select-Object -First 1).Name"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    }
    .unwrap_or_else(|| "unknown CPU".into());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    format!("{} ({} threads), {} {}", cpu, threads, std::env::consts::OS, std::env::consts::ARCH)
}

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

fn build_and_sizes(report: &mut Report) -> Result<PathBuf> {
    let root = workspace();
    // Its own target directory, so a server already running from target/release isn't in the way.
    let target = root.join("target").join("bench");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "-p", "hexdb_api", "-p", "hexdb_cli", "-p", "hexdb_odbc", "--target-dir"])
        .arg(&target)
        .current_dir(&root)
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build --release failed");
    }
    let release = target.join("release");
    let exe = std::env::consts::EXE_SUFFIX;
    let server = release.join(format!("hexdb_api{}", exe));
    let cli = release.join(format!("hexdb{}", exe));
    let odbc = release.join(if cfg!(windows) {
        "hexdb_odbc.dll".to_string()
    } else if cfg!(target_os = "macos") {
        "libhexdb_odbc.dylib".to_string()
    } else {
        "libhexdb_odbc.so".to_string()
    });
    let ui = dir_size(&root.join("hexdb_admin").join("dist"));

    report.section("Binary sizes", "Release builds (`cargo build --release`), not stripped further.");
    report.row("Server (`hexdb_api`): storage engine, REST, GraphQL, SQL, replication", mb(file_size(&server) as f64));
    report.row("CLI (`hexdb`)", mb(file_size(&cli) as f64));
    report.row("ODBC driver", mb(file_size(&odbc) as f64));
    if ui > 0 {
        report.row("Admin UI (built files)", mb(ui as f64));
    }
    report.row("Total", mb((file_size(&server) + file_size(&cli) + file_size(&odbc) + ui) as f64));
    Ok(server)
}

fn crypto_and_compression(report: &mut Report, orders: &[Vec<u8>]) -> Result<()> {
    report.section(
        "Encryption and compression (one core)",
        "Documents are stored on disk as AES-256-GCM(zstd(JSON)), one document at a time. These run in-process on one thread.",
    );
    let keys = KeyRing::new(&[7u8; 32], &[]);
    let block: Vec<u8> = (0..1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
    let rounds = 256;
    let started = Instant::now();
    let mut sealed = Vec::new();
    for _ in 0..rounds {
        sealed = keys.encrypt(&block, b"bench")?;
    }
    let encrypt = started.elapsed();
    let started = Instant::now();
    for _ in 0..rounds {
        keys.decrypt(keys.current_id(), &sealed, b"bench")?;
    }
    let decrypt = started.elapsed();
    report.row("AES-256-GCM encryption (1 MB blocks)", throughput(rounds as f64 * MB, encrypt));
    report.row("AES-256-GCM decryption (1 MB blocks)", throughput(rounds as f64 * MB, decrypt));

    // Per-document zstd, as the storage engine does it (level 0 = zstd's default, 3).
    let raw: usize = orders.iter().map(Vec::len).sum();
    let started = Instant::now();
    let compressed: Vec<Vec<u8>> = orders.iter().map(|d| hexdb_core::compress::compress(d, 0).unwrap_or_default()).collect();
    let compress = started.elapsed();
    let packed: usize = compressed.iter().map(Vec::len).sum();
    let started = Instant::now();
    for c in &compressed {
        hexdb_core::compress::decompress(c)?;
    }
    let decompress = started.elapsed();
    report.row(
        &format!("Order documents (avg {:.0} bytes of JSON): compressed size", raw as f64 / orders.len() as f64),
        format!("{:.0}% of the JSON ({:.2}x smaller)", 100.0 * packed as f64 / raw as f64, raw as f64 / packed as f64),
    );
    report.row("Order documents: zstd compression", throughput(raw as f64, compress));
    report.row("Order documents: zstd decompression", throughput(raw as f64, decompress));
    for (label, doc) in [("10 MB structured JSON document", big_structured(10 << 20, 1)), ("10 MB random (base64) document", big_random(10 << 20, 2))] {
        let started = Instant::now();
        let c = hexdb_core::compress::compress(&doc, 0)?;
        let took = started.elapsed();
        let started = Instant::now();
        hexdb_core::compress::decompress(&c)?;
        let back = started.elapsed();
        report.row(
            &format!("{}: compressed size", label),
            format!("{:.0}% of the JSON ({:.1}x smaller); compress {}, decompress {}", 100.0 * c.len() as f64 / doc.len() as f64, doc.len() as f64 / c.len() as f64, throughput(doc.len() as f64, took), throughput(doc.len() as f64, back)),
        );
    }
    Ok(())
}

fn vertices(report: &mut Report, orders: &[Vec<u8>]) -> Result<()> {
    report.section(
        "Vertices: Reed-Solomon shards in memory (one core)",
        "Each document in memory is split into 4 data and 2 parity shards across six vertices, each shard with a BLAKE3 hash. Any 2 of the 6 can be lost. A lost or corrupt vertex is rebuilt from the other shards; these measure that in-process.",
    );
    let mut hex = Hex::new();
    let keys: Vec<DocKey> = (0..orders.len()).map(|i| DocKey::new("orders", ulid::Ulid::from(i as u128 + 1))).collect();
    let raw: usize = orders.iter().map(Vec::len).sum();
    let started = Instant::now();
    for (k, d) in keys.iter().zip(orders) {
        hex.put(k, 1, None, d, false);
    }
    let encode = started.elapsed();
    report.row(&format!("Encode {} order documents ({}) into shards", orders.len(), mb(raw as f64)), format!("{} ({})", ms(encode), throughput(raw as f64, encode)));
    report.row("Memory used per byte of document (shards + parity)", format!("{:.2}x", hex.memory_bytes() as f64 / raw as f64));
    let started = Instant::now();
    for k in &keys {
        hex.read(k);
    }
    report.row("Read every document back (hashes checked)", throughput(raw as f64, started.elapsed()));

    // A whole vertex lost: every shard on vertex 2 is bad.
    for k in &keys {
        hex.corrupt_for_testing(k, 2);
    }
    let started = Instant::now();
    for k in &keys {
        if !matches!(hex.read(k), Some(Lookup::Live { .. })) {
            bail!("read failed with one lost vertex");
        }
    }
    report.row("Read every document with one vertex lost (rebuilt on the fly)", throughput(raw as f64, started.elapsed()));
    let started = Instant::now();
    let repair = hex.check_integrity(&keys);
    let took = started.elapsed();
    report.row(
        &format!("Rebuild a lost vertex: repair all {} documents' shards", orders.len()),
        format!("{} ({} shards repaired)", ms(took), repair.repaired_shards),
    );
    for k in &keys {
        hex.corrupt_for_testing(k, 0);
        hex.corrupt_for_testing(k, 5);
    }
    let started = Instant::now();
    let repair = hex.check_integrity(&keys);
    report.row("Rebuild two lost vertices at once", format!("{} ({} shards repaired)", ms(started.elapsed()), repair.repaired_shards));

    let big = big_structured(10 << 20, 3);
    let k = DocKey::new("big", ulid::Ulid::from(1u128));
    let started = Instant::now();
    hex.put(&k, 1, None, &big, false);
    report.row("Encode one 10 MB document", ms(started.elapsed()));
    let started = Instant::now();
    hex.read(&k);
    report.row("Read one 10 MB document (intact)", ms(started.elapsed()));
    hex.corrupt_for_testing(&k, 1);
    hex.corrupt_for_testing(&k, 4);
    let started = Instant::now();
    hex.read(&k);
    report.row("Read one 10 MB document with two lost vertices", ms(started.elapsed()));
    Ok(())
}

fn options(dir_toml: &str) -> TestOptions {
    TestOptions { ram_mb: Some(4096), extra_toml: dir_toml.into(), ..TestOptions::default() }
}

const LIMITS: &str = "\n[limits]\nmax_document_kb = 12288\nmax_request_mb = 64\n";

fn server_reads_and_writes(report: &mut Report, orders: &[Vec<u8>]) -> Result<()> {
    let server = TestServer::start_with(options(LIMITS))?;
    let http = Http::new(&server)?;
    let n = orders.len();

    report.section(
        "Writes, reads and queries (one server, localhost HTTP)",
        "A release server with default settings: every write is in the write-ahead log and fsynced before it's acknowledged. Times are end to end from an HTTP client on the same machine.",
    );

    // Bulk load.
    let started = Instant::now();
    for chunk in orders.chunks(1000) {
        let mut body = b"[".to_vec();
        for (i, d) in chunk.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(d);
        }
        body.push(b']');
        let (status, text) = http.send(reqwest::Method::POST, "/orders/_bulk", Some(body))?;
        if status != 201 {
            bail!("bulk insert returned {}: {}", status, String::from_utf8_lossy(&text));
        }
    }
    let took = started.elapsed();
    let raw: usize = orders.iter().map(Vec::len).sum();
    report.row(&format!("Bulk insert {} orders (1,000 per request)", n), format!("{} docs/s ({}, {})", rate(n as f64, took), throughput(raw as f64, took), ms(took)));

    // Single inserts, one client and eight.
    let mut samples = Vec::new();
    for d in orders.iter().take(1000) {
        let started = Instant::now();
        let (status, _) = http.send(reqwest::Method::POST, "/singles", Some(d.clone()))?;
        samples.push(started.elapsed());
        if status != 201 {
            bail!("insert returned {}", status);
        }
    }
    let total: Duration = samples.iter().sum();
    report.row(
        "Single insert, one client: median / p99 latency",
        format!("{} / {} ({} writes/s)", ms(percentile(&mut samples, 0.5)), ms(percentile(&mut samples, 0.99)), rate(1000.0, total)),
    );
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let http = &http;
                scope.spawn(move || -> Result<()> {
                    for d in orders.iter().skip(t * 250).take(250) {
                        http.send(reqwest::Method::POST, "/concurrent", Some(d.clone()))?;
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("thread panicked"))??;
        }
        Ok(())
    })?;
    report.row("Single inserts, 8 concurrent clients (group commit)", format!("{} writes/s", rate(2000.0, started.elapsed())));

    // Reads by ID.
    let ids: Vec<String> = http.json(reqwest::Method::POST, "/orders/_query", Some(&json!({ "limit": 1000, "fields": ["id"], "total": false })))?["documents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|d| d["id"].as_str().map(String::from))
        .collect();
    let mut samples = Vec::new();
    for _ in 0..3 {
        for id in &ids {
            let started = Instant::now();
            http.send(reqwest::Method::GET, &format!("/orders/{}", id), None)?;
            samples.push(started.elapsed());
        }
    }
    let total: Duration = samples.iter().sum();
    report.row(
        "Read one document by ID, one client: median / p99",
        format!("{} / {} ({} reads/s)", ms(percentile(&mut samples, 0.5)), ms(percentile(&mut samples, 0.99)), rate(samples.len() as f64, total)),
    );
    let started = Instant::now();
    std::thread::scope(|scope| {
        for t in 0..8 {
            let (http, ids) = (&http, &ids);
            scope.spawn(move || {
                for id in ids.iter().skip(t * 125).take(125).cycle().take(500) {
                    let _ = http.send(reqwest::Method::GET, &format!("/orders/{}", id), None);
                }
            });
        }
    });
    report.row("Reads by ID, 8 concurrent clients", format!("{} reads/s", rate(4000.0, started.elapsed())));

    // Queries.
    http.json(reqwest::Method::POST, "/tessellations/orders/indexes", Some(&json!({ "fields": ["customer"] })))?;
    let mut samples = Vec::new();
    for i in 0..500 {
        let body = json!({ "filter": { "customer": format!("customer-{}", i * 7 % 5000) }, "limit": 50 });
        let started = Instant::now();
        http.json(reqwest::Method::POST, "/orders/_query", Some(&body))?;
        samples.push(started.elapsed());
    }
    report.row(&format!("Indexed query (customer = ?) over {} orders: median / p99", n), format!("{} / {}", ms(percentile(&mut samples, 0.5)), ms(percentile(&mut samples, 0.99))));
    let mut samples = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        http.json(reqwest::Method::POST, "/sql", Some(&json!({ "sql": "SELECT COUNT(*) FROM orders" })))?;
        samples.push(started.elapsed());
    }
    report.row("SQL `SELECT COUNT(*)`: median", ms(percentile(&mut samples, 0.5)));
    let mut samples = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        http.json(reqwest::Method::POST, "/sql", Some(&json!({ "sql": "SELECT status, COUNT(*), SUM(total), AVG(total) FROM orders GROUP BY status" })))?;
        samples.push(started.elapsed());
    }
    report.row(&format!("SQL `GROUP BY status` with SUM and AVG over {} orders: median", n), ms(percentile(&mut samples, 0.5)));

    // Storage for the orders: memory and disk after a flush and compaction.
    http.json(reqwest::Method::POST, "/flush", Some(&json!({})))?;
    http.json(reqwest::Method::POST, "/compact", Some(&json!({})))?;
    let status = http.status()?;
    let tess = status["metrics"]["tessellations"].as_array().cloned().unwrap_or_default();
    let orders_metrics = tess.iter().find(|t| t["name"] == "orders").cloned().unwrap_or(Value::Null);
    let disk = status["storage"]["disk_bytes"].as_f64().unwrap_or(0.0);
    let all_raw: f64 = tess.iter().filter(|t| t["kind"] == "user").map(|t| t["total_size_bytes"].as_f64().unwrap_or(0.0)).sum();
    report.section(
        "Storage: memory and disk",
        "After writing the orders above, flushing and compacting. Disk is everything in the data directory's SSTables (encrypted, compressed, with indexes); memory is the in-memory shards.",
    );
    report.row("Orders stored", format!("{} documents, {} of JSON", orders_metrics["document_count"], mb(orders_metrics["total_size_bytes"].as_f64().unwrap_or(0.0))));
    report.row("Disk used by all data (SSTables, encrypted and compressed)", format!("{} for {} of JSON ({:.0}%)", mb(disk), mb(all_raw), 100.0 * disk / all_raw.max(1.0)));
    report.row("Memory used by the in-memory shards", format!("{} ({:.2}x the JSON)", mb(status["storage"]["memory_bytes"].as_f64().unwrap_or(0.0)), status["storage"]["memory_bytes"].as_f64().unwrap_or(0.0) / all_raw.max(1.0)));

    // 10 MB documents.
    report.section(
        "10 MB documents",
        "Documents this large need `limits.max_document_kb` raised (the default is 1 MB). Five of each kind, each written and read over HTTP; medians.",
    );
    for (label, tess, make) in [
        ("Structured JSON (a report with ~60,000 rows)", "big_json", big_structured as fn(usize, u64) -> Vec<u8>),
        ("Random data, base64-encoded (incompressible)", "big_random", big_random as fn(usize, u64) -> Vec<u8>),
    ] {
        let before = http.status()?;
        let docs: Vec<Vec<u8>> = (0..5).map(|i| make(10 << 20, 100 + i)).collect();
        let size = docs[0].len() as f64;
        let mut writes = Vec::new();
        let mut ids = Vec::new();
        for d in &docs {
            let started = Instant::now();
            let (status, body) = http.send(reqwest::Method::POST, &format!("/{}", tess), Some(d.clone()))?;
            writes.push(started.elapsed());
            if status != 201 {
                bail!("10 MB insert returned {}: {}", status, String::from_utf8_lossy(&body[..body.len().min(300)]));
            }
            let v: Value = serde_json::from_slice(&body)?;
            ids.push(v["id"].as_str().unwrap_or_default().to_string());
        }
        let mut reads = Vec::new();
        for _ in 0..3 {
            for id in &ids {
                let started = Instant::now();
                let (status, body) = http.send(reqwest::Method::GET, &format!("/{}/{}", tess, id), None)?;
                reads.push(started.elapsed());
                if status != 200 || body.len() < docs[0].len() / 2 {
                    bail!("10 MB read returned {}", status);
                }
            }
        }
        let after_write = http.status()?;
        http.json(reqwest::Method::POST, "/flush", Some(&json!({})))?;
        http.json(reqwest::Method::POST, "/compact", Some(&json!({})))?;
        let after_flush = http.status()?;
        let memory = (after_write["storage"]["memory_bytes"].as_f64().unwrap_or(0.0) - before["storage"]["memory_bytes"].as_f64().unwrap_or(0.0)) / 5.0;
        let disk = (after_flush["storage"]["disk_bytes"].as_f64().unwrap_or(0.0) - before["storage"]["disk_bytes"].as_f64().unwrap_or(0.0)) / 5.0;
        let w = percentile(&mut writes, 0.5);
        let r = percentile(&mut reads, 0.5);
        report.row(&format!("{}: write", label), format!("{} ({})", ms(w), throughput(size, w)));
        report.row(&format!("{}: read", label), format!("{} ({})", ms(r), throughput(size, r)));
        report.row(&format!("{}: memory per document", label), format!("{} ({:.2}x)", mb(memory), memory / size));
        report.row(&format!("{}: disk per document", label), format!("{} ({:.0}%)", mb(disk), 100.0 * disk / size));
    }
    Ok(())
}

fn crash_recovery(report: &mut Report, orders: &[Vec<u8>]) -> Result<()> {
    let mut server = TestServer::start_with(options(LIMITS))?;
    let n = orders.len();
    report.section(
        "Crash recovery (one hex)",
        "The server process is killed (no shutdown) and started again on the same data. Times are from starting the process until it answers /health with every document readable.",
    );
    {
        let http = Http::new(&server)?;
        for chunk in orders.chunks(1000) {
            let body = format!("[{}]", chunk.iter().map(|d| String::from_utf8_lossy(d).into_owned()).collect::<Vec<_>>().join(","));
            http.send(reqwest::Method::POST, "/orders/_bulk", Some(body.into_bytes()))?;
        }
        let unflushed = http.status()?["storage"]["unflushed_entries"].as_u64().unwrap_or(0);
        server.crash_and_restart()?;
        let startup = server.last_startup();
        let count = server.count("orders")?;
        if count != n {
            bail!("after the crash {} of {} orders are back", count, n);
        }
        report.row(&format!("Restart after a crash, {} writes not yet flushed (replayed from the WAL)", unflushed), ms(startup));
    }
    let http = Http::new(&server)?;
    http.json(reqwest::Method::POST, "/flush", Some(&json!({})))?;
    server.crash_and_restart()?;
    let startup = server.last_startup();
    if server.count("orders")? != n {
        bail!("documents missing after the second crash");
    }
    report.row(&format!("Restart after a crash, {} documents flushed to SSTables", n), ms(startup));
    server.stop()?;
    server.launch()?;
    report.row("Restart after a graceful stop", ms(server.last_startup()));
    Ok(())
}

fn failover(report: &mut Report, interval: Option<u64>) -> Result<()> {
    let ports = (0..3).map(|_| TestServer::free_port()).collect::<Result<Vec<_>>>()?;
    let lattice = format!("bench-{}", ports[0]);
    let opts = |i: usize, ram: u64| TestOptions {
        lattice: Some(lattice.clone()),
        discovery_port: Some(ports[i]),
        peers: ports.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| format!("127.0.0.1:{}", p)).collect(),
        ram_mb: Some(ram),
        discovery_interval_seconds: interval,
        ..TestOptions::default()
    };
    let mut leader = TestServer::start_with(opts(0, 4096))?;
    let next = TestServer::start_with(opts(1, 2048))?;
    let third = TestServer::start_with(opts(2, 1024))?;
    let role = |s: &TestServer| -> String {
        s.request(reqwest::Method::GET, "/status", None, &[]).map(|r| r.body["hex_type"].as_str().unwrap_or_default().to_string()).unwrap_or_default()
    };
    let active = |s: &TestServer| -> usize {
        s.request(reqwest::Method::GET, "/status", None, &[])
            .map(|r| r.body["network"]["lattice"]["hexes"].as_array().map(|h| h.iter().filter(|x| x["status"] == "active").count()).unwrap_or(0))
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while [&leader, &next, &third].iter().any(|s| active(s) != 3) || role(&leader) != "Overseer" {
        if Instant::now() > deadline {
            bail!("the lattice didn't form");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    leader.insert("orders", &json!({ "before": true }))?;
    let killed = Instant::now();
    leader.kill();
    let mut elected = None;
    loop {
        if elected.is_none() && role(&next) == "Overseer" {
            elected = Some(killed.elapsed());
        }
        if elected.is_some() && next.request(reqwest::Method::POST, "/orders", Some(&json!({ "after": true })), &[]).map(|r| r.status.as_u16() == 201).unwrap_or(false) {
            break;
        }
        if killed.elapsed() > Duration::from_secs(180) {
            bail!("no failover within 3 minutes");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let writable = killed.elapsed();
    let label = match interval {
        Some(i) => format!("discovery every {} s", i),
        None => "default settings (discovery every 10 s)".into(),
    };
    report.row(&format!("Overseer killed, {}: new Overseer elected", label), ms(elected.unwrap_or(writable)));
    report.row(&format!("Overseer killed, {}: new Overseer accepting writes", label), ms(writable));
    Ok(())
}

fn main() -> Result<()> {
    let quick = std::env::args().any(|a| a == "quick");
    let mut report = Report { text: String::new() };
    let started = Instant::now();
    let machine = machine();
    println!("HexDB benchmarks on {}", machine);

    let server_bin = build_and_sizes(&mut report)?;
    std::env::set_var("HEXDB_SERVER_BIN", &server_bin);

    let count = if quick { 20_000 } else { 100_000 };
    let mut rng = Rng(42);
    let orders: Vec<Vec<u8>> = (0..count).map(|i| serde_json::to_vec(&order(&mut rng, i as u64)).unwrap_or_default()).collect();

    crypto_and_compression(&mut report, &orders)?;
    vertices(&mut report, &orders)?;
    server_reads_and_writes(&mut report, &orders)?;
    crash_recovery(&mut report, &orders)?;
    if !quick {
        report.section(
            "Failover (three hexes on one machine)",
            "The Overseer of a three-hex lattice is killed. A hex is marked lost after three missed discovery rounds; then the others elect a new Overseer.",
        );
        failover(&mut report, Some(1))?;
        failover(&mut report, None)?;
    }

    let header = format!(
        "# HexDB benchmark results\n\nMachine: {}. HexDB {}, release build. Run with `cargo run --release -p hexdb_bench{}` ({} s).\n",
        machine,
        env!("CARGO_PKG_VERSION"),
        if quick { " -- quick" } else { "" },
        started.elapsed().as_secs()
    );
    let text = header + &report.text;
    let dir = workspace().join("bench").join("results");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}-{}.md", std::env::consts::OS, if quick { "quick" } else { "full" }));
    std::fs::write(&file, &text)?;
    println!("\nWrote {}", file.display());
    Ok(())
}
