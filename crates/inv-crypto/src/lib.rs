//! `inv-crypto`: zero-knowledge, client-side encryption with AES-256-GCM.
//!
//! All secrets stay client-side: a [`Key`] is 32 random bytes, typically carried
//! in a URL fragment (never sent to the server) via [`Key::to_url_fragment`].
//! [`encrypt`] produces a self-describing blob laid out as
//! `12-byte random nonce || ciphertext+tag`; [`decrypt`] AEAD-verifies and
//! reverses it. No input can cause a panic — malformed data yields a
//! [`CryptoError`] instead.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key as CipherKey, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Length of an AES-256 key in bytes.
const KEY_LEN: usize = 32;
/// Length of the AES-GCM nonce in bytes (96-bit, the recommended size).
const NONCE_LEN: usize = 12;

/// Errors that can arise while handling keys or encrypted blobs.
///
/// Never the result of a panic: every fallible path returns one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// A URL fragment was not valid base64 (URL-safe, no padding) or did not
    /// decode to exactly 32 bytes.
    BadFragment,
    /// The blob failed AEAD verification: wrong key, or the data was corrupted
    /// or tampered with.
    WrongKeyOrCorrupt,
    /// The blob is too short to even contain a nonce.
    TooShort,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            CryptoError::BadFragment => "invalid key fragment",
            CryptoError::WrongKeyOrCorrupt => "wrong key or corrupt ciphertext",
            CryptoError::TooShort => "blob too short to contain a nonce",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for CryptoError {}

/// A 256-bit symmetric key for AES-256-GCM.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// Generate a fresh key from 32 cryptographically random bytes.
    pub fn generate() -> Key {
        let mut bytes = [0u8; KEY_LEN];
        // getrandom only fails if the OS RNG is unavailable; treat that as fatal
        // since there is no meaningful way to produce a key without entropy.
        getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
        Key(bytes)
    }

    /// Wrap raw key bytes.
    pub fn from_bytes(b: [u8; KEY_LEN]) -> Key {
        Key(b)
    }

    /// Borrow the raw key bytes.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Encode the key as a URL-safe, unpadded base64 string suitable for a URL
    /// fragment.
    pub fn to_url_fragment(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// Decode a key from a URL fragment produced by [`Key::to_url_fragment`].
    ///
    /// Returns [`CryptoError::BadFragment`] if the string is not valid
    /// URL-safe base64 or does not decode to exactly 32 bytes.
    pub fn from_url_fragment(s: &str) -> Result<Key, CryptoError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| CryptoError::BadFragment)?;
        let bytes: [u8; KEY_LEN] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::BadFragment)?;
        Ok(Key(bytes))
    }
}

// Deliberately avoid deriving Debug to keep key material out of logs.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// Encrypt `plaintext` under `key`.
///
/// The output is `12-byte random nonce || ciphertext+tag`. A fresh random
/// nonce is generated on every call, so encrypting the same plaintext twice
/// yields different blobs.
pub fn encrypt(key: &Key, plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(CipherKey::<Aes256Gcm>::from_slice(key.as_bytes()));

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).expect("OS RNG unavailable");
    let nonce = Nonce::from_slice(&nonce_bytes);

    // AES-GCM encryption only fails on absurd input sizes (far beyond what we
    // ever handle); there is no recoverable error to surface here.
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .expect("AES-GCM encryption failed");

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

/// Decrypt a blob produced by [`encrypt`].
///
/// Returns [`CryptoError::TooShort`] if the blob cannot contain a nonce, or
/// [`CryptoError::WrongKeyOrCorrupt`] if AEAD verification fails (wrong key,
/// corruption, or tampering).
pub fn decrypt(key: &Key, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < NONCE_LEN {
        return Err(CryptoError::TooShort);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(CipherKey::<Aes256Gcm>::from_slice(key.as_bytes()));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::WrongKeyOrCorrupt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_simple() {
        let k = Key::generate();
        let msg = b"hello world";
        let blob = encrypt(&k, msg);
        let out = decrypt(&k, &blob).unwrap();
        assert_eq!(out, msg);
    }

    #[test]
    fn roundtrip_empty() {
        let k = Key::generate();
        let blob = encrypt(&k, b"");
        assert_eq!(decrypt(&k, &blob).unwrap(), b"");
    }

    #[test]
    fn too_short_blob_errors() {
        let k = Key::generate();
        // Anything shorter than the nonce cannot be decrypted.
        for len in 0..NONCE_LEN {
            let blob = vec![0u8; len];
            assert_eq!(decrypt(&k, &blob), Err(CryptoError::TooShort));
        }
    }

    #[test]
    fn nonce_sized_blob_is_too_short_for_tag() {
        // Exactly NONCE_LEN bytes: passes the length gate but has no tag, so
        // AEAD verification fails rather than panicking.
        let k = Key::generate();
        let blob = vec![0u8; NONCE_LEN];
        assert_eq!(decrypt(&k, &blob), Err(CryptoError::WrongKeyOrCorrupt));
    }

    #[test]
    fn bad_fragment_errors() {
        // Not valid base64.
        assert_eq!(Key::from_url_fragment("!!!!"), Err(CryptoError::BadFragment));
        // Valid base64 but wrong length (decodes to 1 byte).
        assert_eq!(Key::from_url_fragment("AA"), Err(CryptoError::BadFragment));
        // Empty string -> 0 bytes.
        assert_eq!(Key::from_url_fragment(""), Err(CryptoError::BadFragment));
    }

    #[test]
    fn from_bytes_as_bytes_roundtrip() {
        let raw = [7u8; KEY_LEN];
        let k = Key::from_bytes(raw);
        assert_eq!(k.as_bytes(), &raw);
    }

    #[test]
    fn debug_redacts_key_material() {
        let k = Key::from_bytes([0xAB; KEY_LEN]);
        let s = format!("{k:?}");
        assert!(!s.contains("171") && !s.contains("ab"));
        assert!(s.contains("redacted"));
    }

    // Strategy: arbitrary 32-byte key.
    prop_compose! {
        fn arb_key()(bytes in any::<[u8; KEY_LEN]>()) -> Key {
            Key::from_bytes(bytes)
        }
    }

    proptest! {
        // Invariant 1: roundtrip for arbitrary plaintext (including empty).
        #[test]
        fn prop_roundtrip(k in arb_key(), msg in prop::collection::vec(any::<u8>(), 0..2048)) {
            let blob = encrypt(&k, &msg);
            prop_assert_eq!(decrypt(&k, &blob).unwrap(), msg);
        }

        // Invariant 2: nonce randomness — two encryptions of the same plaintext
        // under the same key produce different blobs.
        #[test]
        fn prop_nonce_randomness(k in arb_key(), msg in prop::collection::vec(any::<u8>(), 0..256)) {
            let a = encrypt(&k, &msg);
            let b = encrypt(&k, &msg);
            prop_assert_ne!(a, b);
        }

        // Invariant 3: a wrong key never decrypts (and never panics).
        #[test]
        fn prop_wrong_key(
            k1 in arb_key(),
            k2 in arb_key(),
            msg in prop::collection::vec(any::<u8>(), 0..256),
        ) {
            prop_assume!(k1 != k2);
            let blob = encrypt(&k1, &msg);
            prop_assert!(decrypt(&k2, &blob).is_err());
        }

        // Invariant 4: flipping any single byte of a valid blob breaks decryption.
        #[test]
        fn prop_tamper(
            k in arb_key(),
            msg in prop::collection::vec(any::<u8>(), 0..256),
            bit in any::<u8>(),
        ) {
            let mut blob = encrypt(&k, &msg);
            let idx = (bit as usize) % blob.len();
            // Guaranteed to change the byte (xor with non-zero).
            blob[idx] ^= 0x01;
            prop_assert!(decrypt(&k, &blob).is_err());
        }

        // Invariant 5: fragment roundtrip.
        #[test]
        fn prop_fragment_roundtrip(k in arb_key()) {
            let frag = k.to_url_fragment();
            prop_assert_eq!(Key::from_url_fragment(&frag).unwrap(), k);
        }

        // Invariant 6: arbitrary/garbage blobs never panic; they error.
        #[test]
        fn prop_garbage_blob_never_panics(
            k in arb_key(),
            blob in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            // Decryption of random bytes is overwhelmingly an error; the
            // astronomically unlikely case of a valid forgery would still be
            // an Ok(_), which is fine — the contract is "no panic".
            match decrypt(&k, &blob) {
                Ok(_) => {}
                Err(CryptoError::TooShort) => prop_assert!(blob.len() < NONCE_LEN),
                Err(CryptoError::WrongKeyOrCorrupt) => prop_assert!(blob.len() >= NONCE_LEN),
                Err(CryptoError::BadFragment) => prop_assert!(false, "unexpected BadFragment"),
            }
        }

        // Invariant 6 (fragment side): garbage fragments never panic.
        #[test]
        fn prop_garbage_fragment_never_panics(s in ".{0,128}") {
            let _ = Key::from_url_fragment(&s);
        }
    }
}
