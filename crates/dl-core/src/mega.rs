//! Mega links: what is in one, and what a client needs to read the file.
//!
//! # The shape
//!
//! `https://mega.nz/file/<handle>#<key>` — and the older
//! `https://mega.nz/#!<handle>!<key>`, which is still in circulation.
//!
//! `<key>` is 32 bytes, base64url. It is in the URL *fragment*, which browsers never put
//! on the wire, so Mega's own servers never receive it. That is the whole design: they
//! store ciphertext they cannot read, and the reader supplies the key out of band. It
//! also means a downloader that honours this is telling the truth when it says nothing
//! leaves the machine — the key stays in the tab and the decryption happens there.
//!
//! # What the 32 bytes are
//!
//! | bytes | meaning |
//! |---|---|
//! | 0..16 | key material, first half |
//! | 16..32 | key material, second half — also the nonce and MAC |
//!
//! The AES-128 key is the two halves XORed. The nonce is `[16..24]`, and `[24..32]` is a
//! MAC over the plaintext that Mega's own client checks. File data is AES-128-CTR with
//! the counter block `nonce ‖ block_index`, which is why any byte range can be decrypted
//! on its own — and why this composes with a resumable, many-connection download instead
//! of fighting it.
//!
//! No AES here: the key arithmetic is XOR and slicing, and the ciphers themselves are
//! WebCrypto's, which every browser has in hardware and which costs the wasm bundle
//! nothing.

use serde::Serialize;

/// Everything a client needs to fetch and read one Mega file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MegaLink {
    /// The file handle Mega's API is asked about.
    pub handle: String,
    /// AES-128 key, 16 bytes.
    pub key: Vec<u8>,
    /// CTR nonce, 8 bytes. The counter block is this followed by the block index.
    pub nonce: Vec<u8>,
    /// The MAC Mega's client checks the plaintext against, 8 bytes.
    pub meta_mac: Vec<u8>,
}

/// Parse a Mega file link.
///
/// `None` for anything that is not one, including a *folder* link — those name a
/// directory whose contents need a second, different request, and quietly treating one as
/// a file is how a downloader produces a confident, empty result.
pub fn parse(url: &str) -> Option<MegaLink> {
    let url = url.trim();
    let host_ok = url.contains("mega.nz/") || url.contains("mega.co.nz/");
    if !host_ok {
        return None;
    }

    // Two link forms, both still in circulation, and the separator differs. `/folder/`
    // and `/#F!` match neither on purpose — see `is_folder_link`.
    let (handle, key_b64) = match (after(url, "/file/"), after(url, "/#!")) {
        (Some(rest), _) => rest.split_once('#')?,
        (None, Some(rest)) => rest.split_once('!')?,
        (None, None) => return None,
    };

    let handle = handle.split(['?', '&']).next()?;
    let key_b64 = key_b64.split(['?', '&', '/']).next()?;
    if handle.is_empty() {
        return None;
    }

    let raw = base64url_decode(key_b64)?;
    if raw.len() != 32 {
        return None;
    }

    // The two halves XORed. Mega stores the key this way so that the same 32 bytes carry
    // the key, the nonce and the MAC in one token.
    let key: Vec<u8> = (0..16).map(|i| raw[i] ^ raw[i + 16]).collect();

    Some(MegaLink {
        handle: handle.to_string(),
        key,
        nonce: raw[16..24].to_vec(),
        meta_mac: raw[24..32].to_vec(),
    })
}

/// Whether this URL is a Mega file link this can read.
pub fn matches(url: &str) -> bool {
    parse(url).is_some()
}

/// Whether this is a Mega *folder* link — recognised so it can be refused by name.
pub fn is_folder_link(url: &str) -> bool {
    let url = url.trim();
    (url.contains("mega.nz/") || url.contains("mega.co.nz/"))
        && (url.contains("/folder/") || url.contains("/#F!"))
}

fn after<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    haystack.find(needle).map(|i| &haystack[i + needle.len()..])
}

/// Mega uses base64url without padding, and its keys are fixed length, so a decoder
/// that ignores padding entirely is both correct here and shorter than a dependency.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut have = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for c in input.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => break,
            _ => return None,
        };
        bits = (bits << 6) | v;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32 bytes whose two halves are known, so the XOR is checkable by hand.
    fn sample_key_b64() -> String {
        // First half 0x00..0x0f, second half 0xff..0xf0 — so the XOR is the complement.
        let mut raw = Vec::new();
        raw.extend(0u8..16);
        raw.extend((0u8..16).map(|b| 255 - b));
        base64url_encode(&raw)
    }

    fn base64url_encode(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..=chunk.len() {
                out.push(A[((n >> (18 - i * 6)) & 63) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn a_modern_file_link_yields_handle_key_nonce_and_mac() {
        let link = parse(&format!(
            "https://mega.nz/file/AbCdEfGh#{}",
            sample_key_b64()
        ))
        .expect("parsed");
        assert_eq!(link.handle, "AbCdEfGh");
        // The two halves XORed: i ^ (255 - i).
        let expected: Vec<u8> = (0u8..16).map(|i| i ^ (255 - i)).collect();
        assert_eq!(link.key, expected);
        assert_eq!(link.nonce, (0u8..8).map(|i| 255 - i).collect::<Vec<_>>());
        assert_eq!(
            link.meta_mac,
            (8u8..16).map(|i| 255 - i).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_legacy_hash_bang_form_still_parses() {
        let modern = parse(&format!(
            "https://mega.nz/file/AbCdEfGh#{}",
            sample_key_b64()
        ))
        .unwrap();
        let legacy = parse(&format!("https://mega.nz/#!AbCdEfGh!{}", sample_key_b64())).unwrap();
        assert_eq!(modern, legacy, "the two forms name the same file");
    }

    #[test]
    fn the_old_domain_is_the_same_service() {
        assert!(matches(&format!(
            "https://mega.co.nz/file/AbCdEfGh#{}",
            sample_key_b64()
        )));
    }

    #[test]
    fn a_folder_link_is_recognised_but_never_read_as_a_file() {
        // Silently treating a folder as a file is how a downloader produces a confident
        // empty result, so these are told apart explicitly.
        for url in [
            "https://mega.nz/folder/AbCdEfGh#0123456789abcdefghijklmnopqrstuvwxyzABCD",
            "https://mega.nz/#F!AbCdEfGh!0123456789abcdefghijklmnopqrstuvwxyzABCD",
        ] {
            assert!(is_folder_link(url), "should be seen as a folder: {url}");
            assert_eq!(parse(url), None, "must not parse as a file: {url}");
        }
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        // The classic truncated paste. Accepting it would mean decrypting to noise and
        // reporting a corrupt file rather than a bad link.
        assert_eq!(parse("https://mega.nz/file/AbCdEfGh#tooshort"), None);
    }

    #[test]
    fn query_parameters_after_the_key_are_not_part_of_it() {
        let link = parse(&format!(
            "https://mega.nz/file/AbCdEfGh#{}?utm_source=x",
            sample_key_b64()
        ))
        .expect("parsed");
        assert_eq!(link.key.len(), 16);
        assert_eq!(link.handle, "AbCdEfGh");
    }

    #[test]
    fn anything_that_is_not_a_mega_link_is_not_one() {
        for url in [
            "https://example.com/file/AbCdEfGh#key",
            "https://mega.nz/file/AbCdEfGh", // no key at all
            "https://notmega.nz.evil.test/file/A#B",
        ] {
            assert_eq!(parse(url), None, "should not parse: {url}");
        }
    }
}
