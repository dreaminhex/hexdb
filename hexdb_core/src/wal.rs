// HexDB Core Write-Ahead Log (WAL) Module
//
// Every write is appended to the WAL before it is acknowledged. Records carry
// a sequence number and are compressed with Zstandard and encrypted with
// AES-256-GCM. A dedicated writer thread appends records in the order they
// were queued, batches whatever is waiting into one write and one fsync
// ("group commit"), and then acknowledges each record.
//
// The WAL is split into segment files named after the first sequence number
// they may contain (`wal/00000000000000000042.wal`). A flush rotates to a new
// segment, writes SSTables, and only then deletes the older segments, so a
// crash at any point leaves every acknowledged write in a segment or an SSTable.
//
// Record framing: [u32 big-endian length][12-byte nonce][ciphertext].

use crate::document::Document;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use ulid::Ulid;
use zstd::stream::{decode_all, encode_all};

const NONCE_LEN: usize = 12;
const MAX_RECORD_LEN: usize = 256 * 1024 * 1024;
const SEGMENT_EXTENSION: &str = "wal";

/// One logged write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalRecord {
    pub seq: u64,
    pub op: WalOp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WalOp {
    /// Insert or replace a document.
    Put(Document),
    /// Delete a document.
    Delete { tessellation: String, id: Ulid },
    /// Several operations applied atomically. They use consecutive sequence
    /// numbers starting at the record's `seq`. Batches are never nested.
    Batch(Vec<WalOp>),
}

impl WalRecord {
    /// The record's operations, each with its own sequence number.
    pub fn into_ops(self) -> Vec<(u64, WalOp)> {
        match self.op {
            WalOp::Batch(ops) => ops
                .into_iter()
                .enumerate()
                .map(|(i, op)| (self.seq + i as u64, op))
                .collect(),
            op => vec![(self.seq, op)],
        }
    }

    /// The highest sequence number used by this record.
    pub fn last_seq(&self) -> u64 {
        match &self.op {
            WalOp::Batch(ops) => self.seq + (ops.len() as u64).saturating_sub(1),
            _ => self.seq,
        }
    }
}

enum WalCommand {
    Append {
        record: WalRecord,
        ack: oneshot::Sender<std::result::Result<(), String>>,
    },
    Rotate {
        next_seq: u64,
        ack: oneshot::Sender<std::result::Result<u64, String>>,
    },
    Shutdown {
        ack: oneshot::Sender<std::result::Result<(), String>>,
    },
}

/// A pending acknowledgement for an appended record.
pub struct WalAck(oneshot::Receiver<std::result::Result<(), String>>);

impl WalAck {
    /// Wait until the record is durable.
    pub async fn wait(self) -> Result<()> {
        match self.0.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("WAL write failed: {}", e)),
            Err(_) => Err(anyhow!("WAL writer stopped before the write was acknowledged")),
        }
    }
}

/// Handle to the WAL writer thread.
pub struct WalWriter {
    tx: mpsc::Sender<WalCommand>,
    failed: Arc<AtomicBool>,
    thread: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl WalWriter {
    /// Start the writer thread with a new segment for records from `next_seq` on.
    pub fn start(dir: &Path, keys: &crate::crypt::KeyRing, compression_level: i32, sync: bool, next_seq: u64) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        let cipher = keys.current_cipher().clone();
        let file = open_segment(dir, next_seq)?;

        let (tx, rx) = mpsc::channel(4096);
        let failed = Arc::new(AtomicBool::new(false));
        let state = WriterState {
            dir: dir.to_path_buf(),
            cipher,
            compression_level,
            sync,
            file,
            failed: failed.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("hexdb-wal".into())
            .spawn(move || state.run(rx))
            .context("Failed to start the WAL writer thread")?;

        info!("📓 WAL writer started (segment {:020}).", next_seq);
        Ok(WalWriter {
            tx,
            failed,
            thread: std::sync::Mutex::new(Some(thread)),
        })
    }

    /// True after any WAL write has failed. Further writes are refused.
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// Queue a record. Records are written in the order they are queued; call
    /// this while holding the lock that assigns sequence numbers.
    pub async fn append(&self, record: WalRecord) -> Result<WalAck> {
        if self.has_failed() {
            bail!("The WAL is unavailable after an earlier write failure; restart HexDB.");
        }
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(WalCommand::Append { record, ack })
            .await
            .map_err(|_| anyhow!("The WAL writer has stopped"))?;
        Ok(WalAck(rx))
    }

    /// Start a new segment for records from `next_seq` on, after making
    /// everything queued so far durable. Returns the new segment's sequence number.
    /// Call this while holding the lock that assigns sequence numbers.
    pub async fn rotate(&self, next_seq: u64) -> Result<oneshot::Receiver<std::result::Result<u64, String>>> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(WalCommand::Rotate { next_seq, ack })
            .await
            .map_err(|_| anyhow!("The WAL writer has stopped"))?;
        Ok(rx)
    }

    /// Make everything queued durable and stop the writer thread.
    pub async fn shutdown(&self) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(WalCommand::Shutdown { ack }).await.is_err() {
            return Ok(()); // already stopped
        }
        let result = rx.await.map_err(|_| anyhow!("WAL writer stopped unexpectedly"))?;
        let handle = self.thread.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
        result.map_err(|e| anyhow!("Final WAL sync failed: {}", e))
    }
}

struct WriterState {
    dir: PathBuf,
    cipher: Aes256Gcm,
    compression_level: i32,
    sync: bool,
    file: BufWriter<File>,
    failed: Arc<AtomicBool>,
}

impl WriterState {
    fn run(mut self, mut rx: mpsc::Receiver<WalCommand>) {
        let mut pending: Vec<oneshot::Sender<std::result::Result<(), String>>> = Vec::new();

        while let Some(first) = rx.blocking_recv() {
            let mut batch = vec![first];
            while batch.len() < 4096 {
                match rx.try_recv() {
                    Ok(cmd) => batch.push(cmd),
                    Err(_) => break,
                }
            }

            for cmd in batch {
                match cmd {
                    WalCommand::Append { record, ack } => {
                        if self.failed.load(Ordering::SeqCst) {
                            let _ = ack.send(Err("WAL unavailable after an earlier failure".into()));
                            continue;
                        }
                        match self.write_record(&record) {
                            Ok(()) => pending.push(ack),
                            Err(e) => {
                                error!("❌ WAL write failed: {}", e);
                                self.failed.store(true, Ordering::SeqCst);
                                let _ = ack.send(Err(e.to_string()));
                            }
                        }
                    }
                    WalCommand::Rotate { next_seq, ack } => {
                        let committed = self.commit(&mut pending);
                        let result = committed.and_then(|_| {
                            self.file = open_segment(&self.dir, next_seq).map_err(|e| e.to_string())?;
                            debug!("🔄 WAL rotated to segment {:020}.", next_seq);
                            Ok(next_seq)
                        });
                        if result.is_err() {
                            self.failed.store(true, Ordering::SeqCst);
                        }
                        let _ = ack.send(result);
                    }
                    WalCommand::Shutdown { ack } => {
                        let result = self.commit(&mut pending);
                        let _ = ack.send(result);
                        info!("📓 WAL writer stopped.");
                        return;
                    }
                }
            }

            let _ = self.commit(&mut pending);
        }

        let _ = self.commit(&mut pending);
    }

    fn write_record(&mut self, record: &WalRecord) -> Result<()> {
        let frame = encode_record(&self.cipher, self.compression_level, record)?;
        self.file.write_all(&frame)?;
        Ok(())
    }

    /// Flush (and optionally fsync) the segment, then acknowledge pending records.
    fn commit(
        &mut self,
        pending: &mut Vec<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> std::result::Result<(), String> {
        let result = self
            .file
            .flush()
            .and_then(|_| if self.sync { self.file.get_ref().sync_data() } else { Ok(()) })
            .map_err(|e| e.to_string());

        if let Err(e) = &result {
            error!("❌ WAL sync failed: {}", e);
            self.failed.store(true, Ordering::SeqCst);
        }
        for ack in pending.drain(..) {
            let _ = ack.send(result.clone());
        }
        result
    }
}

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{:020}.{}", seq, SEGMENT_EXTENSION))
}

fn open_segment(dir: &Path, seq: u64) -> Result<BufWriter<File>> {
    let path = segment_path(dir, seq);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open WAL segment {}", path.display()))?;
    sync_dir(dir);
    Ok(BufWriter::with_capacity(1 << 20, file))
}

fn encode_record(cipher: &Aes256Gcm, level: i32, record: &WalRecord) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(record)?;
    let compressed = encode_all(&json[..], level)?;

    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("Failed to generate nonce: {}", e))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), compressed.as_ref())
        .map_err(|e| anyhow!("WAL encryption failed: {}", e))?;

    let len = NONCE_LEN + ciphertext.len();
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&(len as u32).to_be_bytes());
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&ciphertext);
    Ok(frame)
}

/// Decrypt with the current or any previous key (records don't name their key).
fn decode_record(keys: &crate::crypt::KeyRing, payload: &[u8]) -> Result<WalRecord> {
    let compressed = keys.decrypt_any(payload, b"")?;
    let json = decode_all(&compressed[..])?;
    Ok(serde_json::from_slice(&json)?)
}

/// WAL segments in a directory, oldest first, as (first sequence number, path).
pub fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == SEGMENT_EXTENSION) {
            if let Some(seq) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
                segments.push((seq, path));
            }
        }
    }
    segments.sort_by_key(|(seq, _)| *seq);
    Ok(segments)
}

/// Delete WAL segments older than `seq` (whose contents are now in SSTables).
pub fn delete_segments_before(dir: &Path, seq: u64) -> Result<usize> {
    let mut deleted = 0;
    for (segment_seq, path) in list_segments(dir)? {
        if segment_seq < seq {
            fs::remove_file(&path).with_context(|| format!("Failed to delete {}", path.display()))?;
            deleted += 1;
        }
    }
    if deleted > 0 {
        sync_dir(dir);
    }
    Ok(deleted)
}

/// Statistics from a WAL replay.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayStats {
    pub segments: usize,
    pub records: usize,
    pub torn_tails: usize,
    pub corrupt_records: usize,
    pub max_seq: u64,
}

/// Read every record from every segment, oldest first, and pass it to `apply`.
/// A partial record at the end of a segment (from a crash mid-write) ends that
/// segment. Records that fail to decrypt or decode are skipped and counted.
pub fn replay(dir: &Path, keys: &crate::crypt::KeyRing, mut apply: impl FnMut(WalRecord)) -> Result<ReplayStats> {
    let mut stats = ReplayStats::default();
    let segments = list_segments(dir)?;
    let last = segments.len().saturating_sub(1);

    for (index, (_, path)) in segments.iter().enumerate() {
        stats.segments += 1;
        let mut reader = io::BufReader::new(
            File::open(path).with_context(|| format!("Failed to open {}", path.display()))?,
        );

        loop {
            let mut len_buf = [0u8; 4];
            match read_full(&mut reader, &mut len_buf)? {
                ReadOutcome::Eof => break,
                ReadOutcome::Partial => {
                    note_torn_tail(path, index == last, &mut stats);
                    break;
                }
                ReadOutcome::Full => {}
            }

            let len = u32::from_be_bytes(len_buf) as usize;
            if !(NONCE_LEN + 16..=MAX_RECORD_LEN).contains(&len) {
                // The framing is lost; nothing after this point can be trusted.
                note_torn_tail(path, index == last, &mut stats);
                break;
            }

            let mut payload = vec![0u8; len];
            match read_full(&mut reader, &mut payload)? {
                ReadOutcome::Full => {}
                _ => {
                    note_torn_tail(path, index == last, &mut stats);
                    break;
                }
            }

            match decode_record(keys, &payload) {
                Ok(record) => {
                    stats.records += 1;
                    stats.max_seq = stats.max_seq.max(record.last_seq());
                    apply(record);
                }
                Err(e) => {
                    stats.corrupt_records += 1;
                    error!("❌ Skipping unreadable WAL record in {}: {}", path.display(), e);
                }
            }
        }
    }

    Ok(stats)
}

fn note_torn_tail(path: &Path, is_last_segment: bool, stats: &mut ReplayStats) {
    stats.torn_tails += 1;
    if is_last_segment {
        warn!(
            "⚠️ Ignoring an incomplete record at the end of {} (likely a crash during a write).",
            path.display()
        );
    } else {
        error!(
            "❌ Incomplete record in {}, which is not the newest WAL segment. Later records in it are lost.",
            path.display()
        );
    }
}

enum ReadOutcome {
    Full,
    Partial,
    Eof,
}

fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> Result<ReadOutcome> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(if filled == 0 { ReadOutcome::Eof } else { ReadOutcome::Partial }),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(ReadOutcome::Full)
}

/// Find WAL files written by HexDB before sequence-numbered segments existed.
pub fn legacy_wal_files(storage_dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(storage_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".hexdb") && n.ends_with(".dat"))
        })
        .collect()
}

/// fsync a directory so file creations, renames and deletions in it are durable.
/// A no-op on platforms that don't support it.
pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::infer_fields_from_json;
    use serde_json::json;

    fn key() -> crate::crypt::KeyRing {
        let bytes: [u8; 32] = std::array::from_fn(|i| i as u8);
        crate::crypt::KeyRing::new(&bytes, &[])
    }

    fn put(seq: u64) -> WalRecord {
        WalRecord {
            seq,
            op: WalOp::Put(Document {
                id: Ulid::new(),
                tessellation: "t".into(),
                data: infer_fields_from_json(&json!({ "n": seq })),
                ttl: None,
            }),
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hexdb-wal-test-{}", Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn writes_rotates_and_replays_in_order() {
        let dir = temp_dir();
        let wal = WalWriter::start(&dir, &key(), 0, true, 1).unwrap();
        for seq in 1..=3 {
            wal.append(put(seq)).await.unwrap().wait().await.unwrap();
        }
        let new_seg = wal.rotate(4).await.unwrap().await.unwrap().unwrap();
        assert_eq!(new_seg, 4);
        wal.append(put(4)).await.unwrap().wait().await.unwrap();
        wal.shutdown().await.unwrap();

        assert_eq!(list_segments(&dir).unwrap().len(), 2);
        let mut seqs = Vec::new();
        let stats = replay(&dir, &key(), |r| seqs.push(r.seq)).unwrap();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert_eq!(stats.max_seq, 4);

        assert_eq!(delete_segments_before(&dir, 4).unwrap(), 1);
        let mut seqs = Vec::new();
        replay(&dir, &key(), |r| seqs.push(r.seq)).unwrap();
        assert_eq!(seqs, vec![4]);
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn torn_tail_is_ignored() {
        let dir = temp_dir();
        let wal = WalWriter::start(&dir, &key(), 0, true, 1).unwrap();
        wal.append(put(1)).await.unwrap().wait().await.unwrap();
        wal.shutdown().await.unwrap();

        let (_, path) = list_segments(&dir).unwrap().pop().unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0x00, 0x00, 0x10, 0x00, b'a', b'b', b'c']).unwrap();
        drop(f);

        let mut seqs = Vec::new();
        let stats = replay(&dir, &key(), |r| seqs.push(r.seq)).unwrap();
        assert_eq!(seqs, vec![1]);
        assert_eq!(stats.torn_tails, 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_key_records_are_skipped() {
        let dir = temp_dir();
        let frame = encode_record(key().current_cipher(), 0, &put(1)).unwrap();
        fs::write(segment_path(&dir, 1), frame).unwrap();

        let other_key = crate::crypt::KeyRing::new(&[9u8; 32], &[]);
        let stats = replay(&dir, &other_key, |_| panic!("should not decode")).unwrap();
        assert_eq!(stats.corrupt_records, 1);

        // After rotation, records under the previous key still replay.
        let rotated = crate::crypt::KeyRing::new(&[9u8; 32], &[std::array::from_fn(|i| i as u8)]);
        let mut seqs = Vec::new();
        replay(&dir, &rotated, |r| seqs.push(r.seq)).unwrap();
        assert_eq!(seqs, vec![1]);
        fs::remove_dir_all(&dir).ok();
    }
}
