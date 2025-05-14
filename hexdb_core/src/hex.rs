// HexDB Core Hexagonal Storage
// This module implements a hexagonal storage model for HexDB, allowing for
// efficient storage and retrieval of documents in a hexagonal structure.
// Memory is chunked and distributed across six vertices, providing redundancy and
// fault tolerance. The engine supports operations such as inserting, retrieving,
// validating, and repairing documents.

use std::collections::{HashMap};
use crate::Vertex;
use blake3;
use tracing::{info, warn};

pub struct Hex {
    pub vertices: [Vertex; 6],
    pub total_document_count: usize,
    pub tessellations: std::collections::HashMap<String, String>,
}

impl Hex {
    /// Creates a new Hex instance with six vertices.
    /// Each vertex is initialized with a unique ID.
    pub fn new() -> Self {
        Hex {
            vertices: [
                Vertex::new(0),
                Vertex::new(1),
                Vertex::new(2),
                Vertex::new(3),
                Vertex::new(4),
                Vertex::new(5),
            ],
            total_document_count: 0,
            tessellations: HashMap::new(),
        }
    }

    /// Returns the total number of bytes of documents stored across all vertices.
    /// This includes both full documents and partial chunks.
    pub fn calculate_total_ram_bytes(&self) -> usize {
        self.vertices.iter()
            .flat_map(|v| v.storage.values())
            .map(|(chunk, _hash)| chunk.len())
            .sum()
    }

    /// Returns the total number of unique documents stored across all vertices.
    pub fn count_total_documents(&self) -> usize {
        use std::collections::HashSet;

        let mut seen = HashSet::new();

        for vertex in &self.vertices {
            for (key, _) in vertex.storage.iter() {
                seen.insert(key.clone()); // (tessellation, id)
            }
        }

        seen.len()
    }

    /// Create a new tessellation with the given name and type.
    /// Returns true if the tessellation was created successfully, false if it already exists.
    pub fn create_tessellation(&mut self, tess_name: &str, tess_type: &str) -> bool {
        if self.tessellations.contains_key(tess_name) {
            return false;
        }
        self.tessellations.insert(tess_name.to_string(), tess_type.to_string());
        true
    }

    /// Get the tessellation by its name.
    /// Returns the tessellation if it exists, None otherwise.
    pub fn get_tessellation_type(&self, tess_name: &str) -> Option<&str> {
        self.tessellations.get(tess_name).map(String::as_str)
    }

    /// Delete a tessellation by its name. This will also remove all associated documents from the vertices.
    /// Returns true if the tessellation existed and was removed, false otherwise.
    pub fn delete_tessellation(&mut self, tess_name: &str) -> bool {
        let tess = self.tessellations.remove(tess_name);
        let existed = tess.is_some();
        if existed {
            for vertex in self.vertices.iter_mut() {
                vertex.storage.retain(|(tess, _), _| tess != tess_name);
            }
        }
        existed
    }

    /// Check if a tessellation exists by its name.
    /// Returns true if the tessellation exists, false otherwise.
    pub fn tessellation_exists(&self, tess_name: &str) -> bool {
        self.tessellations.contains_key(tess_name)
    }

    /// Insert a document into the tessellation.
    /// The document is split into chunks and distributed across the vertices.
    /// The primary vertex stores the full document, while others store partial chunks.
    /// Returns the ID of the inserted document.
    pub fn create_document(&mut self, tess_name: &str, doc_id: &str, data: &[u8]) {
        let primary = self.total_document_count % 6;
        self.total_document_count += 1;

        let chunk_size = (data.len() as f32 / 6.0).ceil() as usize;
        let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();

        for (i, vertex) in self.vertices.iter_mut().enumerate() {
            let chunk = if i == primary {
                data.to_vec() // full document on primary
            } else {
                chunks[i % chunks.len()].to_vec() // partial chunk on others
            };
            vertex.store_chunk(tess_name, doc_id, chunk);
        }
    }

    /// Delete a document from all vertices.
    /// The document is identified by its tessellation name and document ID.
    /// Returns true if the document was deleted, false if it didn't exist.
    pub fn delete_document(&mut self, tess_name: &str, doc_id: &str) {
        for vertex in self.vertices.iter_mut() {
        vertex.storage.remove(&(tess_name.to_string(), doc_id.to_string()));
        }
    }

    /// Retrieve a document by its tessellation name and document ID.
    /// If the full document is not found, it attempts to reconstruct it from partial chunks.
    /// Returns Some(data) if the document was found or reconstructed, None otherwise.
    pub fn read_document(&self, tess_name: &str, doc_id: &str) -> Option<Vec<u8>> {
        // Try to find full document first
        for vertex in &self.vertices {
            if let Some((chunk, hash)) = vertex.get_chunk(tess_name, doc_id) {
                if chunk.len() > 1024 && blake3::hash(chunk).to_hex().to_string() == *hash {
                    info!("✅ Retrieved full document from vertex {}.", vertex.id);
                    return Some(chunk.clone());
                }
            }
        }

        // Attempt to reconstruct
        let mut combined: Vec<u8> = Vec::new();
        for vertex in &self.vertices {
            if let Some((chunk, hash)) = vertex.get_chunk(tess_name, doc_id) {
                let computed = blake3::hash(chunk).to_hex().to_string();
                if computed == *hash {
                    combined.extend_from_slice(chunk);
                } else {
                    warn!("❗ Corrupt chunk detected on vertex {} for {}:{}", vertex.id, tess_name, doc_id);
                }
            }
        }

        if combined.is_empty() {
            warn!("❌ Document reconstruction failed: no valid chunks found for {}:{}.", tess_name, doc_id);
            None
        } else {
            info!("🩹 Document reconstructed from partial chunks for {}:{}.", tess_name, doc_id);
            Some(combined)
        }
    }

    // Validate the integrity of all chunks for a given tessellation and document ID.
    // Returns a vector of tuples containing the vertex ID and a boolean indicating validity.
    pub fn validate_vertex_chunks(&self, tessellation: &str, doc_id: &str) -> Vec<(usize, bool)> {
        self.vertices
            .iter()
            .map(|v| (v.id, v.validate_chunk(tessellation, doc_id)))
            .collect()
    }

    /// Repair corrupt chunks by reconstructing them from valid chunks.
    /// If a valid chunk is found, it replaces the corrupt chunk with the valid one.
    /// Returns true if the repair was successful, false otherwise.
    pub fn repair_corrupt_chunks(&mut self, tessellation: &str, doc_id: &str) -> bool {
        if let Some(reference) = self.read_document(tessellation, doc_id) {
            for vertex in self.vertices.iter_mut() {
                if !vertex.validate_chunk(tessellation, doc_id) {
                    warn!("🛠️ Repairing corrupt chunk on vertex {} for {}:{}", vertex.id, tessellation, doc_id);
                    vertex.repair_chunk(tessellation, doc_id, reference.clone());                    
                }                
            }
            true
        } else {
            warn!("❗ Repair failed: could not reconstruct document {}:{}", tessellation, doc_id);
            false
        }
    }

    /// Returns all full documents stored under the given tessellation.
    /// Each entry is (doc_id, full_data).
    pub fn get_all_docs(&self, tess_name: &str) -> Vec<(String, Vec<u8>)> {
        let mut results = Vec::new();

        for vertex in &self.vertices {
            for ((tess, id), (chunk, _hash)) in &vertex.storage {
                if tess == tess_name && chunk.len() > 1024 {
                    results.push((id.clone(), chunk.clone()));
                }
            }
        }

        results
    }

} 
