//! Download-manager links, and the peer-to-peer ones this cannot honour.
//!
//! # `thunder://`, `flashget://`, `qqdl://`
//!
//! None of these is a protocol. Each is an ordinary `http`/`https`/`ftp` URL, base64
//! encoded inside a fixed wrapper, invented so that a link on a page would open in one
//! particular download manager rather than the browser. Decoding one gives back the plain
//! URL, and from there it is an ordinary download like any other — so these are supported
//! completely, on every host, with nothing new in the download path.
//!
//! # `magnet:` and `.torrent`
//!
//! These are a different thing wearing the same coat. A magnet link names content by
//! hash; finding it means asking a tracker or the DHT for peers and then opening
//! connections to them over TCP or uTP. A browser tab cannot open a TCP socket at all —
//! not with a permission, not with a relay, not in an extension — so there is no swarm
//! for this to join. WebRTC is the one transport a page does get, and the handful of
//! peers that speak it are a different, much smaller swarm than the one a magnet link
//! points at: for an ordinary torrent there is nobody there.
//!
//! That is a limit of the tab, not of the machine it runs on — so the swarm is joined by
//! `dl-torrent`, a local process with real sockets, which serves each file inside a
//! torrent over HTTP with byte ranges. [`peer_link_refusal`] is what a host says when
//! that bridge is not running: it names the way through rather than closing the door.
//!
//! # `ed2k://`
//!
//! A third case, and it gets its own answer rather than being lumped in with the two
//! above. eDonkey is not BitTorrent and the bridge does not speak it, so pointing someone
//! at `dl-torrent` for an ed2k link would be a confidently wrong answer. What an ed2k link
//! *does* carry is the file's exact length and its eD2k hash, and some carry an ordinary
//! web address alongside — see [`crate::ed2k`], which parses them and computes that hash,
//! so a file obtained by any route can still be checked against the link that named it.

/// Decode a download-manager link into the plain URL inside it.
///
/// `None` when this is not one of those schemes, when the payload is not valid base64, or
/// when what comes out is not a URL worth handing to the downloader — a wrapper that
/// decodes to something other than `http`, `https` or `ftp` is a link this should refuse
/// rather than follow.
pub fn resolve_download_link(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();

    // Matched on the lowercased copy so the scheme is case-insensitive, but sliced out of
    // the original: base64 is case-sensitive and decoding the lowercased payload would
    // produce different bytes.
    let (prefix, wrapper) = SCHEMES
        .iter()
        .find(|(prefix, _)| lower.starts_with(prefix))?;
    let payload = &trimmed[prefix.len()..];

    // FlashGet appends `&n=<name>` after the payload on some links, and a trailing slash
    // is common on all three because they were written to be pasted into address bars.
    let payload = payload.split('&').next()?.trim_end_matches('/');
    let decoded = base64_decode(payload)?;
    let text = String::from_utf8(decoded).ok()?;
    let inner = wrapper.unwrap(&text);

    let candidate = inner.trim();
    let scheme = candidate.to_ascii_lowercase();
    if scheme.starts_with("http://")
        || scheme.starts_with("https://")
        || scheme.starts_with("ftp://")
    {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// The three schemes, and the sentinel each wraps its URL in.
const SCHEMES: &[(&str, Wrapper)] = &[
    ("thunder://", Wrapper::Thunder),
    ("flashget://", Wrapper::FlashGet),
    ("qqdl://", Wrapper::QqDl),
];

#[derive(Debug, Clone, Copy)]
enum Wrapper {
    Thunder,
    FlashGet,
    QqDl,
}

impl Wrapper {
    /// Strip the sentinel each scheme puts around the real URL.
    ///
    /// The sentinels exist only to make a decoded payload identifiable; they carry no
    /// information. A payload without the expected one is still returned, because several
    /// generators omit it and the URL inside is perfectly good.
    fn unwrap(self, text: &str) -> &str {
        match self {
            Wrapper::Thunder => text
                .strip_prefix("AA")
                .and_then(|t| t.strip_suffix("ZZ"))
                .unwrap_or(text),
            Wrapper::FlashGet => text
                .strip_prefix("[FLASHGET]")
                .and_then(|t| t.strip_suffix("[FLASHGET]"))
                .unwrap_or(text),
            Wrapper::QqDl => text,
        }
    }
}

/// Why a peer-to-peer link cannot be downloaded here, or `None` if this is not one.
///
/// Worded for the person who pasted it. It says what the obstacle is rather than that the
/// link is invalid, because the link is perfectly valid — it just names content that
/// lives in a place a browser tab has no way to reach.
pub fn peer_link_refusal(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();

    // eDonkey is a different network, and the bridge does not speak it — sending someone
    // to start `dl-torrent` for an ed2k link would be a wrong answer confidently given.
    // A link carrying an `|s=http://…|` source is handled before this is ever reached.
    if lower.starts_with("ed2k://") {
        return Some(
            "This is an eDonkey link. It names a file by its eD2k hash, and those bytes \
             live on the eDonkey network — a different network from BitTorrent, which the \
             bridge does not speak. Fetching it needs a client such as eMule or aMule. If \
             you obtain the file another way, paste its web address here and it will be \
             checked against the hash in this link."
                .to_string(),
        );
    }

    let what = if lower.starts_with("magnet:") {
        "A magnet link"
    } else if lower.ends_with(".torrent") || lower.contains(".torrent?") {
        "A .torrent file"
    } else {
        return None;
    };

    Some(format!(
        "{what} names content held by other people's computers, and the file is assembled \
         by connecting to them directly. A browser tab cannot open those connections — \
         that limit is the tab's, and no permission or relay lifts it. What can is the \
         bridge that ships with OpenDownloader: `dl-torrent` joins the swarm from this \
         machine and serves each file inside the torrent over HTTP, byte ranges and all, \
         so it downloads here like anything else. Start it with `npm start` and open this \
         link again."
    ))
}

/// Minimal base64 decoder.
///
/// Written here rather than pulled in as a dependency: this is the only base64 in the
/// crate, the alphabet is fixed, and a decoder is shorter than the argument for adding a
/// crate to a WebAssembly bundle. Accepts the URL-safe alphabet too, since links copied
/// out of a page are sometimes re-encoded that way, and ignores whitespace and padding.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut have = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);

    for c in input.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            '=' => break,
            c if c.is_whitespace() => continue,
            _ => return None,
        };
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    // Leftover bits must be zero padding, never dropped data: `have` is the count of bits
    // that did not complete a byte, and anything set in them means a truncated payload.
    if have > 0 && (bits & ((1 << have) - 1)) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper each scheme expects, encoded the way a real link is.
    fn encode(bytes: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = bytes.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - i * 6)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn a_thunder_link_decodes_to_the_url_inside_it() {
        let link = format!("thunder://{}", encode("AAhttps://example.com/a.mp4ZZ"));
        assert_eq!(
            resolve_download_link(&link).as_deref(),
            Some("https://example.com/a.mp4")
        );
    }

    #[test]
    fn a_flashget_link_decodes_and_its_trailing_name_is_ignored() {
        let link = format!(
            "flashget://{}&n=clip.mp4",
            encode("[FLASHGET]http://example.com/clip.mp4[FLASHGET]")
        );
        assert_eq!(
            resolve_download_link(&link).as_deref(),
            Some("http://example.com/clip.mp4")
        );
    }

    #[test]
    fn a_qqdl_link_has_no_sentinel_around_its_url() {
        let link = format!("qqdl://{}", encode("https://example.com/b.mp4"));
        assert_eq!(
            resolve_download_link(&link).as_deref(),
            Some("https://example.com/b.mp4")
        );
    }

    #[test]
    fn a_payload_without_the_sentinel_is_still_read() {
        // Several generators omit it, and the URL inside is perfectly good.
        let link = format!("thunder://{}", encode("https://example.com/c.mp4"));
        assert_eq!(
            resolve_download_link(&link).as_deref(),
            Some("https://example.com/c.mp4")
        );
    }

    #[test]
    fn the_scheme_match_is_case_insensitive() {
        let link = format!("THUNDER://{}", encode("AAhttps://example.com/d.mp4ZZ"));
        assert!(resolve_download_link(&link).is_some());
    }

    #[test]
    fn a_wrapper_hiding_something_that_is_not_a_url_is_refused() {
        // The whole point of decoding is to hand the result to the downloader. A payload
        // that decodes to a file path, or to another wrapper, is not that.
        let link = format!("thunder://{}", encode("AA/etc/passwdZZ"));
        assert_eq!(resolve_download_link(&link), None);
        let nested = format!("thunder://{}", encode("AAmagnet:?xt=urn:btih:abcZZ"));
        assert_eq!(resolve_download_link(&nested), None);
    }

    #[test]
    fn rubbish_in_the_payload_is_refused_rather_than_guessed_at() {
        assert_eq!(resolve_download_link("thunder://not base64 at all!!"), None);
    }

    #[test]
    fn an_ordinary_url_is_not_a_manager_link() {
        assert_eq!(resolve_download_link("https://example.com/a.mp4"), None);
    }

    #[test]
    fn magnet_and_torrent_links_are_recognised_and_explained() {
        for link in [
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a",
            "https://example.com/ubuntu.torrent",
            "https://example.com/x.torrent?token=1",
        ] {
            let why = peer_link_refusal(link).expect("explained");
            // The sentence has to name the way through, not merely the obstacle.
            assert!(
                why.contains("dl-torrent"),
                "should point at the bridge: {why}"
            );
            assert!(why.contains("connections"));
        }
    }

    #[test]
    fn an_edonkey_link_is_not_sent_to_the_bittorrent_bridge() {
        // The bridge speaks BitTorrent. Telling someone to start it for an ed2k link
        // would send them to do something that cannot possibly work.
        let why = peer_link_refusal("ed2k://|file|x.mkv|123|ABC|/").expect("explained");
        assert!(
            !why.contains("dl-torrent"),
            "must not point at the bridge: {why}"
        );
        assert!(why.contains("eMule") || why.contains("aMule"));
    }

    #[test]
    fn an_ordinary_link_gets_no_peer_refusal() {
        assert_eq!(peer_link_refusal("https://example.com/a.mp4"), None);
        assert_eq!(
            peer_link_refusal("https://example.com/torrential.mp4"),
            None
        );
    }
}
