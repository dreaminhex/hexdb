// HexDB: malware scanning of uploaded files
//
// Two engines, both optional and both free:
// - ClamAV, reached through its daemon (`clamd`) over TCP or a Unix socket. HexDB
//   never links libclamav: the daemon is a separate program, so ClamAV's GPL stays
//   with it, and its signatures (several hundred MB in memory) live in one scanner
//   the whole lattice shares.
// - YARA-X (VirusTotal's Rust YARA), built in when the `yara` feature is on: HexDB's
//   own rules (rules/hexdb.yar) plus any in files.scan.yara_rules, compiled at startup.
//
// Scanning is on by default. `clamd = "auto"` looks for a ClamAV on this machine at
// startup; once ClamAV is configured or found, uploads are held while it can't answer.
// Without one, uploads are scanned by YARA-X alone and HexDB says so loudly.
//
// A file is scanned after its chunks are stored and before its document commits
// (see `engine::files`), so nothing reaches another hex until it passed.

use crate::config::ScanConfig;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// What a scan found.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ScanReport {
    /// `clean`, `infected`, or `not_scanned` (larger than every engine's limit).
    pub status: String,
    /// What each engine detected (empty unless infected).
    pub detections: Vec<Detection>,
    /// The engines that scanned it, with their versions (ClamAV's includes its signature version).
    pub engines: Vec<String>,
    /// Engines that skipped it, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    pub scanned_at: DateTime<Utc>,
}

impl ScanReport {
    pub fn is_infected(&self) -> bool {
        self.status == "infected"
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Detection {
    pub engine: String,
    pub name: String,
}

/// The scanner couldn't give a verdict (unreachable, timed out, or an error):
/// the upload is held, never accepted unscanned.
#[derive(Debug)]
pub struct ScanUnavailable(pub String);

impl std::fmt::Display for ScanUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScanUnavailable {}

/// Where clamd listens.
#[derive(Debug, Clone, PartialEq)]
pub enum ClamdAddress {
    Tcp(String),
    #[cfg_attr(not(unix), allow(dead_code))]
    Unix(std::path::PathBuf),
}

impl ClamdAddress {
    /// `tcp://host:port`, `host:port`, `unix:/path` or an absolute path (Unix sockets, not on Windows).
    pub fn parse(text: &str) -> Result<ClamdAddress> {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix("tcp://") {
            return Ok(ClamdAddress::Tcp(rest.trim_end_matches('/').to_string()));
        }
        let path = text.strip_prefix("unix:").map(str::to_string).or_else(|| text.starts_with('/').then(|| text.to_string()));
        if let Some(path) = path {
            if cfg!(not(unix)) {
                bail!("files.scan.clamd: Unix sockets aren't available on this platform; use tcp://host:3310");
            }
            return Ok(ClamdAddress::Unix(path.into()));
        }
        if text.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()) {
            return Ok(ClamdAddress::Tcp(text.to_string()));
        }
        bail!("files.scan.clamd must be tcp://host:port, host:port or a Unix socket path (got '{}')", text)
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A client for clamd's protocol (`man clamd`): commands are `z`-prefixed and
/// NUL-terminated, and INSTREAM sends the content as length-prefixed pieces.
#[derive(Debug, Clone)]
pub struct Clamd {
    address: ClamdAddress,
    timeout: Duration,
}

/// Content is sent to clamd in pieces of at most this many bytes.
const PIECE: usize = 256 * 1024;

impl Clamd {
    pub fn new(address: ClamdAddress, timeout: Duration) -> Clamd {
        Clamd { address, timeout }
    }

    async fn connect(&self) -> Result<Box<dyn Stream>> {
        let connect = async {
            Ok::<Box<dyn Stream>, anyhow::Error>(match &self.address {
                ClamdAddress::Tcp(addr) => Box::new(tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connecting to clamd at {}", addr))?),
                #[cfg(unix)]
                ClamdAddress::Unix(path) => Box::new(tokio::net::UnixStream::connect(path).await.with_context(|| format!("connecting to clamd at {}", path.display()))?),
                #[cfg(not(unix))]
                ClamdAddress::Unix(_) => bail!("Unix sockets aren't available on this platform"),
            })
        };
        tokio::time::timeout(Duration::from_secs(10), connect).await.context("connecting to clamd timed out")?
    }

    /// Send a simple command and read its NUL-terminated answer.
    async fn command(&self, command: &str) -> Result<String> {
        let mut stream = self.connect().await?;
        stream.write_all(format!("z{}\0", command).as_bytes()).await?;
        read_answer(&mut stream, Duration::from_secs(10)).await
    }

    /// clamd's version and signature version: `ClamAV 1.4.1/27450/Mon Nov  4 08:00:00 2026`.
    pub async fn version(&self) -> Result<String> {
        self.command("VERSION").await
    }

    pub async fn ping(&self) -> Result<()> {
        let answer = self.command("PING").await?;
        if answer != "PONG" {
            bail!("clamd answered '{}' to PING", answer);
        }
        Ok(())
    }

    /// Stream content to clamd while reading its answer, so an early answer (over its
    /// StreamMaxLength, which clamd follows by closing the connection) is never lost.
    /// Content still to come after an early answer is left in `pieces`.
    async fn scan(&self, pieces: &mut mpsc::Receiver<std::io::Result<Arc<Vec<u8>>>>, keep: &mut Option<Vec<u8>>, keep_limit: usize) -> Result<ClamdVerdict> {
        let stream = self.connect().await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        writer.write_all(b"zINSTREAM\0").await?;
        let send = async {
            while let Some(piece) = pieces.recv().await {
                let piece = piece.context("reading the file to scan it")?;
                keep_piece(keep, &piece, keep_limit);
                for part in piece.chunks(PIECE) {
                    writer.write_all(&(part.len() as u32).to_be_bytes()).await?;
                    writer.write_all(part).await?;
                }
            }
            writer.write_all(&[0, 0, 0, 0]).await?;
            writer.flush().await?;
            Ok::<_, anyhow::Error>(())
        };
        let answer = read_answer(&mut reader, self.timeout + Duration::from_secs(30));
        tokio::pin!(send, answer);
        let answer = tokio::select! {
            // clamd answered before everything was sent: over its limit, or an error.
            answer = &mut answer => answer?,
            sent = tokio::time::timeout(self.timeout, &mut send) => match sent {
                Ok(Ok(())) => answer.await?,
                // A failed send usually means clamd answered and closed: its answer says why.
                // If the answer was lost with the connection, say what usually causes it.
                Ok(Err(e)) => answer.await.map_err(|_| {
                    e.context("clamd closed the connection while the file was sent; if clamd's StreamMaxLength is smaller than files.scan.max_size_mb, raise it")
                })?,
                Err(_) => bail!("clamd didn't finish scanning within {} s", self.timeout.as_secs()),
            },
        };
        parse_instream_answer(&answer)
    }
}

/// Add a piece to the copy YARA-X will scan, dropping the copy once it's over `limit`.
fn keep_piece(keep: &mut Option<Vec<u8>>, piece: &[u8], limit: usize) {
    if let Some(buffer) = keep.as_mut() {
        if buffer.len() + piece.len() <= limit {
            buffer.extend_from_slice(piece);
        } else {
            *keep = None;
        }
    }
}

#[derive(Debug, PartialEq)]
enum ClamdVerdict {
    Clean,
    Found(String),
    TooLarge,
}

/// Parse clamd's answer to INSTREAM: `stream: OK`, `stream: <name> FOUND`, or an error.
fn parse_instream_answer(answer: &str) -> Result<ClamdVerdict> {
    let body = answer.strip_prefix("stream: ").unwrap_or(answer).trim();
    if body == "OK" {
        return Ok(ClamdVerdict::Clean);
    }
    if let Some(name) = body.strip_suffix(" FOUND") {
        return Ok(ClamdVerdict::Found(name.trim().to_string()));
    }
    if body.contains("size limit exceeded") {
        return Ok(ClamdVerdict::TooLarge);
    }
    bail!("clamd: {}", body)
}

async fn read_answer<R: AsyncRead + Unpin>(stream: &mut R, timeout: Duration) -> Result<String> {
    let read = async {
        let mut answer = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 || byte[0] == 0 || byte[0] == b'\n' {
                break;
            }
            answer.push(byte[0]);
            if answer.len() > 4096 {
                bail!("clamd's answer is too long");
            }
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&answer).trim().to_string())
    };
    let answer = tokio::time::timeout(timeout, read).await.context("clamd didn't answer")??;
    if answer.is_empty() {
        bail!("clamd closed the connection without an answer");
    }
    Ok(answer)
}

/// YARA rules compiled from a directory.
#[cfg(feature = "yara")]
pub struct Yara {
    rules: yara_x::Rules,
    count: usize,
    /// A hash of the rule sources, so a changed rule set rescans stored files.
    fingerprint: String,
    /// Rule files that didn't compile and were left out (file: error).
    failed: Vec<String>,
}

#[cfg(feature = "yara")]
impl Yara {
    /// HexDB's own rules, plus every `.yar` and `.yara` file under `dir` if given
    /// (each file in its own namespace, so rule names from different sets don't clash).
    pub fn load(dir: Option<&std::path::Path>) -> Result<Yara> {
        let mut compiler = yara_x::Compiler::new();
        compiler.new_namespace("hexdb");
        compiler.add_source(BUILT_IN_RULES).map_err(|e| anyhow::anyhow!("HexDB's built-in YARA rules: {}", e))?;
        let mut files = Vec::new();
        if let Some(dir) = dir {
            collect_rule_files(dir, &mut files).with_context(|| format!("reading files.scan.yara_rules ({})", dir.display()))?;
            files.sort();
            if files.is_empty() {
                bail!("files.scan.yara_rules ({}) has no .yar or .yara files", dir.display());
            }
        }
        let dir = dir.unwrap_or(std::path::Path::new(""));
        let mut failed = Vec::new();
        let mut hasher = blake3::Hasher::new();
        hasher.update(BUILT_IN_RULES.as_bytes());
        for file in &files {
            let source = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
            hasher.update(file.to_string_lossy().as_bytes());
            hasher.update(source.as_bytes());
            let namespace = file.strip_prefix(dir).unwrap_or(file).to_string_lossy().replace(['\\', '/', '.', '-', ' '], "_");
            compiler.new_namespace(&namespace);
            // A file that doesn't compile is left out (and reported) rather than
            // stopping HexDB: the other rules and ClamAV keep scanning.
            if let Err(e) = compiler.add_source(source.as_str()) {
                let first_line = e.to_string().lines().next().unwrap_or_default().to_string();
                tracing::error!("❌ YARA rules {} don't compile and are left out: {}", file.display(), e);
                failed.push(format!("{}: {}", file.display(), first_line));
            }
        }
        let rules = compiler.build();
        let count = rules.iter().count();
        let fingerprint = format!("{} rules, {}", count, &hasher.finalize().to_hex()[..16]);
        Ok(Yara { rules, count, fingerprint, failed })
    }

    /// Names (`namespace:rule`) of the rules that match.
    fn scan(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut scanner = yara_x::Scanner::new(&self.rules);
        scanner.set_timeout(Duration::from_secs(60));
        let results = scanner.scan(data).map_err(|e| anyhow::anyhow!("YARA-X: {}", e))?;
        Ok(results.matching_rules().map(|r| format!("{}:{}", r.namespace(), r.identifier())).collect())
    }
}

/// HexDB's own YARA rules, compiled in.
#[cfg(feature = "yara")]
const BUILT_IN_RULES: &str = include_str!("../rules/hexdb.yar");

#[cfg(feature = "yara")]
fn collect_rule_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rule_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("yar") || e.eq_ignore_ascii_case("yara")) {
            out.push(path);
        }
    }
    Ok(())
}

/// Where a ClamAV is looked for on this machine with `clamd = "auto"`.
const LOCAL_CLAMD: &[&str] = &["127.0.0.1:3310", "/var/run/clamav/clamd.ctl", "/run/clamav/clamd.ctl", "/var/run/clamd.scan/clamd.sock", "/tmp/clamd.sock", "/tmp/clamd.socket"];

/// The configured engines.
pub struct Scanner {
    /// ClamAV: set at once when configured (or off); with `clamd = "auto"`, the first
    /// call to `clamd()` looks for one on this machine (HexDB starts that in the background).
    clamd: tokio::sync::OnceCell<Option<Clamd>>,
    timeout: Duration,
    #[cfg(feature = "yara")]
    yara: Option<Arc<Yara>>,
    max_bytes: u64,
    yara_max_bytes: usize,
}

impl Scanner {
    /// The scanner `config` describes, or None when scanning is off.
    pub fn from_config(config: &ScanConfig) -> Result<Option<Scanner>> {
        if !config.enabled {
            return Ok(None);
        }
        let timeout = Duration::from_secs(config.timeout_seconds.max(1));
        let clamd = match config.clamd.trim() {
            "auto" => tokio::sync::OnceCell::new(),
            "" | "off" | "none" => tokio::sync::OnceCell::new_with(Some(None)),
            address => tokio::sync::OnceCell::new_with(Some(Some(Clamd::new(ClamdAddress::parse(address)?, timeout)))),
        };
        #[cfg(feature = "yara")]
        let yara = {
            let dir = config.yara_rules.trim();
            Some(Arc::new(Yara::load((!dir.is_empty()).then(|| std::path::Path::new(dir)))?))
        };
        #[cfg(not(feature = "yara"))]
        if !config.yara_rules.trim().is_empty() {
            bail!("files.scan.yara_rules is set, but this build has no YARA-X (built without the `yara` feature)");
        }
        Ok(Some(Scanner {
            clamd,
            timeout,
            #[cfg(feature = "yara")]
            yara,
            max_bytes: config.max_size_mb.saturating_mul(1024 * 1024),
            yara_max_bytes: usize::try_from(config.yara_max_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX),
        }))
    }

    /// ClamAV, once known: with `clamd = "auto"`, the first call looks for one on this
    /// machine. Once found it's required like a configured one (uploads are held while
    /// it's down); without one, YARA-X scans alone.
    pub async fn clamd(&self) -> Option<&Clamd> {
        self.clamd
            .get_or_init(|| async {
                for candidate in LOCAL_CLAMD {
                    let Ok(address) = ClamdAddress::parse(candidate) else { continue };
                    let clamd = Clamd::new(address, self.timeout);
                    if tokio::time::timeout(Duration::from_secs(2), clamd.ping()).await.is_ok_and(|r| r.is_ok()) {
                        tracing::info!("🛡️ Found ClamAV at {} for malware scanning.", candidate);
                        return Some(clamd);
                    }
                }
                None
            })
            .await
            .as_ref()
    }

    /// Why scanning is weaker than it should be, if it is: shown at startup, in
    /// `/status` and on the dashboard. None while ClamAV is still being looked for.
    pub fn warning(&self) -> Option<String> {
        #[cfg(feature = "yara")]
        if let Some(yara) = self.yara.as_ref().filter(|y| !y.failed.is_empty()) {
            return Some(format!("{} YARA rule file(s) don't compile and are left out: {}", yara.failed.len(), yara.failed.join("; ")));
        }
        match self.clamd.get() {
            None | Some(Some(_)) => return None,
            Some(None) => {}
        }
        #[cfg(feature = "yara")]
        let yara = self.yara.is_some();
        #[cfg(not(feature = "yara"))]
        let yara = false;
        Some(if yara {
            "ClamAV isn't configured: uploads are scanned by HexDB's built-in YARA rules only, which catch far less. Install ClamAV and set files.scan.clamd (see the manual's Malware scanning).".into()
        } else {
            "No malware scanner: this build has no YARA-X and ClamAV isn't configured, so uploads aren't scanned. Install ClamAV and set files.scan.clamd (see the manual's Malware scanning).".into()
        })
    }

    /// What the verdicts depend on: ClamAV's version and signature version, and the
    /// YARA rules. When it changes, stored files are scanned again. None while a
    /// configured ClamAV can't answer (no rescan against half the engines).
    pub async fn fingerprint(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(clamd) = self.clamd().await {
            parts.push(clamd.version().await.ok()?);
        }
        #[cfg(feature = "yara")]
        if let Some(yara) = &self.yara {
            parts.push(format!("YARA-X {}", yara.fingerprint));
        }
        Some(parts.join(" | "))
    }

    /// The engines and their versions, for `/status` (reaches clamd).
    pub async fn engines(&self) -> Vec<EngineStatus> {
        let mut engines = Vec::new();
        if let Some(clamd) = self.clamd().await {
            engines.push(match clamd.version().await {
                Ok(version) => EngineStatus { engine: "ClamAV".into(), version: Some(version), reachable: true, error: None },
                Err(e) => EngineStatus { engine: "ClamAV".into(), version: None, reachable: false, error: Some(format!("{:#}", e)) },
            });
        }
        #[cfg(feature = "yara")]
        if let Some(yara) = &self.yara {
            engines.push(EngineStatus { engine: "YARA-X".into(), version: Some(format!("{} rules", yara.count)), reachable: true, error: None });
        }
        engines
    }

    /// Scan `size` bytes read from `pieces` (the file's content in order).
    pub async fn scan(&self, size: u64, mut pieces: mpsc::Receiver<std::io::Result<Arc<Vec<u8>>>>) -> Result<ScanReport, ScanUnavailable> {
        let mut report = ScanReport { status: "clean".into(), detections: Vec::new(), engines: Vec::new(), skipped: Vec::new(), scanned_at: Utc::now() };
        if size > self.max_bytes {
            report.status = "not_scanned".into();
            report.skipped.push(format!("larger than files.scan.max_size_mb ({} MB)", self.max_bytes / (1024 * 1024)));
            return Ok(report);
        }
        #[cfg(feature = "yara")]
        let wants_yara = self.yara.is_some() && size as usize <= self.yara_max_bytes;
        #[cfg(not(feature = "yara"))]
        let wants_yara = false;
        let mut kept: Option<Vec<u8>> = wants_yara.then(|| Vec::with_capacity(size as usize));

        let mut scanned = false;
        if let Some(clamd) = self.clamd().await {
            let verdict = clamd.scan(&mut pieces, &mut kept, self.yara_max_bytes).await.map_err(|e| ScanUnavailable(format!("ClamAV: {:#}", e)))?;
            // clamd may answer before it has everything (over its limit): YARA-X still reads the rest.
            while let Some(piece) = pieces.recv().await {
                let piece = piece.map_err(|e| ScanUnavailable(format!("reading the file to scan it: {}", e)))?;
                keep_piece(&mut kept, &piece, self.yara_max_bytes);
            }
            let version = clamd.version().await.unwrap_or_else(|_| "ClamAV".into());
            match verdict {
                ClamdVerdict::Clean => {
                    report.engines.push(version);
                    scanned = true;
                }
                ClamdVerdict::Found(name) => {
                    report.engines.push(version);
                    report.detections.push(Detection { engine: "ClamAV".into(), name });
                    scanned = true;
                }
                ClamdVerdict::TooLarge => report.skipped.push("ClamAV: larger than clamd's StreamMaxLength".into()),
            }
        } else if wants_yara {
            // No clamd: read the content for YARA-X alone.
            let mut buffer = Vec::with_capacity(size as usize);
            while let Some(piece) = pieces.recv().await {
                buffer.extend_from_slice(&piece.map_err(|e| ScanUnavailable(format!("reading the file to scan it: {}", e)))?);
            }
            kept = Some(buffer);
        }

        #[cfg(feature = "yara")]
        if let Some(yara) = &self.yara {
            match kept {
                Some(data) => {
                    let count = yara.count;
                    let yara = yara.clone();
                    let matches = tokio::task::spawn_blocking(move || yara.scan(&data))
                        .await
                        .map_err(|e| ScanUnavailable(format!("YARA-X: {}", e)))?
                        .map_err(|e| ScanUnavailable(format!("{:#}", e)))?;
                    report.engines.push(format!("YARA-X ({} rules)", count));
                    report.detections.extend(matches.into_iter().map(|name| Detection { engine: "YARA-X".into(), name }));
                    scanned = true;
                }
                None => report.skipped.push(format!("YARA-X: larger than files.scan.yara_max_mb ({} MB)", self.yara_max_bytes / (1024 * 1024))),
            }
        }
        #[cfg(not(feature = "yara"))]
        drop(kept);

        report.status = if !report.detections.is_empty() {
            "infected"
        } else if scanned {
            "clean"
        } else {
            "not_scanned"
        }
        .into();
        Ok(report)
    }
}

/// The EICAR anti-virus test file, which every scanner flags (harmless). Built from hex
/// so no source file contains it.
pub fn eicar_test_file() -> Vec<u8> {
    const HEX: &str = "58354F2150254041505B345C505A58353428505E2937434329377D2445494341522D5354414E444152442D414E544956495255532D544553542D46494C452124482B482A";
    (0..HEX.len()).step_by(2).map(|i| u8::from_str_radix(&HEX[i..i + 2], 16).unwrap_or(0)).collect()
}

/// One engine's state, for `/status`.
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub engine: String,
    pub version: Option<String>,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamd_addresses_parse() {
        assert_eq!(ClamdAddress::parse("tcp://clamav:3310").unwrap(), ClamdAddress::Tcp("clamav:3310".into()));
        assert_eq!(ClamdAddress::parse("127.0.0.1:3310").unwrap(), ClamdAddress::Tcp("127.0.0.1:3310".into()));
        assert_eq!(ClamdAddress::parse("[fdaa::3]:3310").unwrap(), ClamdAddress::Tcp("[fdaa::3]:3310".into()));
        #[cfg(unix)]
        {
            assert_eq!(ClamdAddress::parse("/run/clamav/clamd.ctl").unwrap(), ClamdAddress::Unix("/run/clamav/clamd.ctl".into()));
            assert_eq!(ClamdAddress::parse("unix:/tmp/clamd.sock").unwrap(), ClamdAddress::Unix("/tmp/clamd.sock".into()));
        }
        assert!(ClamdAddress::parse("clamav").is_err());
        assert!(ClamdAddress::parse("").is_err());
    }

    #[cfg(feature = "yara")]
    #[test]
    fn built_in_rules_find_the_eicar_test_file() {
        let yara = Yara::load(None).unwrap();
        assert_eq!(yara.scan(&eicar_test_file()).unwrap(), vec!["hexdb:EICAR_Test_File".to_string()]);
        assert!(yara.scan(b"hello, world").unwrap().is_empty());
    }

    #[cfg(feature = "yara")]
    #[test]
    fn a_rule_file_that_doesnt_compile_is_left_out() {
        let dir = std::env::temp_dir().join(format!("hexdb-yara-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good.yar"), "rule Good_Rule { strings: $a = \"hexdb-good-marker\" condition: $a }").unwrap();
        std::fs::write(dir.join("broken.yar"), "rule Broken_Rule { condition: this is not yara }").unwrap();
        let yara = Yara::load(Some(&dir)).unwrap();
        assert_eq!(yara.failed.len(), 1, "{:?}", yara.failed);
        assert!(yara.failed[0].contains("broken.yar"));
        assert_eq!(yara.scan(b"... hexdb-good-marker ...").unwrap(), vec!["good_yar:Good_Rule".to_string()]);
        assert!(!yara.scan(&eicar_test_file()).unwrap().is_empty(), "the built-in rules still load");
        let scanner = Scanner { clamd: tokio::sync::OnceCell::new_with(Some(None)), timeout: Duration::from_secs(1), yara: Some(Arc::new(yara)), max_bytes: 1 << 20, yara_max_bytes: 1 << 20 };
        assert!(scanner.warning().unwrap().contains("broken.yar"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Compiles a rules directory named by HEXDB_TEST_YARA_RULES (e.g. YARA Forge's core
    /// package): `cargo test -p hexdb_core yara_rules_from -- --ignored --nocapture`.
    #[cfg(feature = "yara")]
    #[test]
    #[ignore]
    fn yara_rules_from_hexdb_test_yara_rules() {
        let dir = std::env::var("HEXDB_TEST_YARA_RULES").expect("HEXDB_TEST_YARA_RULES");
        let started = std::time::Instant::now();
        let yara = Yara::load(Some(std::path::Path::new(&dir))).unwrap();
        println!("{} in {:?}", yara.fingerprint, started.elapsed());
        assert!(yara.scan(&eicar_test_file()).unwrap().contains(&"hexdb:EICAR_Test_File".to_string()));
    }

    /// A fake clamd: answers PING and VERSION, and flags INSTREAM content that contains
    /// `flag`; `limit` imitates StreamMaxLength. Returns its address.
    async fn fake_clamd(flag: &'static [u8], limit: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut command = Vec::new();
                    let mut byte = [0u8; 1];
                    while socket.read_exact(&mut byte).await.is_ok() && byte[0] != 0 {
                        command.push(byte[0]);
                    }
                    let answer: Vec<u8> = match command.as_slice() {
                        b"zPING" => b"PONG\0".to_vec(),
                        b"zVERSION" => b"ClamAV 1.4.1/27450/Mon Nov  4 08:00:00 2026\0".to_vec(),
                        b"zINSTREAM" => {
                            let mut data = Vec::new();
                            let mut too_large = false;
                            loop {
                                let mut len = [0u8; 4];
                                if socket.read_exact(&mut len).await.is_err() {
                                    return;
                                }
                                let len = u32::from_be_bytes(len) as usize;
                                if len == 0 {
                                    break;
                                }
                                let mut piece = vec![0u8; len];
                                if socket.read_exact(&mut piece).await.is_err() {
                                    return;
                                }
                                data.extend_from_slice(&piece);
                                // Read on (like clamd's buffer) but remember it was too much.
                                too_large |= data.len() > limit;
                            }
                            if too_large {
                                b"INSTREAM size limit exceeded. ERROR\0".to_vec()
                            } else if data.windows(flag.len()).any(|w| w == flag) {
                                b"stream: Test.Flagged FOUND\0".to_vec()
                            } else {
                                b"stream: OK\0".to_vec()
                            }
                        }
                        _ => b"UNKNOWN COMMAND\0".to_vec(),
                    };
                    let _ = socket.write_all(&answer).await;
                });
            }
        });
        address
    }

    fn pieces(parts: Vec<Vec<u8>>) -> mpsc::Receiver<std::io::Result<Arc<Vec<u8>>>> {
        let (tx, rx) = mpsc::channel(parts.len().max(1));
        for part in parts {
            tx.try_send(Ok(Arc::new(part))).unwrap();
        }
        rx
    }

    fn config(clamd: &str) -> ScanConfig {
        ScanConfig { clamd: clamd.into(), timeout_seconds: 10, ..ScanConfig::default() }
    }

    #[tokio::test]
    async fn clamd_and_yara_give_one_verdict() {
        let address = fake_clamd(b"MALWARE", 1024 * 1024).await;
        let scanner = Scanner::from_config(&config(&address)).unwrap().unwrap();
        assert!(scanner.clamd().await.is_some());
        assert!(scanner.warning().is_none(), "ClamAV configured");

        let clean = scanner.scan(11, pieces(vec![b"hello, ".to_vec(), b"world".to_vec()])).await.unwrap();
        assert_eq!(clean.status, "clean");
        assert!(clean.engines[0].starts_with("ClamAV 1.4.1/27450"), "{:?}", clean.engines);

        // Split across pieces, as chunks are.
        let infected = scanner.scan(14, pieces(vec![b"some MAL".to_vec(), b"WARE..".to_vec()])).await.unwrap();
        assert!(infected.is_infected());
        assert_eq!(infected.detections[0], Detection { engine: "ClamAV".into(), name: "Test.Flagged".into() });

        #[cfg(feature = "yara")]
        {
            let eicar = eicar_test_file();
            let report = scanner.scan(eicar.len() as u64, pieces(vec![eicar])).await.unwrap();
            assert!(report.detections.iter().any(|d| d.engine == "YARA-X" && d.name == "hexdb:EICAR_Test_File"), "{:?}", report);
        }
    }

    #[tokio::test]
    async fn an_unreachable_clamd_holds_the_upload() {
        // A port nothing listens on.
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().to_string();
        let scanner = Scanner::from_config(&config(&unused)).unwrap().unwrap();
        let err = scanner.scan(5, pieces(vec![b"hello".to_vec()])).await.unwrap_err();
        assert!(err.0.starts_with("ClamAV"), "{}", err);
    }

    #[tokio::test]
    async fn over_the_limits_is_recorded_not_hidden() {
        let address = fake_clamd(b"MALWARE", 8).await;
        let scanner = Scanner::from_config(&ScanConfig { max_size_mb: 1, yara_max_mb: 0, ..config(&address) }).unwrap().unwrap();
        // Over clamd's StreamMaxLength, and over yara_max_mb: no engine scanned it.
        let report = scanner.scan(16, pieces(vec![b"0123456789abcdef".to_vec()])).await.unwrap();
        assert_eq!(report.status, "not_scanned", "{:?}", report);
        assert_eq!(report.skipped.len(), if cfg!(feature = "yara") { 2 } else { 1 }, "{:?}", report);
        // Over files.scan.max_size_mb: not even sent.
        let report = scanner.scan(2 * 1024 * 1024, pieces(vec![])).await.unwrap();
        assert_eq!(report.status, "not_scanned");
    }

    #[tokio::test]
    async fn without_clamd_it_warns() {
        let scanner = Scanner::from_config(&config("off")).unwrap().unwrap();
        assert!(scanner.warning().unwrap().contains("ClamAV isn't configured") || !cfg!(feature = "yara"));
        assert!(Scanner::from_config(&ScanConfig { enabled: false, ..ScanConfig::default() }).unwrap().is_none());
    }

    #[test]
    fn instream_answers_parse() {
        assert_eq!(parse_instream_answer("stream: OK").unwrap(), ClamdVerdict::Clean);
        assert_eq!(parse_instream_answer("stream: Win.Test.EICAR_HDB-1 FOUND").unwrap(), ClamdVerdict::Found("Win.Test.EICAR_HDB-1".into()));
        assert_eq!(parse_instream_answer("INSTREAM size limit exceeded. ERROR").unwrap(), ClamdVerdict::TooLarge);
        assert!(parse_instream_answer("stream: Can't allocate memory ERROR").is_err());
    }
}
