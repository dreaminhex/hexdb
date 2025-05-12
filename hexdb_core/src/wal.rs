// HexDB Core Write-Ahead Log (WAL) Module
// This module implements a Write-Ahead Log (WAL) for HexDB, allowing for
// asynchronous logging of operations. The WAL is used to ensure durability and
// consistency of data in the event of a crash or failure. The WAL is written
// to a file, and operations are serialized in JSON format. The WAL is designed
// to be efficient and can handle high-throughput workloads. The WAL is also
// designed to be easy to use, with a simple API for writing operations. The WAL
// is implemented using Tokio's asynchronous I/O capabilities, allowing for
// non-blocking writes and efficient use of system resources.

use crate::{document::Document, HexConfig, MemoryEngine};
use serde::{Serialize, Deserialize};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc::Receiver,
};
use ulid::Ulid;
use std::{io::ErrorKind, path::PathBuf, sync::Arc};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Orinoco
use aes_gcm::aead::{Aead, KeyInit};
use getrandom::fill;
use anyhow::Result;
use zstd::stream::{encode_all, decode_all};
use tracing::{info, warn, error};

#[derive(Debug, Serialize, Deserialize)]
pub enum Wal {
    Insert(Document),
    Delete {
        tessellation: String,
        id: String,
    },
    Rotate,
}

pub async fn wal_writer_task(
    config: HexConfig,
    mut rx: Receiver<Wal>,
    wal_path: PathBuf,
    key: Arc<Vec<u8>>,
) -> anyhow::Result<()> {
    use tokio::fs;

    // Ensure parent directory exists
    if let Some(parent) = wal_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut writer = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)
            .await?,
    );

    let current_path = wal_path.clone();

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));

    info!("WAL writer task started (path: {:?})", wal_path);

    while let Some(op) = rx.recv().await {
        // Rotate WAL on command
        if matches!(op, Wal::Rotate) {
            
            writer.flush().await?;
            
            let rotated_path = current_path.with_file_name(format!(".hexdb.{}.dat", Ulid::new()));

            drop(writer);

            fs::rename(&current_path, &rotated_path).await?;

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&current_path)
                .await?;
            writer = BufWriter::new(file);

            info!("🔄 WAL rotated, previous log saved to {:?}.", rotated_path);
            continue;
        }

        // Normal WAL operation (insert/delete)
        let json = match serde_json::to_vec(&op) {
            Ok(data) => data,
            Err(e) => {
                error!("❌ Failed to serialize WAL: {:?}.", e);
                continue;
            }
        };

        let compressed = encode_all(&*json, config.compression.compression_level)?;
        let mut nonce_bytes = [0u8; 12];
        fill(&mut nonce_bytes)?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let encrypted = match cipher.encrypt(nonce, compressed.as_ref()) {
            Ok(enc) => enc,
            Err(e) => {
                error!("❌ WAL encryption failed: {:?}", e);
                continue;
            }
        };

        writer.write_u32((12 + encrypted.len()) as u32).await?;
        writer.write_all(&nonce_bytes).await?;
        writer.write_all(&encrypted).await?;
        writer.flush().await?;
    }

    Ok(())
}

pub async fn recover_from_all_wal_files(
    wal_dir: PathBuf,
    key: &[u8],
    engine: Arc<MemoryEngine>,
    cleanup: bool,
    ) -> Result<()> {
    use std::ffi::OsStr;
    use std::time::SystemTime;

    if !wal_dir.exists() {
        info!("🆕 No WAL directory found at {:?} — Fresh installation.", wal_dir);
        return Ok(());
    }

    let mut wal_files: Vec<_> = std::fs::read_dir(&wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(OsStr::to_str)
                .map(|n| n.starts_with(".hexdb") && n.ends_with(".dat"))
                .unwrap_or(false)
        })
        .collect();

    wal_files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });

    for file in &wal_files {
        info!("🔁 Replaying WAL: {:?}", file);
        recover_single_wal(file.clone(), key, engine.clone()).await?;
    }

    if cleanup {
        for file in wal_files {
            if file.file_name().unwrap() != ".hexdb.dat" {
                match std::fs::remove_file(&file) {
                    Ok(_) => info!("🗑️ Deleted old WAL file: {:?}", file),
                    Err(e) => warn!("❗ Failed to delete WAL file {:?}: {}", file, e),
                }
            }
        }
    }

    Ok(())
}

async fn recover_single_wal(
    wal_path: PathBuf,
    key: &[u8],
    engine: Arc<MemoryEngine>,
) -> Result<()> {

    let file = match File::open(&wal_path).await {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            info!("🆕 No WAL file found at {:?} — assuming first run.", wal_path);
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!("❌ Failed to open WAL file: {}", e));
        }
    };

    let mut reader = BufReader::new(file);

    loop {
        let len_result = reader.read_u32().await;

        let size = match len_result {
            Ok(sz) => sz as usize,
            Err(ref e) if e.kind() == ErrorKind::UnexpectedEof => break, // done
            Err(e) => return Err(anyhow::anyhow!("❌ Failed to read WAL record length: {}", e)),
        };

        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf).await?;

        if buf.len() <= 12 {
            error!("❌ WAL record too short, skipping.");
            continue;
        }

        let (nonce_bytes, encrypted_data) = buf.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));

        let decrypted = match cipher.decrypt(nonce, encrypted_data) {
            Ok(data) => data,
            Err(e) => {
                error!("❌ Data decryption failed: {}", e);
                continue;
            }
        };

        let decompressed = match decode_all(&*decrypted) {
            Ok(data) => data,
            Err(e) => {
                error!("❌ WAL decompression failed: {:?}", e);
                continue;
            }
        };

        let op: Wal = match serde_json::from_slice(&decompressed) {
            Ok(op) => op,
            Err(e) => {
                error!("❌ WAL JSON decode failed: {}", e);
                continue;
            }
        };

        match op {
            Wal::Insert(doc) => {
                info!("🔁 Replaying INSERT for {}", doc.id);
                let data = serde_json::to_vec(&doc)?;
                let mut hex = engine.node.lock().await;
                hex.insert_document(&doc.tessellation, &doc.id.to_string(), &data);
                drop(hex);
                engine.track_doc_size(&doc);
                engine.store.insert(doc.id.to_string(), doc);
            }
            Wal::Delete { tessellation, id } => {
                info!("🔁 Replaying DELETE for {}:{}", tessellation, id);
                let mut hex = engine.node.lock().await;
                hex.delete_document(&tessellation, &id);
                drop(hex);
                engine.store.remove(&id);
            }
            Wal::Rotate => {
                info!("🔁 Skipping WAL Rotate marker (not replayed).");
            }
        }
    }

    info!("✅ Data recovery complete.");
    Ok(())
}
