//! The eD2k hash, and the links that carry one.
//!
//! # The hash
//!
//! eDonkey names a file by a hash of its contents, computed in two levels. The file is
//! cut into chunks of exactly 9,728,000 bytes (9500 KiB); each chunk is hashed with MD4;
//! and the file's hash is the MD4 of those chunk hashes joined together. A file of one
//! chunk or less skips the second level and is simply the MD4 of its bytes.
//!
//! There is one notorious disagreement, and it is not a detail that can be waved away:
//! for a file whose length is an exact multiple of the chunk size, eMule appends the MD4
//! of a *zero-length* chunk before hashing the list, and the original eDonkey client does
//! not. The two produce different hashes for the same bytes. eMule won — its hash is what
//! links in the wild carry — so [`Ed2kHasher`] implements the eMule rule, and
//! [`Ed2kHasher::finish_hex_legacy`] exposes the other for the rare old link.
//!
//! # Why it is here
//!
//! An `ed2k://` link states the file's name, its exact length and this hash. That is
//! enough to *check* a file, whatever route it arrived by — and checking what landed on
//! disk is what everything else in this crate already does. Downloading from the eDonkey
//! network is a different matter and is not attempted; see [`Ed2kLink::http_source`].

use md4::{Digest, Md4};

/// The chunk size the whole scheme is built on. Not a tunable.
pub const CHUNK: u64 = 9_728_000;

/// Incremental eD2k hashing over an ordered byte stream.
///
/// Streaming rather than one-shot for the same reason [`crate::integrity::Hasher`] is:
/// the file is read back off disk in bounded pieces, and holding a multi-gigabyte
/// download in memory to hash it is not an option.
#[derive(Clone)]
pub struct Ed2kHasher {
    /// MD4 of the chunk currently being filled.
    current: Md4,
    /// How many bytes are in the current chunk so far.
    current_len: u64,
    /// The finished chunk digests, concatenated — the input to the second level.
    chunk_digests: Vec<u8>,
    len: u64,
}

impl Default for Ed2kHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Ed2kHasher {
    pub fn new() -> Self {
        Self {
            current: Md4::new(),
            current_len: 0,
            chunk_digests: Vec::new(),
            len: 0,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let room = (CHUNK - self.current_len) as usize;
            let take = room.min(bytes.len());
            self.current.update(&bytes[..take]);
            self.current_len += take as u64;
            self.len += take as u64;
            bytes = &bytes[take..];

            if self.current_len == CHUNK {
                // A chunk is only sealed once it is full. Sealing on the *arrival* of
                // the next byte instead would leave a file of exactly one chunk with an
                // empty second level, which is the eMule/eDonkey disagreement in reverse.
                let digest = core::mem::replace(&mut self.current, Md4::new()).finalize();
                self.chunk_digests.extend_from_slice(&digest);
                self.current_len = 0;
            }
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The eMule-compatible hash: what a link found in the wild will carry.
    pub fn finish_hex(&self) -> String {
        self.finish(true)
    }

    /// The original eDonkey hash, which differs only for files that are an exact
    /// multiple of the chunk size.
    pub fn finish_hex_legacy(&self) -> String {
        self.finish(false)
    }

    fn finish(&self, emule: bool) -> String {
        // One chunk or less: no second level at all, just the bytes' own MD4.
        if self.chunk_digests.is_empty() {
            return hex(&self.current.clone().finalize());
        }

        let mut digests = self.chunk_digests.clone();
        // The current chunk is partial (or empty, when the file ended exactly on a
        // boundary). eMule hashes the empty remainder in; eDonkey stops.
        if self.current_len > 0 || emule {
            digests.extend_from_slice(&self.current.clone().finalize());
        }

        let mut top = Md4::new();
        top.update(&digests);
        hex(&top.finalize())
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// What an `ed2k://` link says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ed2kLink {
    pub filename: String,
    pub size: u64,
    /// Lowercase hex, 32 characters.
    pub hash: String,
    /// An ordinary `http(s)` URL some links carry, under `|s=…|`.
    ///
    /// This is the only part of an eDonkey link this product can act on by itself: with
    /// one, the file is fetched over HTTP like anything else and then checked against
    /// `hash`. Without one, the bytes only exist on the eDonkey network, and reaching
    /// that needs a client this does not ship.
    pub http_source: Option<String>,
    /// The AICH root hash, base32, from `|h=…|`. Recorded, not yet used: verifying it
    /// needs the full AICH tree, which a link does not carry.
    pub aich: Option<String>,
}

/// Anything an `ed2k://` URI can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ed2kUri {
    File(Box<Ed2kLink>),
    /// `ed2k://|server|host|port|/` — an address to connect a client to.
    Server {
        host: String,
        port: u16,
    },
    /// `ed2k://|serverlist|<url>|/` — a list of the above.
    ServerList(String),
}

/// Parse an `ed2k://` URI.
///
/// The format is pipe-delimited rather than a URL with a query, so it is split by hand.
/// Trailing fields after the hash are `key=value` extras in any order, and unknown ones
/// are ignored rather than treated as an error — clients have added their own for years
/// and a link carrying one is not malformed.
pub fn parse(uri: &str) -> Option<Ed2kUri> {
    let body = uri
        .trim()
        .strip_prefix("ed2k://")
        .or_else(|| uri.trim().strip_prefix("ED2K://"))
        .or_else(|| {
            let lower = uri.trim().to_ascii_lowercase();
            lower.starts_with("ed2k://").then(|| &uri.trim()[7..])
        })?;

    let parts: Vec<&str> = body.split('|').collect();
    // A leading empty field comes from the `|` that always follows the scheme.
    let parts: Vec<&str> = parts
        .into_iter()
        .skip_while(|p| p.is_empty())
        .collect::<Vec<_>>();

    match parts.first()?.to_ascii_lowercase().as_str() {
        "file" => {
            let filename = parts.get(1).filter(|s| !s.is_empty())?;
            let size: u64 = parts.get(2)?.trim().parse().ok()?;
            let hash = parts.get(3)?.trim().to_ascii_lowercase();
            if hash.len() != 32 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }

            let mut http_source = None;
            let mut aich = None;
            for extra in parts.iter().skip(4) {
                let Some((key, value)) = extra.split_once('=') else {
                    continue;
                };
                match key.trim().to_ascii_lowercase().as_str() {
                    // Only http(s). A link may name any source it likes; this one is
                    // going to be handed to `fetch`, so anything else is not a source.
                    "s" if value.starts_with("http://") || value.starts_with("https://") => {
                        http_source = Some(value.to_string());
                    }
                    "h" if !value.is_empty() => aich = Some(value.to_string()),
                    _ => {}
                }
            }

            Some(Ed2kUri::File(Box::new(Ed2kLink {
                filename: percent_decode(filename),
                size,
                hash,
                http_source,
                aich,
            })))
        }
        "server" => Some(Ed2kUri::Server {
            host: parts.get(1).filter(|s| !s.is_empty())?.to_string(),
            port: parts.get(2)?.trim().parse().ok()?,
        }),
        "serverlist" => Some(Ed2kUri::ServerList(
            parts.get(1).filter(|s| !s.is_empty())?.to_string(),
        )),
        _ => None,
    }
}

/// Undo percent-encoding in a filename. Names in these links are routinely CJK or
/// Cyrillic and are encoded by whatever produced the link.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // An undecodable name is better shown as it arrived than replaced with nothing.
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(bytes: &[u8]) -> String {
        let mut h = Ed2kHasher::new();
        h.update(bytes);
        h.finish_hex()
    }

    #[test]
    fn an_empty_file_hashes_to_md4_of_nothing() {
        // The published eD2k hash of a zero-length file.
        assert_eq!(hash_of(b""), "31d6cfe0d16ae931b73c59d7e0c089c0");
    }

    #[test]
    fn a_short_file_is_just_md4_of_its_bytes() {
        // Below one chunk there is no second level, so this must equal plain MD4.
        let mut md4 = Md4::new();
        md4.update(b"hello");
        assert_eq!(hash_of(b"hello"), hex(&md4.finalize()));
    }

    #[test]
    fn the_chunk_boundary_is_where_the_second_level_begins() {
        // One byte under a chunk: single-level. One byte over: two-level, and different.
        let under = vec![7u8; (CHUNK - 1) as usize];
        let over = vec![7u8; (CHUNK + 1) as usize];
        let mut md4 = Md4::new();
        md4.update(&under);
        assert_eq!(hash_of(&under), hex(&md4.finalize()));
        assert_ne!(hash_of(&over), hash_of(&under));
    }

    #[test]
    fn feeding_the_same_bytes_in_different_sized_writes_gives_one_hash() {
        // The read-back streams in whatever sizes the sink hands over; the hash cannot
        // depend on them. Sizes chosen to straddle the chunk boundary awkwardly.
        let data = vec![3u8; (CHUNK + 12_345) as usize];
        let whole = hash_of(&data);
        for step in [1usize, 1000, 65_536, (CHUNK - 1) as usize] {
            let mut h = Ed2kHasher::new();
            for piece in data.chunks(step) {
                h.update(piece);
            }
            assert_eq!(h.finish_hex(), whole, "differed at write size {step}");
        }
    }

    #[test]
    fn an_exact_multiple_of_the_chunk_size_is_where_emule_and_edonkey_disagree() {
        // The one case the two clients hash differently. eMule appends the empty chunk;
        // eDonkey does not. Links in the wild carry eMule's, which is what `finish_hex`
        // returns — but both must be reachable, and they must not be equal.
        let data = vec![1u8; CHUNK as usize];
        let mut h = Ed2kHasher::new();
        h.update(&data);
        assert_ne!(h.finish_hex(), h.finish_hex_legacy());
        // Below the boundary the question does not arise and the two agree.
        let mut short = Ed2kHasher::new();
        short.update(b"abc");
        assert_eq!(short.finish_hex(), short.finish_hex_legacy());
    }

    #[test]
    fn the_length_is_tracked_so_a_short_read_back_is_detectable() {
        let mut h = Ed2kHasher::new();
        h.update(&[0u8; 100]);
        assert_eq!(h.len(), 100);
        assert!(!h.is_empty());
    }

    #[test]
    fn a_plain_file_link_yields_name_size_and_hash() {
        let link = parse("ed2k://|file|Some.File.mkv|734003200|31d6cfe0d16ae931b73c59d7e0c089c0|/")
            .expect("parsed");
        let Ed2kUri::File(f) = link else {
            panic!("expected a file link")
        };
        assert_eq!(f.filename, "Some.File.mkv");
        assert_eq!(f.size, 734_003_200);
        assert_eq!(f.hash, "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(f.http_source, None);
    }

    #[test]
    fn an_http_source_is_lifted_out_when_the_link_carries_one() {
        let link =
            parse("ed2k://|file|X.mkv|123|31d6cfe0d16ae931b73c59d7e0c089c0|s=https://host/X.mkv|/")
                .expect("parsed");
        let Ed2kUri::File(f) = link else {
            panic!("expected a file link")
        };
        assert_eq!(f.http_source.as_deref(), Some("https://host/X.mkv"));
    }

    #[test]
    fn a_source_that_is_not_http_is_not_treated_as_one() {
        // `fetch` is where this ends up, so anything it cannot fetch is not a source.
        let link =
            parse("ed2k://|file|X.mkv|123|31d6cfe0d16ae931b73c59d7e0c089c0|s=ftp://host/X|/")
                .expect("parsed");
        let Ed2kUri::File(f) = link else {
            panic!("expected a file link")
        };
        assert_eq!(f.http_source, None);
    }

    #[test]
    fn extras_are_order_independent_and_unknown_ones_are_ignored() {
        let link = parse(
            "ed2k://|file|X.mkv|123|31d6cfe0d16ae931b73c59d7e0c089c0|p=aa,bb|h=ABC123|\
             s=http://host/X|zzz=nonsense|/",
        )
        .expect("parsed");
        let Ed2kUri::File(f) = link else {
            panic!("expected a file link")
        };
        assert_eq!(f.aich.as_deref(), Some("ABC123"));
        assert_eq!(f.http_source.as_deref(), Some("http://host/X"));
    }

    #[test]
    fn a_percent_encoded_name_comes_back_readable() {
        let link =
            parse("ed2k://|file|%E7%BE%A4%E4%BD%93.mkv|123|31d6cfe0d16ae931b73c59d7e0c089c0|/")
                .expect("parsed");
        let Ed2kUri::File(f) = link else {
            panic!("expected a file link")
        };
        assert_eq!(f.filename, "群体.mkv");
    }

    #[test]
    fn server_and_serverlist_links_are_recognised_as_themselves() {
        assert_eq!(
            parse("ed2k://|server|1.2.3.4|4661|/"),
            Some(Ed2kUri::Server {
                host: "1.2.3.4".into(),
                port: 4661
            })
        );
        assert_eq!(
            parse("ed2k://|serverlist|http://host/list.met|/"),
            Some(Ed2kUri::ServerList("http://host/list.met".into()))
        );
    }

    #[test]
    fn malformed_links_are_refused_rather_than_half_read() {
        for bad in [
            "https://example.com/a.mkv",
            "ed2k://|file|X.mkv|notanumber|31d6cfe0d16ae931b73c59d7e0c089c0|/",
            // A hash of the wrong length is the classic truncated-paste, and accepting
            // it would mean reporting "mismatch" on a correct file.
            "ed2k://|file|X.mkv|123|31d6cfe0|/",
            "ed2k://|file|X.mkv|123|zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz|/",
            "ed2k://|file||123|31d6cfe0d16ae931b73c59d7e0c089c0|/",
            "ed2k://|nonsense|a|b|/",
        ] {
            assert_eq!(parse(bad), None, "should have refused {bad}");
        }
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        assert!(parse("ED2K://|file|X.mkv|1|31d6cfe0d16ae931b73c59d7e0c089c0|/").is_some());
    }
}

/// Cross-checks against RHash, an independent eD2k implementation.
///
/// The tests above prove this hasher is self-consistent; these prove it agrees with
/// somebody else's reading of the spec, which is the only thing that makes a "verified"
/// badge worth anything. The expected values were produced by `rhash --ed2k` over files
/// of a known, deterministic byte pattern — see the generator in the test itself, so the
/// fixtures can be regenerated rather than trusted.
#[cfg(test)]
mod cross_check {
    use super::*;

    /// The same pattern the fixtures were generated from: non-uniform, so a chunking
    /// error cannot hide behind repeated bytes.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 37 + 11) & 0xFF) as u8).collect()
    }

    #[test]
    fn every_boundary_case_matches_rhash() {
        let cases: &[(usize, &str)] = &[
            (0, "31d6cfe0d16ae931b73c59d7e0c089c0"),
            (5, "20cf7d625f2a48932d7f18cb9e95c242"),
            // One byte under a chunk: single level.
            (CHUNK as usize - 1, "8974416c2b5aa27f0569576573576a74"),
            // Exactly one chunk: the case eMule and eDonkey disagree on. RHash follows
            // eMule, and so must this.
            (CHUNK as usize, "fc4c5c75efdbc5a1c70bdc218f4e4fbc"),
            // One byte over: two levels, with a one-byte second chunk.
            (CHUNK as usize + 1, "454ee4df695d37f000b3a65e28948a40"),
            // Two full chunks and a remainder.
            (
                CHUNK as usize * 2 + 12_345,
                "3960655158be6f4ea26bf35915e1987b",
            ),
        ];

        for (len, expected) in cases {
            let data = pattern(*len);
            let mut hasher = Ed2kHasher::new();
            hasher.update(&data);
            assert_eq!(
                &hasher.finish_hex(),
                expected,
                "eD2k hash of a {len}-byte file disagrees with rhash"
            );
            assert_eq!(hasher.len(), *len as u64);
        }
    }

    #[test]
    fn a_chunked_read_back_still_matches_rhash() {
        // The real caller streams the file in whatever sizes the sink produces, so the
        // agreement above has to survive being fed in pieces.
        let data = pattern(CHUNK as usize * 2 + 12_345);
        let mut hasher = Ed2kHasher::new();
        for piece in data.chunks(64 * 1024) {
            hasher.update(piece);
        }
        assert_eq!(hasher.finish_hex(), "3960655158be6f4ea26bf35915e1987b");
    }
}
