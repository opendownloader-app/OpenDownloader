//! Streaming SHA-256.
//!
//! This exists in Rust rather than TypeScript for a concrete reason: WebCrypto's
//! `crypto.subtle.digest()` is one-shot, not incremental. Verifying a multi-gigabyte
//! download through it would mean holding the entire file in memory. `sha2` hashes
//! incrementally, so the manager can stream the finished file back off disk in bounded
//! chunks and still get a single digest.
//!
//! Note the verification reads the file back from the sink rather than hashing bytes on
//! their way out. That is deliberate — it proves what actually landed on disk, which is
//! the only thing that makes a *resumed* download trustworthy.

use sha2::{Digest, Sha256};

/// Incremental SHA-256 over an ordered byte stream.
#[derive(Default, Clone)]
pub struct Hasher {
    inner: Sha256,
    len: u64,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
        self.len += bytes.len() as u64;
    }

    /// Bytes hashed so far — used to confirm the read-back saw the whole file.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The digest of everything hashed so far.
    ///
    /// Takes `&self` and clones the internal state rather than consuming the hasher, so
    /// a long-lived session object can report progress without being torn down. Cloning
    /// a SHA-256 state is a 100-byte copy, not a rehash.
    pub fn finish_hex(&self) -> String {
        let digest = self.inner.clone().finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            // `write!` would pull in `core::fmt` machinery per byte; this is both faster
            // and smaller in wasm.
            const HEX: &[u8; 16] = b"0123456789abcdef";
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_sha256_vectors() {
        let mut h = Hasher::new();
        h.update(b"abc");
        assert_eq!(
            h.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let empty = Hasher::new();
        assert_eq!(
            empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn chunking_does_not_change_the_digest() {
        let mut whole = Hasher::new();
        whole.update(b"the quick brown fox");
        let mut split = Hasher::new();
        split.update(b"the quick ");
        split.update(b"brown fox");
        assert_eq!(whole.finish_hex(), split.finish_hex());
    }

    #[test]
    fn tracks_the_number_of_bytes_seen() {
        let mut h = Hasher::new();
        assert!(h.is_empty());
        h.update(&[0u8; 100]);
        h.update(&[1u8; 23]);
        assert_eq!(h.len(), 123);
    }
}
