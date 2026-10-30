// HexDB Core JWS: the lattice's signature on documents it publishes
//
// The A2A agent card is signed as A2A 1.0 asks (section 8.4): a detached JWS
// (RFC 7515) over the card's JSON Canonicalization Scheme form (RFC 8785),
// with `signatures` left out. The key is an Ed25519 key derived from the
// lattice key, so every hex signs alike and the key changes when the lattice
// key is rotated; its public half is published as a JWKS (`jwks`), which the
// signature's `jku` names.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};

/// The lattice's publishing key.
pub struct Signer {
    pair: Ed25519KeyPair,
    kid: String,
}

impl Signer {
    pub fn new(lattice_key: &[u8; 32]) -> Signer {
        let seed = blake3::derive_key("HexDB 2026 published document signing v1", lattice_key);
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("any 32 bytes are an Ed25519 seed");
        let kid = URL_SAFE_NO_PAD.encode(&blake3::hash(pair.public_key().as_ref()).as_bytes()[..12]);
        Signer { pair, kid }
    }

    /// The public key, as a JWKS.
    pub fn jwks(&self) -> Value {
        json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": self.kid, "x": URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref()),
        }] })
    }

    /// A detached signature over a document's canonical form:
    /// `{"protected", "signature"}`, its header naming the key and where it's published.
    pub fn sign(&self, document: &Value, jku: &str) -> Value {
        let header = json!({ "alg": "EdDSA", "typ": "JOSE", "kid": self.kid, "jku": jku });
        let protected = URL_SAFE_NO_PAD.encode(canonical(&header));
        let input = format!("{}.{}", protected, URL_SAFE_NO_PAD.encode(canonical(document)));
        json!({ "protected": protected, "signature": URL_SAFE_NO_PAD.encode(self.pair.sign(input.as_bytes()).as_ref()) })
    }
}

/// RFC 8785 (JCS): no whitespace, object members sorted by their names' UTF-16
/// code units, strings escaped minimally, numbers in their shortest form.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            members.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (i, (name, item)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(name, out);
                out.push(':');
                write(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::String(s) => string(s, out),
        // Integers print exactly; other numbers as ECMAScript's shortest round-trip form,
        // which Rust's float Display matches for the values cards hold.
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (_, Some(u), _) => out.push_str(&u.to_string()),
            (_, _, Some(f)) if f.fract() == 0.0 && f.abs() < 1e15 => out.push_str(&format!("{}", f as i64)),
            (_, _, Some(f)) => out.push_str(&f.to_string()),
            _ => out.push_str("null"),
        },
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
    }
}

fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Leave out properties with default values (false, empty strings, lists and
/// objects), which A2A removes before signing a card.
pub fn without_defaults(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, without_defaults(v)))
                .filter(|(_, v)| !matches!(v, Value::Bool(false)) && !matches!(v, Value::String(s) if s.is_empty()) && !matches!(v, Value::Array(a) if a.is_empty()))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_defaults).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, ED25519};

    #[test]
    fn canonical_json_follows_rfc_8785() {
        // From RFC 8785's examples: member order by UTF-16 code units, minimal escapes.
        let value = json!({ "\u{20ac}": "Euro Sign", "\r": "Carriage Return", "1": "One", "\u{1f600}": "Emoji: Grinning Face", "\u{0080}": "Control", "\u{00f6}": "Latin Small Letter O With Diaeresis", "\u{fb33}": "Hebrew Letter Dalet With Dagesh" });
        let expected = concat!(
            "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{0080}\":\"Control\",\"\u{00f6}\":\"Latin Small Letter O With Diaeresis\",",
            "\"\u{20ac}\":\"Euro Sign\",\"\u{1f600}\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
        );
        assert_eq!(canonical(&value), expected);
        assert_eq!(canonical(&json!({ "b": [1, true, null, "x\ty\u{1}"], "a": {} })), "{\"a\":{},\"b\":[1,true,null,\"x\\ty\\u0001\"]}");
        assert_eq!(canonical(&json!(2.0)), "2");
    }

    #[test]
    fn signatures_verify_with_the_published_key() {
        let signer = Signer::new(&[7u8; 32]);
        let card = without_defaults(json!({ "name": "HexDB", "capabilities": { "streaming": true, "extendedAgentCard": false }, "tags": [] }));
        assert_eq!(card, json!({ "name": "HexDB", "capabilities": { "streaming": true } }));
        let sig = signer.sign(&card, "https://hexdb.example.com/.well-known/jwks.json");
        let protected = sig["protected"].as_str().unwrap();
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).unwrap()).unwrap();
        assert_eq!((header["alg"].as_str(), header["typ"].as_str()), (Some("EdDSA"), Some("JOSE")));
        let jwk = &signer.jwks()["keys"][0];
        assert_eq!(header["kid"], jwk["kid"]);
        let x = URL_SAFE_NO_PAD.decode(jwk["x"].as_str().unwrap()).unwrap();
        let input = format!("{}.{}", protected, URL_SAFE_NO_PAD.encode(canonical(&card)));
        let signature = URL_SAFE_NO_PAD.decode(sig["signature"].as_str().unwrap()).unwrap();
        assert!(UnparsedPublicKey::new(&ED25519, &x).verify(input.as_bytes(), &signature).is_ok());
        assert!(UnparsedPublicKey::new(&ED25519, &x).verify(b"tampered", &signature).is_err());
        assert_eq!(Signer::new(&[7u8; 32]).jwks(), signer.jwks(), "every hex derives the same key");
        assert_ne!(Signer::new(&[8u8; 32]).jwks(), signer.jwks());
    }
}
