// HexDB Core SSTable Module
// This module implements the SSTable (Sorted String Table) format for HexDB.
// The SSTable format is used for storing large amounts of data in a compact
// and efficient manner. The module provides functions for writing and reading
// SSTable files, as well as for compressing and decompressing data using
// Zstandard (zstd) compression. The SSTable format is designed to be fast and
// efficient, allowing for quick access to data while minimizing disk space usage.

use crate::document::Document;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use serde_json;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use ulid::Ulid;
use zstd::stream::{decode_all, encode_all};

const MAGIC: &[u8; 4] = b"HXDB";
const VERSION: u16 = 1;
const COMPRESSION_ZSTD: u8 = 1;

#[derive(Debug)]
pub struct SstEntry {
    pub id: Ulid,
    pub ttl: Option<i64>,
    pub data: Vec<u8>,
}

pub struct SstWriter;

impl SstWriter {
    pub fn write<P: AsRef<Path>>(path: P, entries: Vec<(Ulid, Document)>) -> io::Result<()> {
        let mut file = BufWriter::new(File::create(path)?);
        let created = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;

        let mut index = Vec::new();
        let mut entry_buf = Vec::new();
        let mut offset = 64u64;

        for (i, (id, doc)) in entries.iter().enumerate() {
            let mut flags = 0u8;
            if doc.ttl.is_some() {
                flags |= 0b00000001;
            }

            let raw_json = serde_json::to_vec(doc)?;
            let compressed = encode_all(&raw_json[..], 0)?;
            let length = compressed.len() as u32;

            entry_buf.write_all(&id.to_bytes())?;
            entry_buf.write_u8(flags)?;
            if let Some(ttl_ms) = doc.ttl {
                entry_buf.write_i64::<BigEndian>(ttl_ms)?;
            }
            entry_buf.write_u32::<BigEndian>(length)?;
            entry_buf.write_all(&compressed)?;

            if i % 100 == 0 {
                index.push((*id, offset, (16 + 1 + if doc.ttl.is_some() { 8 } else { 0 } + 4 + compressed.len()) as u32));
            }

            offset += (16 + 1 + if doc.ttl.is_some() { 8 } else { 0 } + 4 + compressed.len()) as u64;
        }

        file.seek(SeekFrom::Start(64))?;
        file.write_all(&entry_buf)?;

        let index_offset = file.stream_position()?;
        for (ulid, off, len) in &index {
            file.write_all(&ulid.to_bytes())?;
            file.write_u64::<BigEndian>(*off)?;
            file.write_u32::<BigEndian>(*len)?;
        }
        let index_size = file.stream_position()? - index_offset;

        file.seek(SeekFrom::Start(0))?;
        file.write_all(MAGIC)?;
        file.write_u16::<BigEndian>(VERSION)?;
        file.write_u8(COMPRESSION_ZSTD)?;
        file.write_u8(0)?; // reserved
        file.write_u64::<BigEndian>(entries.len() as u64)?;
        file.write_i64::<BigEndian>(created)?;
        file.write_u64::<BigEndian>(index_offset)?;
        file.write_u64::<BigEndian>(index_size as u64)?;
        file.write_u64::<BigEndian>(0)?; // checksum placeholder
        file.write_all(&[0u8; 16])?;

        Ok(())
    }
}

pub struct SstReader;

impl SstReader {
    pub fn load_all<P: AsRef<Path>>(path: P) -> io::Result<BTreeMap<Ulid, Document>> {
        let mut file = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid SSTable magic header."));
        }

        file.seek(SeekFrom::Start(8))?;
        let _count = file.read_u64::<BigEndian>()?;
        file.seek(SeekFrom::Start(16))?;
        let _created = file.read_i64::<BigEndian>()?;
        file.seek(SeekFrom::Start(24))?;
        let index_offset = file.read_u64::<BigEndian>()?;

        file.seek(SeekFrom::Start(index_offset))?;
        let mut offsets = Vec::new();
        while let Ok(id_bytes) = {
            let mut b = [0u8; 16];
            file.read_exact(&mut b).map(|_| b)
        } {
            let id = Ulid::from(id_bytes);
            let off = file.read_u64::<BigEndian>()?;
            let len = file.read_u32::<BigEndian>()?;
            offsets.push((id, off, len));
        }

        let mut map = BTreeMap::new();
        for (id, offset, _len) in offsets {
            file.seek(SeekFrom::Start(offset))?;

            let mut id_buf = [0u8; 16];
            file.read_exact(&mut id_buf)?;
            let flags = file.read_u8()?;
            let has_ttl = flags & 0b00000001 != 0;
            let ttl = if has_ttl {
                Some(file.read_i64::<BigEndian>()?)
            } else {
                None
            };
            let clen = file.read_u32::<BigEndian>()?;
            let mut comp = vec![0u8; clen as usize];
            file.read_exact(&mut comp)?;
            let json_bytes = decode_all(&comp[..])?;
            let mut doc: Document = serde_json::from_slice(&json_bytes)?;
            doc.ttl = ttl;
            map.insert(id, doc);
        }

        Ok(map)
    }
}
