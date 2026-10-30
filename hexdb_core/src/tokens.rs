// HexDB Core Tokenization
//
// A field whose schema rule says `"tokenize": "one_way"` or `"vault"` never
// holds its real value: writes replace it with a token, before the document
// reaches the WAL, memory, SSTables, replicas or backups.
//
//   tok_<26 characters>_1111
//
// The token is a keyed BLAKE3 hash of the value under a key derived from the
// lattice key, in lowercase base32, followed by the value's last 4 characters
// when it has at least 10 (PCI DSS allows showing the last 4 of a card
// number). Equal values give equal tokens across the lattice, so equality
// filters, unique indexes and joins work: filters on a tokenized field are
// rewritten to compare tokens (see `HexDBEngine::prepare_filter`). Ranges,
// prefixes and full-text search can't see the value.
//
// One-way tokens can't be turned back into the value by anyone. Vault tokens
// can: the value is kept, sealed with the lattice keys and bound to its
// token, in the `_vault` system tessellation (written in the same commit),
// and users with the `detokenize` permission on the tessellation can read it
// back (`HexDBEngine::detokenize`, audited). There's one entry per token and
// tessellation; entries no document holds the token of any more are deleted
// with compaction (`HexDBEngine::collect_vault_garbage`).

use crate::engine::writes::BatchItem;
use crate::hex::DocKey;
use crate::schema::Tokenize;
use crate::wal::WalOp;
use crate::HexDBEngine;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tracing::info;
use ulid::Ulid;
use zeroize::Zeroizing;

/// System tessellation holding vault tokens' values.
pub const VAULT_TESSELLATION: &str = "_vault";
const PREFIX: &str = "tok_";
const HASH_CHARS: usize = 26;
/// Values this long or longer keep their last 4 characters in the token.
const SHOW_LAST_FROM: usize = 10;
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// Documents read at a time by the vault sweep.
const PAGE: usize = 1000;

/// The lattice's tokenization key.
pub struct TokenKey(Zeroizing<[u8; 32]>);

impl TokenKey {
    pub fn new(lattice_key: &[u8; 32]) -> TokenKey {
        TokenKey(Zeroizing::new(blake3::derive_key("HexDB 2026 tokenization v1", lattice_key)))
    }

    /// The token for a value.
    pub fn token(&self, value: &str) -> String {
        let hash = blake3::keyed_hash(&self.0, value.as_bytes());
        let mut out = String::with_capacity(PREFIX.len() + HASH_CHARS + 5);
        out.push_str(PREFIX);
        // 5 bits per character, from the hash's bytes.
        let (mut acc, mut bits) = (0u32, 0u32);
        for &byte in hash.as_bytes() {
            acc = (acc << 8) | byte as u32;
            bits += 8;
            while bits >= 5 && out.len() < PREFIX.len() + HASH_CHARS {
                bits -= 5;
                out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
            }
        }
        let chars: Vec<char> = value.chars().collect();
        if chars.len() >= SHOW_LAST_FROM {
            out.push('_');
            out.extend(&chars[chars.len() - 4..]);
        }
        out
    }
}

/// True for a string shaped like a token (a value written back as read).
pub fn is_token(value: &str) -> bool {
    let Some((hash, tail)) = value.strip_prefix(PREFIX).and_then(|rest| rest.split_at_checked(HASH_CHARS)) else { return false };
    let tail_ok = tail.is_empty() || tail.strip_prefix('_').is_some_and(|last| last.chars().count() == 4);
    tail_ok && hash.bytes().all(|b| ALPHABET.contains(&b))
}

/// The vault document ID for a token in a tessellation. Equal values give
/// equal tokens across the lattice, so each tessellation has its own entry.
fn vault_id(tess: &str, token: &str) -> Ulid {
    let hash = blake3::hash(format!("{}\n{}", tess, token).as_bytes());
    Ulid::from(u128::from_be_bytes(hash.as_bytes()[..16].try_into().expect("16 bytes")))
}

fn aad(token: &str) -> String {
    format!("hexdb-vault:{}", token)
}

/// A vault entry a write needs.
pub(crate) enum VaultEntry {
    /// A value tokenized by this write: its entry, (ID, document data).
    New(Ulid, Value),
    /// A vault token written back as read: its entry is rewritten unchanged,
    /// so a vault sweep that saw the entry before this write can't delete it.
    Existing(Ulid),
}

impl HexDBEngine {
    /// The tokenized fields of a tessellation's current schema: (path, kind).
    pub(crate) fn tokenized_fields(&self, tess: &str) -> Vec<(String, Tokenize)> {
        self.schemas(tess)
            .last()
            .map(|v| v.fields.iter().filter_map(|(path, rule)| rule.tokenize.map(|t| (path.clone(), t))).collect())
            .unwrap_or_default()
    }

    /// Replace a document's tokenized values with tokens. Returns the vault
    /// entries to write with it.
    pub(crate) fn tokenize_json(&self, tess: &str, json: &mut Value, fields: &[(String, Tokenize)]) -> Vec<VaultEntry> {
        let mut vault = Vec::new();
        for (path, kind) in fields {
            let Some(slot) = path.split('.').try_fold(&mut *json, |current, part| current.get_mut(part)) else { continue };
            let Value::String(value) = slot else { continue };
            if is_token(value) {
                if *kind == Tokenize::Vault {
                    vault.push(VaultEntry::Existing(vault_id(tess, value)));
                }
                continue;
            }
            let plain = Zeroizing::new(std::mem::take(value));
            let token = self.token_key.token(&plain);
            if *kind == Tokenize::Vault {
                let sealed = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, self.lattice_keys.seal(plain.as_bytes(), aad(&token).as_bytes()));
                vault.push(VaultEntry::New(vault_id(tess, &token), json!({ "token": token, "tessellation": tess, "field": path, "value": sealed })));
            }
            *slot = Value::String(token);
        }
        vault
    }

    /// The token a filter value on a tokenized field compares with.
    pub(crate) fn token_for_filter(&self, value: &Value) -> Option<Value> {
        match value {
            Value::String(s) if !is_token(s) => Some(Value::String(self.token_key.token(s))),
            _ => None,
        }
    }

    /// The values behind vault tokens of `tess` (None for tokens that aren't
    /// vault tokens of that tessellation). The caller checks permissions.
    pub async fn detokenize(&self, tess: &str, tokens: &[String]) -> Result<Vec<Option<String>>> {
        let mut out = Vec::with_capacity(tokens.len());
        for token in tokens {
            let value = match is_token(token) {
                false => None,
                true => match self.get_system_document(VAULT_TESSELLATION, vault_id(tess, token)).await? {
                    None => None,
                    Some(doc) => {
                        let data = doc.data_json();
                        if data["token"] != *token || data["tessellation"] != tess {
                            None
                        } else {
                            data["value"]
                                .as_str()
                                .and_then(|b| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b).ok())
                                .and_then(|sealed| self.lattice_keys.open(&sealed, aad(token).as_bytes()))
                                .and_then(|plain| String::from_utf8(plain).ok())
                        }
                    }
                },
            };
            out.push(value);
        }
        Ok(out)
    }

    /// Delete vault entries whose token no document of their tessellation
    /// holds any more (deleted documents, changed values, dropped
    /// tessellations). On the Overseer; returns how many were deleted.
    pub async fn collect_vault_garbage(&self) -> Result<usize> {
        if !self.is_writable() || !self.tessellation_exists(VAULT_TESSELLATION) {
            return Ok(0);
        }
        // Entries and their versions before the documents: a write after this
        // point that uses a token rewrites its entry, and the delete below,
        // which expects the version read here, leaves it alone.
        let mut entries: HashMap<String, Vec<(Ulid, String, u64)>> = HashMap::new();
        let mut after = None;
        loop {
            let ids = self.visible_ids_after(VAULT_TESSELLATION, after, PAGE).await;
            for &id in &ids {
                let (Some(doc), seq) = self.read_latest(&DocKey::new(VAULT_TESSELLATION, id)).await? else { continue };
                let data = doc.data_json();
                if let (Some(token), Some(tess)) = (data["token"].as_str(), data["tessellation"].as_str()) {
                    entries.entry(tess.to_string()).or_default().push((id, token.to_string(), seq));
                }
            }
            match ids.last() {
                Some(&last) if ids.len() == PAGE => after = Some(last),
                _ => break,
            }
        }
        let mut deleted = 0;
        for (tess, entries) in entries {
            let in_use = self.tokens_in_use(&tess).await?;
            for (id, token, seq) in entries {
                if in_use.contains(&token) {
                    continue;
                }
                let op = WalOp::Delete { tessellation: VAULT_TESSELLATION.into(), id };
                let item = BatchItem { key: DocKey::new(VAULT_TESSELLATION, id), op, expected_seq: Some(seq) };
                if self.commit(vec![item]).await? {
                    deleted += 1;
                }
            }
        }
        if deleted > 0 {
            info!("🧹 Vault: deleted {} token(s) no document uses any more.", deleted);
        }
        Ok(deleted)
    }

    /// Every token held anywhere in a tessellation's documents (whatever its
    /// schema says now: a field that stopped being tokenized keeps its tokens).
    async fn tokens_in_use(&self, tess: &str) -> Result<HashSet<String>> {
        fn collect(value: &Value, out: &mut HashSet<String>) {
            match value {
                Value::String(s) if is_token(s) => {
                    out.insert(s.clone());
                }
                Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
                Value::Object(map) => map.values().for_each(|v| collect(v, out)),
                _ => {}
            }
        }
        let mut out = HashSet::new();
        if !self.tessellation_exists(tess) {
            return Ok(out);
        }
        let mut after = None;
        loop {
            let page = self.list_documents(tess, after, PAGE).await?;
            for doc in &page.documents {
                collect(&doc.data_json(), &mut out);
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_stable_opaque_and_keep_the_last_four() {
        let key = TokenKey::new(&[3u8; 32]);
        let card = key.token("4111111111111111");
        assert!(card.starts_with("tok_") && card.ends_with("_1111"), "{}", card);
        assert_eq!(card.len(), 4 + 26 + 5);
        assert_eq!(card, key.token("4111111111111111"), "equal values, equal tokens");
        assert_ne!(card, key.token("4111111111111112"));
        assert_ne!(card, TokenKey::new(&[4u8; 32]).token("4111111111111111"), "another lattice, other tokens");
        let short = key.token("123-45");
        assert_eq!(short.len(), 30, "short values show nothing: {}", short);
        assert!(is_token(&card) && is_token(&short));
        assert!(!is_token("tok_short") && !is_token("4111111111111111") && !is_token(&format!("{}x", card)));
    }
}
