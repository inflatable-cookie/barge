//! Shared LAN request authentication and owner-only key storage.
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Timestamp request header.
pub const TIMESTAMP: &str = "x-barge-timestamp";
/// Nonce request header.
pub const NONCE: &str = "x-barge-nonce";
/// MAC request header.
pub const SIGNATURE: &str = "x-barge-signature";

type HmacSha256 = Hmac<Sha256>;

/// Encode bytes as lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a fixed-length lowercase hexadecimal value.
pub fn decode(value: &str, length: usize) -> Option<Vec<u8>> {
    if value.len() != length * 2
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    (0..length)
        .map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// Generate cryptographically random bytes.
pub fn random(length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes)
}

fn mac(
    key: &[u8],
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> Option<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    let digest = hex(&Sha256::digest(body));
    mac.update(format!("{method}\n{path}\n{timestamp}\n{nonce}\n{digest}").as_bytes());
    Some(mac)
}

/// Sign a request and its body.
pub fn sign(
    key: &[u8],
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> Option<String> {
    Some(hex(&mac(key, method, path, timestamp, nonce, body)?
        .finalize()
        .into_bytes()))
}

/// Verify a request MAC in constant time.
pub fn verify(
    key: &[u8],
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    decode(signature, 32).is_some_and(|signature| {
        mac(key, method, path, timestamp, nonce, body)
            .is_some_and(|mac| mac.verify_slice(&signature).is_ok())
    })
}

/// Atomically replace a key without ever creating a world-readable file.
#[cfg(unix)]
pub fn write_key(path: &Path, key: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("key path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".barge-key-{}", hex(&random(16)?)));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(key.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
