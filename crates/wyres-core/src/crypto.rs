//! Node keys and signatures. Every node has an Ed25519 key pair; each event it authors is
//! signed, so events relayed through a hub can be checked against their origin node.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|e| Error::Invalid(format!("invalid base64: {e}")))
}

pub fn generate_key() -> Result<SigningKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn decode_secret(text: &str) -> Result<SigningKey> {
    let bytes: [u8; 32] = unb64(text)?
        .try_into()
        .map_err(|_| Error::Invalid("node secret key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn decode_public(text: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = unb64(text)?
        .try_into()
        .map_err(|_| Error::Invalid("node public key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| Error::Invalid(format!("invalid public key: {e}")))
}

pub fn public_of(key: &SigningKey) -> String {
    b64(key.verifying_key().as_bytes())
}

/// Short, human-comparable fingerprint of a public key.
pub fn fingerprint(public_b64: &str) -> String {
    let hex = sha256_hex(public_b64.as_bytes());
    hex.as_bytes()[..16]
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// The bytes an event signature covers.
pub fn event_message(
    event_id: &str,
    node_id: &str,
    ts: i64,
    actor: &str,
    kind: &str,
    payload: &str,
) -> Vec<u8> {
    format!(
        "wyres-event-v1\n{event_id}\n{node_id}\n{ts}\n{actor}\n{kind}\n{}",
        sha256_hex(payload.as_bytes())
    )
    .into_bytes()
}

/// The bytes a node-to-node HTTP request signature covers.
pub fn request_message(method: &str, path_and_query: &str, time_ms: i64, body: &[u8]) -> Vec<u8> {
    format!(
        "wyres-request-v1\n{method}\n{path_and_query}\n{time_ms}\n{}",
        sha256_hex(body)
    )
    .into_bytes()
}

pub fn sign(key: &SigningKey, message: &[u8]) -> String {
    b64(&key.sign(message).to_bytes())
}

pub fn verify(public_b64: &str, message: &[u8], signature_b64: &str) -> bool {
    let Ok(key) = decode_public(public_b64) else {
        return false;
    };
    let Ok(sig_bytes) = unb64(signature_b64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    key.verify(message, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let key = generate_key().unwrap();
        let public = public_of(&key);
        let msg = event_message("E", "N", 1, "a", "k", "{}");
        let sig = sign(&key, &msg);
        assert!(verify(&public, &msg, &sig));
        let tampered = event_message("E", "N", 1, "a", "k", "{\"x\":1}");
        assert!(!verify(&public, &tampered, &sig));
        let other = public_of(&generate_key().unwrap());
        assert!(!verify(&other, &msg, &sig));
        assert!(!verify("garbage", &msg, &sig));
        let round = decode_secret(&b64(key.as_bytes())).unwrap();
        assert_eq!(public_of(&round), public);
        assert_eq!(fingerprint(&public).len(), 19);
    }
}
