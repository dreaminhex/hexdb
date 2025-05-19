// HexDB Core Hexagonal Storage
// This module implements a hexagonal storage model for HexDB, allowing for
// efficient storage and retrieval of documents in a hexagonal structure.
// Memory is chunked and distributed across six vertices, providing redundancy and
// fault tolerance. The engine supports operations such as inserting, retrieving,
// validating, and repairing documents.

use crate::Vertex;
use blake3;
use std::collections::{HashMap, HashSet};
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
        self.vertices
            .iter()
            .flat_map(|v| v.storage.values())
            .filter(|(_, _, is_full)| *is_full)
            .map(|(chunk, _, _)| chunk.len())
            .sum()
    }

    /// Returns the total number of unique documents stored across all vertices.
    pub fn count_total_documents(&self) -> usize {
        let mut unique_ids = HashSet::new();

        for vertex in &self.vertices {
            for (key, _) in vertex.storage.keys() {
                unique_ids.insert(key.clone()); // (tessellation, doc_id)
            }
        }

        unique_ids.len()
    }

    /// Create a new tessellation with the given name and type.
    /// Returns true if the tessellation was created successfully, false if it already exists.
    pub fn create_tessellation(&mut self, tess_name: &str, tess_type: &str) -> bool {
        if self.tessellations.contains_key(tess_name) {
            return false;
        }
        self.tessellations
            .insert(tess_name.to_string(), tess_type.to_string());
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

        // Determine replica targets (exclude primary)
        let replica_indices: Vec<usize> = (0..6).filter(|&i| i != primary).collect();

        // TODO: filter based on vertex health if available
        let num_replicas = replica_indices.len();

        // Store full document in primary
        self.vertices[primary].store_chunk(tess_name, doc_id, data.to_vec(), true);

        // Split into equal chunks for replicas
        let chunk_size = (data.len() as f32 / num_replicas as f32).ceil() as usize;
        let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();

        for (i, &idx) in replica_indices.iter().enumerate() {
            let chunk = chunks.get(i).copied().unwrap_or(&[]).to_vec();
            self.vertices[idx].store_chunk(tess_name, doc_id, chunk, false);
        }

        // Optional: track which vertex is primary for this document
        // self.primary_map.insert((tess_name.to_string(), doc_id.to_string()), primary);
    }

    /// Delete a document from all vertices.
    /// The document is identified by its tessellation name and document ID.
    /// Returns true if the document was deleted, false if it didn't exist.
    pub fn delete_document(&mut self, tess_name: &str, doc_id: &str) {
        for vertex in self.vertices.iter_mut() {
            vertex
                .storage
                .remove(&(tess_name.to_string(), doc_id.to_string()));
        }
    }

    /// Retrieve a document by its tessellation name and document ID.
    /// Returns Some(data) if a valid document is found or reconstructed, None otherwise.
    pub fn read_document(&self, tess_name: &str, doc_id: &str) -> Option<Vec<u8>> {
        for vertex in &self.vertices {
            if let Some((chunk, hash, is_full)) = vertex.get_chunk(tess_name, doc_id) {
                if *is_full && blake3::hash(chunk).to_hex().to_string() == *hash {
                    info!("✅ Retrieved valid document from vertex {}.", vertex.id);
                    return Some(chunk.clone());
                }
            }
        }

        // Attempt reconstruction from partials if no full found
        let mut combined: Vec<u8> = Vec::new();
        for vertex in &self.vertices {
            if let Some((chunk, hash, _)) = vertex.get_chunk(tess_name, doc_id) {
                let computed = blake3::hash(chunk).to_hex().to_string();
                if computed == *hash {
                    combined.extend_from_slice(chunk);
                } else {
                    warn!(
                        "❗ Corrupt chunk detected on vertex {} for {}:{}",
                        vertex.id, tess_name, doc_id
                    );
                }
            }
        }

        if combined.is_empty() {
            warn!(
                "❌ Document reconstruction failed: no valid chunks found for {}:{}.",
                tess_name, doc_id
            );
            None
        } else {
            info!(
                "🩹 Document reconstructed from partial chunks for {}:{}.",
                tess_name, doc_id
            );
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
                    warn!(
                        "🛠️ Repairing corrupt chunk on vertex {} for {}:{}",
                        vertex.id, tessellation, doc_id
                    );
                    vertex.repair_chunk(tessellation, doc_id, reference.clone());
                }
            }
            true
        } else {
            warn!(
                "❗ Repair failed: could not reconstruct document {}:{}",
                tessellation, doc_id
            );
            false
        }
    }

    /// Returns all full documents stored under the given tessellation.
    /// Each entry is (doc_id, full_data).
    pub fn get_all_docs(&self, tess_name: &str) -> Vec<(String, Vec<u8>)> {
        let mut results = Vec::new();

        for vertex in &self.vertices {
            for ((tess, id), (chunk, _hash, is_full)) in &vertex.storage {
                if tess == tess_name && *is_full {
                    results.push((id.clone(), chunk.clone()));
                }
            }
        }

        results
    }
}
