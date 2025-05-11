// HexDB Core Write-Ahead Log (WAL) Module
// This module implements a Write-Ahead Log (WAL) for HexDB, allowing for
// asynchronous logging of operations. The WAL is used to ensure durability and
// consistency of data in the event of a crash or failure. The WAL is written
// to a file, and operations are serialized in JSON format. The WAL is designed
// to be efficient and can handle high-throughput workloads. The WAL is also
// designed to be easy to use, with a simple API for writing operations. The WAL
// is implemented using Tokio's asynchronous I/O capabilities, allowing for
// non-blocking writes and efficient use of system resources.

use crate::{document::Document, MemoryEngine};
use serde::{Serialize, Deserialize};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc::Receiver,
};
use std::{io::ErrorKind, path::PathBuf, sync::Arc};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Orinoco
use aes_gcm::aead::{Aead, KeyInit};
use getrandom::fill;
use anyhow::Result;
use zstd::stream::{encode_all, decode_all};
use tracing::{info, error};

#[derive(Debug, Serialize, Deserialize)]
pub enum Wal {
    Insert(Document),
    Delete {
        tessellation: String,
        id: String,
    },
}

pub async fn wal_writer_task(
    mut rx: Receiver<Wal>,
    wal_path: PathBuf,
    key: Arc<Vec<u8>>,
) -> anyhow::Result<()> {

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&wal_path)
        .await
        .map_err(|e| anyhow::anyhow!("❌ Failed to open WAL log at {:?}: {}", wal_path, e))?;

    let mut writer = BufWriter::new(file);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));

    while let Some(op) = rx.recv().await {
        let json = match serde_json::to_vec(&op) {
            Ok(data) => data,
            Err(e) => {
                error!("❌ Failed to serialize WAL op: {:?}", e);
                continue;
            }
        };

        let compressed = encode_all(&*json, 0)?; // zstd compress
        let mut nonce_bytes = [0u8; 12];
        fill(&mut nonce_bytes)?; // secure nonce

        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = match cipher.encrypt(nonce, compressed.as_ref()) {
            Ok(enc) => enc,
            Err(e) => {
                error!("❌ WAL encryption failed: {:?}", e);
                continue;
            }
        };

        let mut record = Vec::with_capacity(4 + 12 + encrypted.len());
        record.extend_from_slice(&nonce_bytes);
        record.extend_from_slice(&encrypted);

        // Prefix with length
        writer.write_u32(record.len() as u32).await?;
        writer.write_all(&record).await?;

        if let Err(e) = writer.flush().await {
            error!("❌ WAL flush failed: {:?}", e);
        }
    }

    Ok(())
}


pub async fn recover_from_wal(
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
                engine.store.insert(doc.id.to_string(), doc);
            }
            Wal::Delete { tessellation, id } => {
                info!("🔁 Replaying DELETE for {}:{}", tessellation, id);
                let mut hex = engine.node.lock().await;
                hex.delete_document(&tessellation, &id);
                drop(hex);
                engine.store.remove(&id);
            }
        }
    }

    info!("✅ Data recovery complete.");
    Ok(())
}
