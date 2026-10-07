// HexDB Core Vertex
// A vertex is one of the six memory regions of a hex. Each document is split
// into six shards (four data, two parity), and each vertex holds one shard of
// every document, along with a BLAKE3 hash used to detect corruption.

use crate::hex::DocKey;
use std::collections::HashMap;

/// One shard of a document, with the hash it had when stored.
pub struct Shard {
    pub bytes: Vec<u8>,
    pub hash: blake3::Hash,
}

impl Shard {
    pub fn new(bytes: Vec<u8>) -> Self {
        let hash = blake3::hash(&bytes);
        Shard { bytes, hash }
    }

    /// True if the shard still matches its stored hash.
    pub fn is_intact(&self) -> bool {
        blake3::hash(&self.bytes) == self.hash
    }
}

/// Represents a vertex in the hexagonal memory model.
pub struct Vertex {
    pub id: usize,
    shards: HashMap<DocKey, Shard>,
    bytes: usize,
    /// Corrupt shards found on this vertex since startup.
    pub corrupt_found: u64,
    /// Shards rebuilt on this vertex since startup.
    pub repaired: u64,
}

impl Vertex {
    pub fn new(id: usize) -> Self {
        Vertex {
            id,
            shards: HashMap::new(),
            bytes: 0,
            corrupt_found: 0,
            repaired: 0,
        }
    }

    /// Store (or replace) a shard.
    pub fn store(&mut self, key: &DocKey, bytes: Vec<u8>) {
        self.bytes += bytes.len();
        if let Some(old) = self.shards.insert(key.clone(), Shard::new(bytes)) {
            self.bytes -= old.bytes.len();
        }
    }

    pub fn get(&self, key: &DocKey) -> Option<&Shard> {
        self.shards.get(key)
    }

    /// Return a copy of the shard if it exists and is intact.
    pub fn intact_copy(&self, key: &DocKey) -> Option<Vec<u8>> {
        self.shards
            .get(key)
            .filter(|s| s.is_intact())
            .map(|s| s.bytes.clone())
    }

    pub fn remove(&mut self, key: &DocKey) {
        if let Some(old) = self.shards.remove(key) {
            self.bytes -= old.bytes.len();
        }
    }

    /// Total shard bytes held by this vertex.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Flip bits in a stored shard without updating its hash. Testing aid only.
    #[doc(hidden)]
    pub fn corrupt_for_testing(&mut self, key: &DocKey) -> bool {
        match self.shards.get_mut(key) {
            Some(shard) if !shard.bytes.is_empty() => {
                shard.bytes[0] ^= 0xFF;
                true
            }
            _ => false,
        }
    }
}
