//! Vimeo's JSON adaptive manifest — the format its player actually uses.
//!
//! # Why this exists
//!
//! A Vimeo clip can be unreachable through the player config: `player.vimeo.com` answers
//! `403` for videos restricted to their own page. The page still plays them, because it
//! never asks for a config — it fetches
//! `…/v2/playlist/av/primary/playlist.json` and drives the player from that.
//!
//! It is DASH-shaped but JSON, and it is nobody's standard, so nothing in this crate
//! could read it and such a video had no path at all.
//!
//! # The shape
//!
//! ```json
//! { "clip_id": "…", "base_url": "../../../remux/avf/",
//!   "video": [ { "id": "…", "base_url": "<id>/", "mime_type": "video/mp4",
//!                "codecs": "avc1.640028", "bitrate": 6082000,
//!                "width": 1920, "height": 1080, "duration": 147.1,
//!                "init_segment": "<base64>",
//!                "segments": [ { "start": 0, "end": 7.8,
//!                                "url": "segment.m4s?…", "size": 176142 } ] } ],
//!   "audio": [ … same, with `channels` and `audio_primary` ] }
//! ```
//!
//! Two details decide how the rest of the product can use it.
//!
//! **The init segment is inline base64, not a URL.** So this is not expressible as an
//! HLS playlist without inventing a URL for it, and synthesising one was the first plan
//! and the wrong one.
//!
//! **Every segment states its `size`.** That is what makes a rendition usable as an
//! ordinary byte stream: the init and the segments concatenated *are* a fragmented MP4,
//! and knowing each length up front means any byte range can be served by fetching only
//! the segments it covers. The merger already reads its inputs that way, so the muxing
//! that joins picture to sound needs no new code — only a reader that maps offsets onto
//! segments.

use serde::Serialize;

use super::super::hls::resolve_url;

/// One segment of a rendition, with the byte span it occupies in the whole stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdaptiveSegment {
    pub url: String,
    /// Bytes, as the manifest states them. The offsets below are built from these.
    pub size: u64,
    /// Where this segment starts in the concatenated stream, init segment included.
    pub offset: u64,
    pub duration_ms: u64,
}

/// One video or audio rendition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdaptiveRendition {
    pub id: String,
    pub mime: String,
    pub codecs: Option<String>,
    pub bitrate: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    /// The init segment, base64 as the manifest gives it. Not a URL.
    pub init_base64: String,
    pub segments: Vec<AdaptiveSegment>,
    /// Init plus every segment. What a reader over this rendition will serve.
    pub total_bytes: u64,
}

/// A parsed manifest: the video renditions best first, and the audio ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdaptivePlaylist {
    pub clip_id: String,
    pub video: Vec<AdaptiveRendition>,
    pub audio: Vec<AdaptiveRendition>,
}

/// Whether a URL is one of these manifests.
///
/// Matched on the path rather than the host: the signed hosts vary by CDN and region
/// (`vod-adaptive-ak.vimeocdn.com`, `skyfire.vimeocdn.com`), while the path shape is
/// Vimeo's own and stable.
pub fn is_adaptive_playlist(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with("/playlist.json") && path.contains("/v2/playlist/")
}

/// Resolve a relative *directory*, keeping the trailing slash.
///
/// [`resolve_url`] is written for HLS, where what is being resolved is always a file, so
/// it returns a path with no trailing slash. That is right there and wrong here: these
/// bases nest — the manifest's `../../../remux/avf/`, then the rendition's `<id>/`, then
/// the segment — and feeding a slash-less directory back in as a base makes the next
/// resolve read its last segment as a filename and drop it.
///
/// It cost `…/v2/remux/avf/<id>/segment.m4s` two directories and produced
/// `…/v2/remux/segment.m4s`, which 404s. Keeping the slash is the whole fix; changing
/// `resolve_url` itself would alter what every HLS playlist resolves to.
fn resolve_dir(base: &str, rel: &str) -> String {
    let mut out = resolve_url(base, rel);
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn seconds_to_ms(v: Option<f64>) -> Option<u64> {
    v.filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * 1000.0).round() as u64)
}

fn rendition(
    item: &serde_json::Value,
    stream_base: &str,
    init_len: u64,
) -> Option<AdaptiveRendition> {
    let init_base64 = item.get("init_segment")?.as_str()?.to_string();
    if init_base64.is_empty() {
        return None;
    }
    let base = item.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
    let rendition_base = resolve_dir(stream_base, base);

    // Offsets run over the concatenated stream, and the init segment is its first bytes,
    // so every segment sits `init_len` further along than its own sizes suggest.
    let mut offset = init_len;
    let mut segments = Vec::new();
    for seg in item.get("segments")?.as_array()? {
        let url = resolve_url(&rendition_base, seg.get("url")?.as_str()?);
        let size = seg.get("size").and_then(serde_json::Value::as_u64)?;
        let start = seg.get("start").and_then(serde_json::Value::as_f64);
        let end = seg.get("end").and_then(serde_json::Value::as_f64);
        let duration_ms = match (start, end) {
            (Some(a), Some(b)) if b > a => ((b - a) * 1000.0).round() as u64,
            _ => 0,
        };
        segments.push(AdaptiveSegment {
            url,
            size,
            offset,
            duration_ms,
        });
        offset += size;
    }
    if segments.is_empty() {
        return None;
    }

    Some(AdaptiveRendition {
        id: item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        mime: item
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or("video/mp4")
            .to_string(),
        codecs: item
            .get("codecs")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        bitrate: item.get("bitrate").and_then(serde_json::Value::as_u64),
        width: item
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32),
        height: item
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32),
        duration_ms: seconds_to_ms(item.get("duration").and_then(serde_json::Value::as_f64)),
        init_base64,
        total_bytes: offset,
        segments,
    })
}

/// Read a manifest, resolving every segment URL against the manifest's own address.
///
/// `None` when the document is not one of these, or names no usable rendition — a
/// rendition with no segments, or none with an init segment, cannot be downloaded and is
/// better dropped here than offered and failed later.
pub fn parse(json: &str, playlist_url: &str) -> Option<AdaptivePlaylist> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    // Two nested relative bases: the manifest's own, then each rendition's.
    let stream_base = resolve_dir(
        playlist_url,
        root.get("base_url").and_then(|v| v.as_str()).unwrap_or(""),
    );

    let renditions = |key: &str| -> Vec<AdaptiveRendition> {
        root.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        // The decoded length of the init segment, needed before the
                        // segment offsets can be laid out. Base64 with padding is four
                        // characters per three bytes.
                        let init = item.get("init_segment")?.as_str()?;
                        let init_len = base64_decoded_len(init)?;
                        rendition(item, &stream_base, init_len)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut video = renditions("video");
    let mut audio = renditions("audio");
    if video.is_empty() && audio.is_empty() {
        return None;
    }
    // Best first, by pixels then bitrate — the order a picker shows.
    video.sort_by_key(|r| {
        std::cmp::Reverse((
            r.height.unwrap_or(0) as u64 * r.width.unwrap_or(0) as u64,
            r.bitrate.unwrap_or(0),
        ))
    });
    audio.sort_by_key(|r| std::cmp::Reverse(r.bitrate.unwrap_or(0)));

    Some(AdaptivePlaylist {
        clip_id: root
            .get("clip_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        video,
        audio,
    })
}

/// How many bytes a base64 string decodes to, without decoding it.
///
/// The offsets have to be laid out before anything is fetched, and the init segment is
/// the first thing in the stream, so its length is needed up front. Returns `None` for a
/// string that is not valid base64 length, since a wrong length would silently shift
/// every offset after it.
fn base64_decoded_len(s: &str) -> Option<u64> {
    let len = s.len() as u64;
    if len == 0 || !len.is_multiple_of(4) {
        return None;
    }
    let padding = s.bytes().rev().take_while(|b| *b == b'=').count() as u64;
    if padding > 2 {
        return None;
    }
    Some(len / 4 * 3 - padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a real manifest, trimmed. `AAAA` decodes to three bytes, which is
    /// enough to check that offsets start after the init segment.
    const MANIFEST: &str = r#"{
      "clip_id": "742a11bb",
      "base_url": "../../../remux/avf/",
      "video": [
        {"id":"small","base_url":"small/","mime_type":"video/mp4","codecs":"avc1.42C01E",
         "bitrate":418000,"width":480,"height":270,"duration":147.1,
         "init_segment":"AAAA",
         "segments":[{"start":0,"end":8,"url":"segment.m4s?sid=1","size":100},
                     {"start":8,"end":16,"url":"segment.m4s?sid=2","size":200}]},
        {"id":"big","base_url":"big/","mime_type":"video/mp4","codecs":"avc1.640028",
         "bitrate":6082000,"width":1920,"height":1080,"duration":147.1,
         "init_segment":"AAAA",
         "segments":[{"start":0,"end":8,"url":"segment.m4s?sid=1","size":900}]}
      ],
      "audio": [
        {"id":"aud","base_url":"aud/","mime_type":"audio/mp4","codecs":"mp4a.40.2",
         "bitrate":255000,"init_segment":"AAAA",
         "segments":[{"start":0,"end":8,"url":"segment.m4s?sid=1","size":50}]}
      ]}"#;

    const URL: &str = "https://cdn.test/sig/clip/v2/playlist/av/primary/playlist.json?pathsig=abc";

    #[test]
    fn it_recognises_its_own_manifests_and_nothing_else() {
        assert!(is_adaptive_playlist(URL));
        assert!(is_adaptive_playlist(
            "https://cdn.test/a/v2/playlist/av/x/playlist.json"
        ));
        // A JSON file that is not one of these, and an HLS playlist.
        assert!(!is_adaptive_playlist("https://cdn.test/config.json"));
        assert!(!is_adaptive_playlist(
            "https://cdn.test/v2/playlist/av/x/playlist.m3u8"
        ));
    }

    /// The bases nest, and each one must survive being used as a base again.
    ///
    /// This is the regression that mattered: resolving a directory and feeding it back in
    /// dropped its last segment, turning `…/v2/remux/avf/<id>/segment.m4s` into
    /// `…/v2/remux/segment.m4s`, which the CDN answers with 404. Checked against the
    /// path a real player requested.
    #[test]
    fn nested_relative_bases_resolve_to_the_path_the_player_uses() {
        let p = parse(MANIFEST, URL).expect("parses");
        assert_eq!(
            p.video[0].segments[0].url,
            "https://cdn.test/sig/clip/v2/remux/avf/big/segment.m4s?sid=1"
        );
        assert_eq!(
            p.audio[0].segments[0].url,
            "https://cdn.test/sig/clip/v2/remux/avf/aud/segment.m4s?sid=1"
        );
    }

    #[test]
    fn renditions_are_ordered_best_first() {
        let p = parse(MANIFEST, URL).expect("parses");
        assert_eq!(p.video.len(), 2);
        assert_eq!(p.video[0].height, Some(1080), "the largest comes first");
        assert_eq!(p.video[1].height, Some(270));
    }

    /// Offsets describe the concatenated stream, and it begins with the init segment.
    #[test]
    fn offsets_run_over_the_whole_stream_starting_after_the_init_segment() {
        let p = parse(MANIFEST, URL).expect("parses");
        let small = &p.video[1];
        // "AAAA" is four base64 characters with no padding: three bytes.
        assert_eq!(
            small.segments[0].offset, 3,
            "the first segment follows the init"
        );
        assert_eq!(
            small.segments[1].offset, 103,
            "and the next follows the first"
        );
        assert_eq!(small.total_bytes, 3 + 100 + 200);
        assert_eq!(small.segments[0].duration_ms, 8000);
    }

    #[test]
    fn a_rendition_that_cannot_be_downloaded_is_dropped_rather_than_offered() {
        // No init segment, and no segments: each is unusable on its own terms.
        let broken = MANIFEST.replace(r#""init_segment":"AAAA","#, "").replace(
            r#""segments":[{"start":0,"end":8,"url":"segment.m4s?sid=1","size":900}]"#,
            r#""segments":[]"#,
        );
        assert!(parse(&broken, URL).is_none(), "nothing usable is left");
        // A document that is not a manifest at all.
        assert!(parse("{\"video\":\"nope\"}", URL).is_none());
        assert!(parse("not json", URL).is_none());
    }

    #[test]
    fn a_base64_length_that_cannot_be_right_is_refused() {
        // Offsets are laid out from the init length, so a wrong length shifts every
        // segment after it — better to drop the rendition than to place it wrongly.
        assert_eq!(base64_decoded_len("AAAA"), Some(3));
        assert_eq!(base64_decoded_len("AAA="), Some(2));
        assert_eq!(base64_decoded_len("AA=="), Some(1));
        assert_eq!(base64_decoded_len("AAAAA"), None, "not a multiple of four");
        assert_eq!(base64_decoded_len(""), None);
        assert_eq!(base64_decoded_len("A==="), None, "three pad characters");
    }
}
