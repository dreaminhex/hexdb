// HexDB Core: AWS Signature Version 4
//
// Signs requests to AWS APIs and S3-compatible stores: S3 for point-in-time
// restore (`objstore`), KMS for customer-managed keys (`kms`). Implemented on
// the HMAC and SHA-256 libraries HexDB already uses.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Credentials and where they're used.
pub struct Signer<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
    /// `s3`, `kms`...
    pub service: &'a str,
}

/// Percent-encode as SigV4 requires: everything but A-Z a-z 0-9 - _ . ~
pub fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// The query string, sorted and encoded as SigV4 signs it (and as it's sent).
pub fn canonical_query(query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k), uri_encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&")
}

/// Hex SHA-256 of a request body.
pub fn payload_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// `YYYYMMDDTHHMMSSZ`, now.
pub fn amz_date_now() -> String {
    jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

impl Signer<'_> {
    /// The `Authorization` header for a request. `headers` are the names and values to
    /// sign (all of them; names in any case); `path` is the encoded URL path.
    pub fn authorization(&self, method: &str, path: &str, query: &[(&str, String)], headers: &[(String, String)], payload_hash: &str, amz_date: &str) -> String {
        let date = &amz_date[..8];
        let mut headers: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
        headers.sort();
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{}:{}\n", k, v)).collect();
        let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let canonical_request = format!("{}\n{}\n{}\n{}\n{}\n{}", method, path, canonical_query(query), canonical_headers, signed_headers, payload_hash);
        let scope = format!("{}/{}/{}/aws4_request", date, self.region, self.service);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, hex::encode(Sha256::digest(canonical_request.as_bytes())));
        let key = hmac(format!("AWS4{}", self.secret_access_key).as_bytes(), date);
        let key = hmac(&key, self.region);
        let key = hmac(&key, self.service);
        let key = hmac(&key, "aws4_request");
        let signature = hex::encode(hmac(&key, &string_to_sign));
        format!("AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}", self.access_key_id, scope, signed_headers, signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's Signature Version 4 test suite, "get-vanilla".
    #[test]
    fn signs_the_test_suites_vanilla_get() {
        let signer = Signer { access_key_id: "AKIDEXAMPLE", secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", region: "us-east-1", service: "service" };
        let headers = vec![("Host".to_string(), "example.amazonaws.com".to_string()), ("X-Amz-Date".to_string(), "20150830T123600Z".to_string())];
        let auth = signer.authorization("GET", "/", &[], &headers, &payload_hash(b""), "20150830T123600Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }
}
