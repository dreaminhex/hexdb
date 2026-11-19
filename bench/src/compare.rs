//! Comparing HexDB versions on one machine:
//!
//!   cargo run --release -p hexdb_bench -- compare <old hexdb_api> <new hexdb_api> [rounds]
//!
//! Each round starts a fresh server from each binary in turn, alternating which goes
//! first (A B, then B A, ...), so drift on the machine (heat, other load, the disk's
//! cache) falls on both versions alike. Every round runs the same workload on the same
//! data. Latencies from all rounds are pooled into p50 / p95 / p99; throughputs are the
//! median of the rounds, with their range so the noise is visible. Memory is the server's
//! resident set (RSS) after loading, and its peak.
//!
//! Numbers from different machines (or a machine doing other work) aren't comparable:
//! run both versions here, in one go, on an otherwise idle host.

use crate::{options, order, percentile, Http, Rng, LIMITS};
use anyhow::{bail, Context, Result};
use hexdb_tests::TestServer;
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Documents loaded per round.
const DOCS: usize = 50_000;

/// One version's results from every round.
#[derive(Default)]
struct Samples {
    /// Operation -> latencies, pooled over rounds.
    latency: BTreeMap<&'static str, Vec<Duration>>,
    /// Operation -> one rate (operations per second) per round.
    rate: BTreeMap<&'static str, Vec<f64>>,
    /// One value per round, in bytes.
    rss_loaded: Vec<f64>,
    rss_peak: Vec<f64>,
    version: String,
}

impl Samples {
    fn time<T>(&mut self, op: &'static str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let started = Instant::now();
        let out = f()?;
        self.latency.entry(op).or_default().push(started.elapsed());
        Ok(out)
    }
    fn rate(&mut self, op: &'static str, count: usize, took: Duration) {
        self.rate.entry(op).or_default().push(count as f64 / took.as_secs_f64().max(1e-9));
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        bail!("usage: hexdb_bench compare <hexdb_api A> <hexdb_api B> [rounds]");
    };
    let rounds: usize = args.get(2).map(|r| r.parse()).transpose().context("rounds must be a number")?.unwrap_or(4).max(2);
    let bins = [absolute(a)?, absolute(b)?];

    let mut rng = Rng(42);
    let orders: Vec<Vec<u8>> = (0..DOCS).map(|i| serde_json::to_vec(&order(&mut rng, i as u64)).unwrap_or_default()).collect();

    let machine = crate::machine();
    println!("Comparing on {}:\n  A = {}\n  B = {}\n  {} rounds of {} documents", machine, bins[0].display(), bins[1].display(), rounds, DOCS);
    let started = Instant::now();
    let mut results = [Samples::default(), Samples::default()];
    for round in 0..rounds {
        let order: [usize; 2] = if round % 2 == 0 { [0, 1] } else { [1, 0] };
        for which in order {
            println!("Round {} of {}: {}", round + 1, rounds, if which == 0 { "A" } else { "B" });
            one_round(&bins[which], &orders, &mut results[which]).with_context(|| format!("round {} with {}", round + 1, bins[which].display()))?;
        }
    }

    let text = report(&machine, &bins, rounds, &results, started.elapsed());
    print!("\n{}", text);
    let dir = crate::workspace().join("bench").join("results");
    std::fs::create_dir_all(&dir)?;
    let system = crate::label().unwrap_or_else(|| std::env::consts::OS.to_string());
    let file = dir.join(format!("compare-{}-{}-{}.md", system, results[0].version, results[1].version));
    std::fs::write(&file, &text)?;
    println!("Wrote {}", file.display());
    Ok(())
}

fn absolute(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if !path.is_file() {
        bail!("{} isn't a file (give the path to a hexdb_api binary)", path.display());
    }
    Ok(std::path::absolute(path)?)
}

/// One fresh server from `bin`, the whole workload, then its memory.
fn one_round(bin: &Path, orders: &[Vec<u8>], out: &mut Samples) -> Result<()> {
    let mut opts = options(LIMITS);
    opts.server_bin = Some(bin.to_path_buf());
    let server = TestServer::start_with(opts)?;
    let pid = server.pid().context("server isn't running")?;
    let http = Http::new(&server)?;
    out.version = http.json(reqwest::Method::GET, "/health", None).ok().and_then(|h| h["version"].as_str().map(String::from)).unwrap_or_else(|| "unknown".into());

    // Bulk load.
    let started = Instant::now();
    for chunk in orders.chunks(1000) {
        let body = [b"[".as_slice(), &chunk.join(&b","[..]), b"]"].concat();
        let (status, text) = http.send(reqwest::Method::POST, "/orders/_bulk", Some(body))?;
        if status != 201 {
            bail!("bulk insert returned {}: {}", status, String::from_utf8_lossy(&text));
        }
    }
    out.rate("Bulk insert (1,000 per request), docs/s", orders.len(), started.elapsed());
    http.json(reqwest::Method::POST, "/tessellations/orders/indexes", Some(&json!({ "fields": ["customer"] })))?;
    http.json(reqwest::Method::POST, "/tessellations/orders/indexes", Some(&json!({ "fields": ["total"] })))?;

    // Single inserts, one client.
    for d in orders.iter().take(1000) {
        let status = out.time("Insert one document", || Ok(http.send(reqwest::Method::POST, "/singles", Some(d.clone()))?.0))?;
        if status != 201 {
            bail!("insert returned {}", status);
        }
    }

    // Inserts, 8 clients (group commit).
    let started = Instant::now();
    let per_thread = concurrent(8, |t, lat| {
        for d in orders.iter().skip(t * 250).take(250) {
            let s = Instant::now();
            http.send(reqwest::Method::POST, "/concurrent", Some(d.clone()))?;
            lat.push(s.elapsed());
        }
        Ok(())
    })?;
    out.rate("Inserts, 8 clients, writes/s", 2000, started.elapsed());
    out.latency.entry("Insert, 8 clients").or_default().extend(per_thread);

    // Reads by ID.
    let ids: Vec<String> = http.json(reqwest::Method::POST, "/orders/_query", Some(&json!({ "limit": 1000, "fields": ["id"], "total": false })))?["documents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|d| d["id"].as_str().map(String::from))
        .collect();
    for id in ids.iter().cycle().take(3000) {
        out.time("Read by ID", || http.send(reqwest::Method::GET, &format!("/orders/{}", id), None))?;
    }
    let started = Instant::now();
    let per_thread = concurrent(8, |t, lat| {
        for id in ids.iter().skip(t * 125).take(125).cycle().take(500) {
            let s = Instant::now();
            http.send(reqwest::Method::GET, &format!("/orders/{}", id), None)?;
            lat.push(s.elapsed());
        }
        Ok(())
    })?;
    out.rate("Reads by ID, 8 clients, reads/s", 4000, started.elapsed());
    out.latency.entry("Read by ID, 8 clients").or_default().extend(per_thread);

    // Queries.
    for i in 0..500 {
        let body = json!({ "filter": { "customer": format!("customer-{}", i * 7 % 5000) }, "limit": 50 });
        out.time("Indexed query (customer = ?), 50 results", || http.json(reqwest::Method::POST, "/orders/_query", Some(&body)))?;
    }
    for i in 0..200 {
        let low = (i * 3 % 400) as f64;
        let body = json!({ "filter": { "total": { "$gte": low } }, "sort": [{ "field": "total" }], "limit": 20, "total": false });
        out.time("Sorted page with a range on the sort field", || http.json(reqwest::Method::POST, "/orders/_query", Some(&body)))?;
    }
    for _ in 0..20 {
        out.time("SQL SELECT COUNT(*)", || http.json(reqwest::Method::POST, "/sql", Some(&json!({ "sql": "SELECT COUNT(*) FROM orders" }))))?;
    }
    for _ in 0..10 {
        out.time("SQL GROUP BY with SUM and AVG", || {
            http.json(reqwest::Method::POST, "/sql", Some(&json!({ "sql": "SELECT status, COUNT(*), SUM(total), AVG(total) FROM orders GROUP BY status" })))
        })?;
    }

    let (rss, peak) = memory(pid);
    out.rss_loaded.push(rss);
    out.rss_peak.push(peak);
    drop(server);
    Ok(())
}

/// Runs `f` on `threads` threads; returns every latency they recorded.
fn concurrent(threads: usize, f: impl Fn(usize, &mut Vec<Duration>) -> Result<()> + Sync) -> Result<Vec<Duration>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let f = &f;
                scope.spawn(move || -> Result<Vec<Duration>> {
                    let mut lat = Vec::new();
                    f(t, &mut lat)?;
                    Ok(lat)
                })
            })
            .collect();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().map_err(|_| anyhow::anyhow!("thread panicked"))??);
        }
        Ok(all)
    })
}

/// The process's resident set and its peak, in bytes (0 where unknown).
fn memory(pid: u32) -> (f64, f64) {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
        let kb = |key: &str| {
            status.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) * 1024.0
        };
        (kb("VmRSS:"), kb("VmHWM:"))
    }
    #[cfg(windows)]
    {
        let script = format!("$p = Get-Process -Id {}; \"$($p.WorkingSet64) $($p.PeakWorkingSet64)\"", pid);
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let mut parts = out.split_whitespace().map(|v| v.parse::<f64>().unwrap_or(0.0));
        (parts.next().unwrap_or(0.0), parts.next().unwrap_or(0.0))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        // ps reports the resident set in KB; the peak isn't available, so it's the same value.
        let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let rss = out.trim().parse::<f64>().unwrap_or(0.0) * 1024.0;
        (rss, rss)
    }
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

fn change(a: f64, b: f64) -> String {
    if a <= 0.0 {
        return "-".into();
    }
    format!("{:+.1}%", 100.0 * (b - a) / a)
}

fn report(machine: &str, bins: &[PathBuf; 2], rounds: usize, results: &[Samples; 2], took: Duration) -> String {
    let [a, b] = results;
    let mut t = String::new();
    let _ = writeln!(t, "# HexDB {} vs {}\n", a.version, b.version);
    let _ = writeln!(
        t,
        "Machine: {}. {} rounds of {} documents, alternating which version runs first; {} s in all.\n\n- A: HexDB {} (`{}`)\n- B: HexDB {} (`{}`)\n",
        machine,
        rounds,
        DOCS,
        took.as_secs(),
        a.version,
        bins[0].display(),
        b.version,
        bins[1].display()
    );
    let _ = writeln!(t, "Run with `cargo run --release -p hexdb_bench -- compare <A> <B> {}`. Both versions ran on this machine in one go; numbers from other machines aren't comparable.\n", rounds);

    let _ = writeln!(t, "## Latency (all rounds pooled; ms)\n\n| Operation | A p50 / p95 / p99 | B p50 / p95 / p99 | p50 change | p99 change |\n| --- | --- | --- | --- | --- |");
    for (op, samples_a) in &a.latency {
        let Some(samples_b) = b.latency.get(op) else { continue };
        let (mut sa, mut sb) = (samples_a.clone(), samples_b.clone());
        let p = |s: &mut [Duration], q: f64| percentile(s, q).as_secs_f64() * 1000.0;
        let (a50, a95, a99) = (p(&mut sa, 0.5), p(&mut sa, 0.95), p(&mut sa, 0.99));
        let (b50, b95, b99) = (p(&mut sb, 0.5), p(&mut sb, 0.95), p(&mut sb, 0.99));
        let _ = writeln!(t, "| {} | {:.2} / {:.2} / {:.2} | {:.2} / {:.2} / {:.2} | {} | {} |", op, a50, a95, a99, b50, b95, b99, change(a50, b50), change(a99, b99));
    }

    let _ = writeln!(t, "\n## Throughput (median of rounds, with the range)\n\n| Operation | A | B | Change |\n| --- | --- | --- | --- |");
    for (op, ra) in &a.rate {
        let Some(rb) = b.rate.get(op) else { continue };
        let range = |r: &[f64]| format!("{:.0} ({:.0} to {:.0})", median(r), r.iter().cloned().fold(f64::INFINITY, f64::min), r.iter().cloned().fold(0.0, f64::max));
        let _ = writeln!(t, "| {} | {} | {} | {} |", op, range(ra), range(rb), change(median(ra), median(rb)));
    }

    let mb = |v: f64| format!("{:.0} MB", v / crate::MB);
    let _ = writeln!(t, "\n## Server memory (median of rounds)\n\n| Measurement | A | B | Change |\n| --- | --- | --- | --- |");
    let (al, bl) = (median(&a.rss_loaded), median(&b.rss_loaded));
    let (ap, bp) = (median(&a.rss_peak), median(&b.rss_peak));
    let _ = writeln!(t, "| Resident set after the workload | {} | {} | {} |", mb(al), mb(bl), change(al, bl));
    let _ = writeln!(t, "| Peak resident set | {} | {} | {} |", mb(ap), mb(bp), change(ap, bp));
    let _ = writeln!(t, "\nA change within the throughput ranges above is noise, not a difference between the versions.");
    t
}
