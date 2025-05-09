use std::collections::{HashMap, HashSet};
use blake3; // fast, cryptographic hash
use tracing::{info, warn};

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

    pub fn store_chunk(&mut self, tessellation: &str, doc_id: &str, chunk: Vec<u8>) {
        let hash = blake3::hash(&chunk).to_hex().to_string();
        self.storage.insert((tessellation.to_string(), doc_id.to_string()), (chunk, hash));
    }

    pub fn get_chunk(&self, tessellation: &str, doc_id: &str) -> Option<&(Vec<u8>, String)> {
        self.storage.get(&(tessellation.to_string(), doc_id.to_string()))
    }

    pub fn validate_chunk(&self, tessellation: &str, doc_id: &str) -> bool {
        if let Some((chunk, stored_hash)) = self.get_chunk(tessellation, doc_id) {
            let computed = blake3::hash(chunk).to_hex().to_string();
            &computed == stored_hash
        } else {
            false
        }
    }

    pub fn repair_chunk(&mut self, tessellation: &str, doc_id: &str, new_chunk: Vec<u8>) {
        let new_hash = blake3::hash(&new_chunk).to_hex().to_string();
        self.storage.insert((tessellation.to_string(), doc_id.to_string()), (new_chunk, new_hash));
    }
}

pub struct HexNode {
    pub vertices: [Vertex; 6],
    insert_counter: usize,
    tessellations: std::collections::HashSet<String>,
}

impl HexNode {
    pub fn new() -> Self {
        HexNode {
            vertices: [
                Vertex::new(0),
                Vertex::new(1),
                Vertex::new(2),
                Vertex::new(3),
                Vertex::new(4),
                Vertex::new(5),
            ],
            insert_counter: 0,
            tessellations: HashSet::new(),
        }
    }

    pub fn create_tessellation(&mut self, name: &str) -> bool {
        self.tessellations.insert(name.to_string())
    }

    pub fn drop_tessellation(&mut self, name: &str) -> bool {
        let existed = self.tessellations.remove(name);
        if existed {
            for vertex in self.vertices.iter_mut() {
                vertex.storage.retain(|(tess, _), _| tess != name);
            }
        }
        existed
    }

    pub fn tessellation_exists(&self, name: &str) -> bool {
        self.tessellations.contains(name)
    }

    pub fn insert_document(&mut self, tessellation: &str, doc_id: &str, data: &[u8]) {
        let primary = self.insert_counter % 6;
        self.insert_counter += 1;

        let chunk_size = (data.len() as f32 / 6.0).ceil() as usize;
        let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();

        for (i, vertex) in self.vertices.iter_mut().enumerate() {
            let chunk = if i == primary {
                data.to_vec() // full document on primary
            } else {
                chunks[i % chunks.len()].to_vec() // partial chunk on others
            };
            vertex.store_chunk(tessellation, doc_id, chunk);
        }
    }

    pub fn retrieve_document(&self, tessellation: &str, doc_id: &str) -> Option<Vec<u8>> {
        // Try to find full document first
        for vertex in &self.vertices {
            if let Some((chunk, hash)) = vertex.get_chunk(tessellation, doc_id) {
                if chunk.len() > 1024 && blake3::hash(chunk).to_hex().to_string() == *hash {
                    info!("Retrieved full document from vertex {}", vertex.id);
                    return Some(chunk.clone());
                }
            }
        }

        // Attempt to reconstruct
        let mut combined: Vec<u8> = Vec::new();
        for vertex in &self.vertices {
            if let Some((chunk, hash)) = vertex.get_chunk(tessellation, doc_id) {
                let computed = blake3::hash(chunk).to_hex().to_string();
                if computed == *hash {
                    combined.extend_from_slice(chunk);
                } else {
                    warn!("Corrupt chunk detected on vertex {} for {}:{}", vertex.id, tessellation, doc_id);
                }
            }
        }

        if combined.is_empty() {
            warn!("Document reconstruction failed: no valid chunks found for {}:{}", tessellation, doc_id);
            None
        } else {
            info!("Document reconstructed from partial chunks for {}:{}", tessellation, doc_id);
            Some(combined)
        }
    }

    pub fn validate_vertex_chunks(&self, tessellation: &str, doc_id: &str) -> Vec<(usize, bool)> {
        self.vertices
            .iter()
            .map(|v| (v.id, v.validate_chunk(tessellation, doc_id)))
            .collect()
    }

    pub fn repair_corrupt_chunks(&mut self, tessellation: &str, doc_id: &str) {
        if let Some(reference) = self.retrieve_document(tessellation, doc_id) {
            for vertex in self.vertices.iter_mut() {
                if !vertex.validate_chunk(tessellation, doc_id) {
                    warn!("Repairing corrupt chunk on vertex {} for {}:{}", vertex.id, tessellation, doc_id);
                    vertex.repair_chunk(tessellation, doc_id, reference.clone());
                }
            }
        } else {
            warn!("Repair failed: could not reconstruct document {}:{}", tessellation, doc_id);
        }
    }
} 
