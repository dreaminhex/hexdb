// HexDB Core Compression
//
// zstd for document bodies (SSTables), WAL records and index snapshots, one
// small buffer at a time. Creating a zstd context costs far more than
// compressing a 1 KB document, so each thread keeps its contexts and reuses
// them. Frames record their content size, so decompression allocates once;
// frames from earlier versions (written by the streaming encoder, without a
// size) are still read.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

/// Largest content size trusted from a frame header when allocating.
const MAX_FRAME_CONTENT: u64 = 1 << 30;

thread_local! {
    static COMPRESSORS: RefCell<HashMap<i32, zstd::bulk::Compressor<'static>>> = RefCell::new(HashMap::new());
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// Compress `data` at `level` (0: zstd's default).
pub fn compress(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    COMPRESSORS.with(|cell| {
        let mut map = cell.borrow_mut();
        let compressor = match map.entry(level) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(zstd::bulk::Compressor::new(level)?),
        };
        compressor.compress(data)
    })
}

/// Decompress one zstd frame.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let size = match zstd::zstd_safe::get_frame_content_size(data) {
        Ok(Some(size)) if size <= MAX_FRAME_CONTENT => size as usize,
        // No recorded size (an earlier version's frame) or an implausible one.
        _ => return zstd::stream::decode_all(data),
    };
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        slot.as_mut().map(|d| d.decompress(data, size)).unwrap_or_else(|| zstd::stream::decode_all(data))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_reads_streamed_frames() {
        for data in [Vec::new(), b"hello".to_vec(), vec![7u8; 100_000]] {
            let packed = compress(&data, 0).unwrap();
            assert_eq!(decompress(&packed).unwrap(), data);
            // A frame from the streaming encoder (no content size) still reads.
            let streamed = zstd::stream::encode_all(&data[..], 3).unwrap();
            assert_eq!(decompress(&streamed).unwrap(), data);
            // And streaming readers read the new frames.
            assert_eq!(zstd::stream::decode_all(&packed[..]).unwrap(), data);
        }
    }
}
