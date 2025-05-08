use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Document {
    pub id: String,
    pub body: serde_json::Value,
    pub created_at: i64,
    pub ttl_seconds: Option<u64>,
}

impl Document {
    pub fn is_expired(&self, now: i64) -> bool {
        self.ttl_seconds.map_or(false, |ttl| now > self.created_at + ttl as i64)
    }
}
