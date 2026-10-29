// HexDB Core Sealed Fields
//
// A field that any role masks (see `crate::access`) holds sensitive data, such
// as card or account numbers, that most callers only ever see masked. Its
// real value is kept out of the hex's long-lived memory: wherever documents
// stay in memory (the hex's vertices, which hold unflushed and recently read
// documents, the change feed's history, WAL records being written) the value
// is sealed, and it's opened only while a request uses it, into buffers that
// are wiped afterwards. A process memory dump then shows ciphertext for these
// fields, not values.
//
// A sealed value is `{"$hexdb_sealed": "v1.<key id>.<base64 nonce+ciphertext>"}`:
// AES-256-GCM under a key derived from the lattice key (every hex holds it, so
// sealed values replicate as they are), with the tessellation, document ID and
// field path as associated data, so a value can't be moved to another
// document or field. Sealed values are written to the WAL and SSTables as
// they are (on top of the storage encryption).
//
// Reading a stored document (`Document`'s deserializer) opens its sealed
// values, so filters, sorting, aggregation, SQL and the API see the real
// values as before, and masks then apply per caller. Such documents are never
// put in the parsed-document cache. Indexes hold a keyed token for a sealed
// value instead of the value (unique indexes still work); the query planner
// doesn't use those indexes, so filters and sorts on sealed fields scan, and
// text indexes leave them out.
//
// Which fields are sealed follows the roles: the masked paths of every role,
// per tessellation (`*` restrictions: every user tessellation). When roles
// change, the hex reseals what it holds (`HexDBEngine::refresh_seal_policy`).
// Values already sealed stay readable after their mask is removed, and are
// written back as plain values with the document's next write.

use crate::{auth::RoleDefinitions, document::Document};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

/// The key of a sealed value's envelope object.
pub const MARK: &str = "$hexdb_sealed";
const FORMAT: &str = "v1";

struct FieldKey {
    id: String,
    key: Zeroizing<[u8; 32]>,
}

/// Every field key this process has seen (current and previous lattice keys
/// of every engine), so any stored document can be opened.
static KEYS: RwLock<Vec<Arc<FieldKey>>> = RwLock::new(Vec::new());

fn derive(lattice_key: &[u8; 32]) -> FieldKey {
    let key = Zeroizing::new(blake3::derive_key("HexDB 2026 sealed field v1", lattice_key));
    let id = blake3::hash(&*key).to_hex()[..8].to_string();
    FieldKey { id, key }
}

/// Seals values for one engine, with its current lattice key.
pub struct Sealer {
    current: Arc<FieldKey>,
    tokens: Tokenizer,
}

/// Keyed tokens for sealed values (equal values, equal tokens), for indexes.
#[derive(Clone)]
pub struct Tokenizer(Arc<Zeroizing<[u8; 32]>>);

impl Tokenizer {
    pub fn token(&self, value: &Value) -> String {
        let plain = Zeroizing::new(serde_json::to_vec(value).unwrap_or_default());
        format!("sealed:{}", &blake3::keyed_hash(&self.0, &plain).to_hex()[..32])
    }
}

impl std::fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokenizer")
    }
}

impl Sealer {
    /// A sealer for these lattice keys (current first). Registers all of them
    /// so values sealed under any can be opened.
    pub fn new(lattice_keys: &[[u8; 32]]) -> Sealer {
        let mut registry = KEYS.write().unwrap_or_else(|e| e.into_inner());
        let mut current = None;
        for lattice_key in lattice_keys {
            let derived = derive(lattice_key);
            let found = registry.iter().find(|k| k.id == derived.id).cloned();
            let key = found.unwrap_or_else(|| {
                let key = Arc::new(derived);
                registry.push(key.clone());
                key
            });
            current.get_or_insert(key);
        }
        let current = current.expect("at least one lattice key");
        let tokens = Tokenizer(Arc::new(Zeroizing::new(blake3::derive_key("HexDB 2026 sealed field token v1", &*current.key))));
        Sealer { current, tokens }
    }

    pub fn tokenizer(&self) -> Tokenizer {
        self.tokens.clone()
    }

    /// The envelope for a value at `path` of document `id` in `tess`.
    pub fn seal(&self, tess: &str, id: &str, path: &str, value: &Value) -> Value {
        let plain = Zeroizing::new(serde_json::to_vec(value).unwrap_or_default());
        let cipher = aes_gcm::Aes256Gcm::new(&aes_gcm::Key::<aes_gcm::Aes256Gcm>::from(*self.current.key));
        let nonce = crate::crypt::random_bytes(crate::crypt::NONCE_LEN);
        let aad = aad(tess, id, path);
        let sealed = cipher
            .encrypt(&aes_gcm::Nonce::try_from(&nonce[..]).expect("NONCE_LEN bytes"), Payload { msg: &plain, aad: aad.as_bytes() })
            .expect("AES-GCM encryption of a bounded value");
        let envelope = format!("{}.{}.{}", FORMAT, self.current.id, STANDARD.encode([nonce, sealed].concat()));
        let mut map = Map::new();
        map.insert(MARK.into(), Value::String(envelope));
        Value::Object(map)
    }

    /// Seal `paths` of a document's fields (dotted paths through objects).
    /// Null, missing and already sealed values are left as they are. True if
    /// anything was sealed.
    pub fn seal_json(&self, tess: &str, id: &str, json: &mut Value, paths: &[String]) -> bool {
        let mut sealed = false;
        for path in paths {
            if let Some(value) = get_path_mut(json, path) {
                if !value.is_null() && !is_envelope(value) {
                    let plain = std::mem::take(value);
                    *value = self.seal(tess, id, path, &plain);
                    wipe(plain);
                    sealed = true;
                }
            }
        }
        sealed
    }

    /// A copy of `doc` with `paths` sealed (the document itself if none apply).
    pub fn seal_document(&self, doc: &Document, paths: &[String]) -> Document {
        if paths.is_empty() || !paths.iter().any(|p| doc.data.contains_key(p.split('.').next().unwrap_or_default())) {
            return doc.clone();
        }
        let mut json = doc.data_json();
        let id = doc.id.to_string();
        if !self.seal_json(&doc.tessellation, &id, &mut json, paths) {
            wipe(json);
            return doc.clone();
        }
        Document { id: doc.id, tessellation: doc.tessellation.clone(), data: crate::document::infer_fields_from_json(&json), ttl: doc.ttl }
    }
}

fn aad(tess: &str, id: &str, path: &str) -> String {
    format!("hexdb-sealed-field:{}:{}:{}", tess, id, path)
}

/// True for an envelope object.
pub fn is_envelope(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.len() == 1 && map.get(MARK).is_some_and(Value::is_string))
}

/// The value inside an envelope, if a registered key opens it for this place.
fn open(tess: &str, id: &str, path: &str, envelope: &str) -> Option<Value> {
    let mut parts = envelope.splitn(3, '.');
    let (Some(FORMAT), Some(key_id), Some(body)) = (parts.next(), parts.next(), parts.next()) else { return None };
    let key = KEYS.read().unwrap_or_else(|e| e.into_inner()).iter().find(|k| k.id == key_id).cloned()?;
    let bytes = STANDARD.decode(body).ok()?;
    if bytes.len() < crate::crypt::NONCE_LEN {
        return None;
    }
    let (nonce, sealed) = bytes.split_at(crate::crypt::NONCE_LEN);
    let cipher = aes_gcm::Aes256Gcm::new(&aes_gcm::Key::<aes_gcm::Aes256Gcm>::from(*key.key));
    let aad = aad(tess, id, path);
    let plain = Zeroizing::new(cipher.decrypt(&aes_gcm::Nonce::try_from(nonce).ok()?, Payload { msg: sealed, aad: aad.as_bytes() }).ok()?);
    serde_json::from_slice(&plain).ok()
}

/// A value as stored at `path` of a document: an envelope is opened (None
/// if it can't be), anything else is returned as it is.
pub fn open_value(tess: &str, id: &str, path: &str, value: &Value) -> Option<Value> {
    match value.get(MARK).and_then(Value::as_str) {
        Some(envelope) if is_envelope(value) => open(tess, id, path, envelope),
        _ => Some(value.clone()),
    }
}

/// True if an index on `field` covers the sealed `path` (the same field, or
/// one inside the other).
pub fn covers(field: &str, path: &str) -> bool {
    field == path || path.strip_prefix(field).is_some_and(|rest| rest.starts_with('.')) || field.strip_prefix(path).is_some_and(|rest| rest.starts_with('.'))
}

/// Open every sealed value in a document's fields, in place. Envelopes that
/// don't open (an unknown key, or one moved from elsewhere) are left as they
/// are. True if the document held any.
pub fn unseal_json(tess: &str, id: &str, json: &mut Value) -> bool {
    fn walk(tess: &str, id: &str, value: &mut Value, path: &mut String, found: &mut bool) {
        let Value::Object(map) = value else { return };
        for (key, child) in map.iter_mut() {
            let len = path.len();
            if len > 0 {
                path.push('.');
            }
            path.push_str(key);
            if is_envelope(child) {
                *found = true;
                if let Some(plain) = child.get(MARK).and_then(Value::as_str).and_then(|e| open(tess, id, path, e)) {
                    *child = plain;
                }
            } else {
                walk(tess, id, child, path, found);
            }
            path.truncate(len);
        }
    }
    let mut found = false;
    walk(tess, id, json, &mut String::new(), &mut found);
    found
}

/// True if the document holds sealed values (as the change feed keeps them).
pub fn has_sealed(doc: &Document) -> bool {
    fn any(value: &crate::document::FieldValue) -> bool {
        match value {
            crate::document::FieldValue::Object(map) => map.contains_key(MARK) || map.values().any(any),
            _ => false,
        }
    }
    doc.data.values().any(any)
}

/// The document with its sealed values opened (a copy only if it held any).
pub fn unsealed(doc: &Document) -> Document {
    if !has_sealed(doc) {
        return doc.clone();
    }
    let mut json = doc.data_json();
    unseal_json(&doc.tessellation, &doc.id.to_string(), &mut json);
    let data = crate::document::infer_fields_from_json(&json);
    wipe(json);
    Document { id: doc.id, tessellation: doc.tessellation.clone(), data, ttl: doc.ttl }
}

/// Overwrite the strings in a value before it's freed (best effort: copies
/// made along the way aren't reached).
pub fn wipe(mut value: Value) {
    fn walk(value: &mut Value) {
        match value {
            Value::String(s) => zeroize::Zeroize::zeroize(s),
            Value::Array(items) => items.iter_mut().for_each(walk),
            Value::Object(map) => map.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut value);
}

fn get_path_mut<'a>(json: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(json, |current, part| current.get_mut(part))
}

/// Which fields are sealed: every path some role masks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SealPolicy {
    tessellations: BTreeMap<String, BTreeSet<String>>,
    everywhere: BTreeSet<String>,
}

impl SealPolicy {
    pub fn from_roles(definitions: &RoleDefinitions) -> SealPolicy {
        let mut policy = SealPolicy::default();
        for rules in definitions.0.values() {
            for (tess, restriction) in &rules.restrictions {
                let paths = restriction.mask.keys().cloned();
                if tess == "*" {
                    policy.everywhere.extend(paths);
                } else {
                    policy.tessellations.entry(tess.clone()).or_default().extend(paths);
                }
            }
        }
        policy.tessellations.retain(|_, paths| !paths.is_empty());
        policy
    }

    pub fn is_empty(&self) -> bool {
        self.tessellations.is_empty() && self.everywhere.is_empty()
    }

    /// The sealed paths by tessellation (`*`: every user tessellation), for `/status`.
    pub fn summary(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = self.tessellations.iter().map(|(t, p)| (t.clone(), p.iter().cloned().collect())).collect();
        if !self.everywhere.is_empty() {
            out.insert("*".into(), self.everywhere.iter().cloned().collect());
        }
        out
    }

    /// The sealed paths of a user tessellation (callers leave system tessellations out).
    pub fn paths(&self, tess: &str) -> Vec<String> {
        let mut paths = self.everywhere.clone();
        if let Some(own) = self.tessellations.get(tess) {
            paths.extend(own.iter().cloned());
        }
        paths.into_iter().collect()
    }

    /// True if `tess` may have different sealed paths under `other`.
    pub fn differs_for(&self, other: &SealPolicy, tess: &str) -> bool {
        self.paths(tess) != other.paths(tess)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sealer() -> Sealer {
        Sealer::new(&[[42u8; 32]])
    }

    #[test]
    fn sealed_values_open_only_where_they_were_sealed() {
        let s = sealer();
        let mut json = json!({ "card": "4111111111111111", "name": "Ada", "bank": { "iban": "DE89370400440532013000" }, "none": null });
        let paths = vec!["card".to_string(), "bank.iban".to_string(), "none".to_string(), "missing".to_string()];
        assert!(s.seal_json("orders", "01A", &mut json, &paths));
        let text = json.to_string();
        assert!(!text.contains("4111111111111111") && !text.contains("DE89"), "{}", text);
        assert!(json["none"].is_null(), "null stays null");
        assert!(!s.seal_json("orders", "01A", &mut json, &paths), "sealing twice changes nothing");

        // Elsewhere (another document, field or tessellation) the envelope doesn't open.
        let mut moved = json!({ "card": json["card"].clone() });
        unseal_json("orders", "01B", &mut moved);
        assert!(is_envelope(&moved["card"]));
        let mut renamed = json!({ "other": json["card"].clone() });
        unseal_json("orders", "01A", &mut renamed);
        assert!(is_envelope(&renamed["other"]));

        assert!(unseal_json("orders", "01A", &mut json));
        assert_eq!(json, json!({ "card": "4111111111111111", "name": "Ada", "bank": { "iban": "DE89370400440532013000" }, "none": null }));
    }

    #[test]
    fn documents_round_trip_through_storage() {
        let s = sealer();
        let doc = Document { id: ulid::Ulid::generate(), tessellation: "orders".into(), data: crate::document::infer_fields_from_json(&json!({ "card": 4111111111111111u64, "n": 1 })), ttl: None };
        let sealed = s.seal_document(&doc, &["card".to_string()]);
        assert!(has_sealed(&sealed));
        let bytes = serde_json::to_vec(&sealed).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("4111111111111111"));
        let read: Document = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read, doc, "reading opens sealed values");
        assert_eq!(unsealed(&sealed), doc);
        let tokens = s.tokenizer();
        assert_eq!(tokens.token(&json!("a")), tokens.token(&json!("a")));
        assert_ne!(tokens.token(&json!("a")), tokens.token(&json!("b")));
        assert!(covers("bank", "bank.iban") && covers("bank.iban.x", "bank.iban") && !covers("bankrupt", "bank"));
    }

    #[test]
    fn the_policy_is_every_masked_path() {
        let mut defs = RoleDefinitions::builtin();
        let restriction = |mask: Value| serde_json::from_value::<crate::access::Restriction>(json!({ "mask": mask })).unwrap();
        let mut rules = crate::auth::RoleRules::default();
        rules.restrictions.insert("orders".into(), restriction(json!({ "card": { "show_last": 4 } })));
        rules.restrictions.insert("*".into(), restriction(json!({ "ssn": {} })));
        defs.0.insert("clerk".into(), rules);
        let policy = SealPolicy::from_roles(&defs);
        assert_eq!(policy.paths("orders"), vec!["card".to_string(), "ssn".to_string()]);
        assert_eq!(policy.paths("people"), vec!["ssn".to_string()]);
        assert!(SealPolicy::from_roles(&RoleDefinitions::builtin()).is_empty());
    }
}
