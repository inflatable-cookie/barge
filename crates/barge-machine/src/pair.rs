//! Ephemeral X25519 + HKDF-SHA256 pairing key exchange and six-digit code.
//!
//! Both sides generate an ephemeral X25519 keypair, compute the raw shared
//! secret, derive the 256-bit HMAC key with HKDF-SHA256 using
//! `salt = client_pub || daemon_pub` and `info = "barge-pair-v1"`, and derive
//! the 6-digit verification code from `SHA-256("barge-pair-code-v1\0" ||
//! client_pub || daemon_pub)`.

use std::ffi::CStr;
use std::fmt;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{auth, Error, ErrorKind, Result};

const HKDF_INFO: &[u8] = b"barge-pair-v1";
const CODE_PREFIX: &[u8] = b"barge-pair-code-v1\0";

/// Which side of the pairing exchange holds the local secret key, so both
/// sides place `client_public || daemon_public` in the same order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRole {
    /// The connecting client initiating `POST /v1/pair`.
    Client,
    /// The daemon answering `POST /v1/pair`.
    Daemon,
}

/// An ephemeral X25519 keypair used for one pairing request.
pub struct KeyPair {
    secret: StaticSecret,
    public: PublicKey,
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair")
            .field("public", &self.public_key_hex())
            .finish_non_exhaustive()
    }
}

impl KeyPair {
    /// Generate a fresh random X25519 keypair.
    pub fn generate() -> Result<Self> {
        let bytes = auth::random(32)
            .map_err(|e| Error::new(ErrorKind::Io, format!("cannot generate keypair: {e}")))?;
        let raw: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::new(ErrorKind::Io, "random key length mismatch"))?;
        Ok(Self::from_secret_bytes(raw))
    }

    /// Construct a keypair from exact 32-byte X25519 secret scalar bytes.
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Raw 32-byte X25519 public key.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Lowercase 64-character hex X25519 public key.
    pub fn public_key_hex(&self) -> String {
        auth::hex(self.public.as_bytes())
    }

    /// Derive the shared 256-bit HMAC key and 6-digit verification code with
    /// the peer's 32-byte X25519 public key.
    pub fn derive(&self, peer_public: &[u8; 32], role: PairRole) -> Result<DerivedPairing> {
        let peer = PublicKey::from(*peer_public);
        let shared = self.secret.diffie_hellman(&peer);
        if !shared.was_contributory() {
            return Err(Error::refused("invalid X25519 public key"));
        }
        let own = self.public.to_bytes();
        let (client_pub, daemon_pub) = match role {
            PairRole::Client => (&own, peer_public),
            PairRole::Daemon => (peer_public, &own),
        };

        let mut salt = [0_u8; 64];
        salt[..32].copy_from_slice(client_pub);
        salt[32..].copy_from_slice(daemon_pub);

        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let mut key = [0_u8; 32];
        hk.expand(HKDF_INFO, &mut key)
            .map_err(|_| Error::new(ErrorKind::Io, "HKDF expansion failed"))?;

        let code = verification_code(client_pub, daemon_pub);
        Ok(DerivedPairing { key, code })
    }
}

/// The output of X25519 + HKDF-SHA256 pairing key agreement.
#[derive(Clone, PartialEq, Eq)]
pub struct DerivedPairing {
    key: [u8; 32],
    code: String,
}

impl fmt::Debug for DerivedPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DerivedPairing")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

impl DerivedPairing {
    /// The derived 256-bit client HMAC key.
    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }

    /// The six-digit verification code (`"000000"`..`"999999"`).
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Parse a 64-character lowercase hex public key into 32 bytes.
pub fn parse_public_key(hex: &str) -> Result<[u8; 32]> {
    let bytes = auth::decode(hex.trim(), 32)
        .ok_or_else(|| Error::usage("public_key must be 64 lowercase hexadecimal characters"))?;
    bytes
        .try_into()
        .map_err(|_| Error::usage("public_key must be 32 bytes"))
}

/// Compute the 6-digit verification code from `(client_pub, daemon_pub)`.
pub fn verification_code(client_pub: &[u8; 32], daemon_pub: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CODE_PREFIX);
    hasher.update(client_pub);
    hasher.update(daemon_pub);
    let digest = hasher.finalize();
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{value:06}")
}

/// The local machine's short hostname (before any `.local` or domain suffix),
/// used as the default client display name when initiating pairing.
pub fn local_hostname() -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: `buf` is a live, writable buffer of `buf.len()` bytes.
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    buf[buf.len() - 1] = 0;
    // SAFETY: `buf` is NUL-terminated above.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_string();
    let short = name.split('.').next().unwrap_or("").trim().to_string();
    if short.is_empty() {
        None
    } else {
        Some(short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_daemon_derive_identical_key_and_code() {
        let client = KeyPair::generate().unwrap();
        let daemon = KeyPair::generate().unwrap();
        let from_client = client
            .derive(&daemon.public_key_bytes(), PairRole::Client)
            .unwrap();
        let from_daemon = daemon
            .derive(&client.public_key_bytes(), PairRole::Daemon)
            .unwrap();
        assert_eq!(from_client.key(), from_daemon.key());
        assert_eq!(from_client.code(), from_daemon.code());
        assert_eq!(from_client.code().len(), 6);
        assert!(from_client.code().bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn low_order_public_key_is_refused() {
        let daemon = KeyPair::generate().unwrap();
        let error = daemon.derive(&[0; 32], PairRole::Daemon).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Refused);
    }

    #[test]
    fn matches_fixed_test_vector() {
        // Fixed vector documented in docs/knowledge/machines.md for all clients.
        let client_secret: [u8; 32] = auth::decode(
            "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
            32,
        )
        .unwrap()
        .try_into()
        .unwrap();
        let daemon_secret: [u8; 32] = auth::decode(
            "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
            32,
        )
        .unwrap()
        .try_into()
        .unwrap();

        let client = KeyPair::from_secret_bytes(client_secret);
        let daemon = KeyPair::from_secret_bytes(daemon_secret);

        assert_eq!(
            client.public_key_hex(),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            daemon.public_key_hex(),
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
        );

        let from_client = client
            .derive(&daemon.public_key_bytes(), PairRole::Client)
            .unwrap();
        let from_daemon = daemon
            .derive(&client.public_key_bytes(), PairRole::Daemon)
            .unwrap();

        assert_eq!(from_client, from_daemon);
        assert_eq!(
            auth::hex(from_client.key()),
            "18d60e9b82dc130c6972596a7b47f3c1381520faa8704ded8429c85231323ea8"
        );
        assert_eq!(from_client.code(), "340092");
    }
}
