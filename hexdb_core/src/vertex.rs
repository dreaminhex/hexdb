use std::collections::HashMap;
use blake3; // fast, cryptographic hash

pub struct Vertex {
    pub id: usize,
    pub storage: HashMap<(String, String), (Vec<u8>, String)>, // (tessellation, doc_id) -> (chunk, hash)
}

impl Vertex {
    pub fn new(id: usize) -> Self {
        Vertex {
            id,
            storage: HashMap::new(),
        }
    }

    /// Store a hashed chunk of data within the vertex.
    pub fn store_chunk(&mut self, tessellation: &str, doc_id: &str, chunk: Vec<u8>) -> bool {
        let hash = blake3::hash(&chunk).to_hex().to_string();
        self.storage.insert((tessellation.to_string(), doc_id.to_string()), (chunk, hash));
        true
    }

    /// Retrieve a chunk of data from the vertex.
    pub fn get_chunk(&self, tessellation: &str, doc_id: &str) -> Option<&(Vec<u8>, String)> {
        self.storage.get(&(tessellation.to_string(), doc_id.to_string()))
    }

    /// Validate the integrity of a chunk by comparing its hash.
    pub fn validate_chunk(&self, tessellation: &str, doc_id: &str) -> bool {
        if let Some((chunk, stored_hash)) = self.get_chunk(tessellation, doc_id) {
            let computed = blake3::hash(chunk).to_hex().to_string();
            &computed == stored_hash
        } else {
            false
        }
    }

    /// Repair a chunk by replacing it with a new one.
    pub fn repair_chunk(&mut self, tessellation: &str, doc_id: &str, new_chunk: Vec<u8>) -> bool {
        let new_hash = blake3::hash(&new_chunk).to_hex().to_string();
        self.storage.insert((tessellation.to_string(), doc_id.to_string()), (new_chunk, new_hash));
        true
    }
}