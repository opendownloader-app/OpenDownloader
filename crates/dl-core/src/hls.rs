//! HLS playlist handling.
//!
//! Parsing itself is delegated to `m3u8-rs` (mature, pure, no I/O). What lives here is
//! everything that turns a parsed playlist into a download job: URL resolution against
//! the playlist's own location, variant selection, byte-range translation, and the
//! refusal of encrypted streams.

use crate::plan::ByteRange;
use crate::policy;

/// One rendition listed in a master playlist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<(u64, u64)>,
    pub codecs: Option<String>,
    /// `AUDIO="..."` — the [`Rendition::group_id`] of the audio tracks that go with this
    /// variant. `None` when the audio is muxed into the variant itself.
    pub audio_group: Option<String>,
    /// `SUBTITLES="..."` — the [`Rendition::group_id`] of the subtitle tracks that go
    /// with this variant.
    pub subtitles_group: Option<String>,
}

/// One `#EXT-X-MEDIA` entry: an alternate audio or subtitle track.
///
/// A rendition belongs to a group, and a [`Variant`] names the group it wants via
/// `audio_group` / `subtitles_group`. That indirection is the whole reason the group id
/// is carried here: a caller picks a variant first, then filters renditions by group.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rendition {
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    /// Resolved media playlist URL. `None` means the rendition has no playlist of its
    /// own — its content is muxed into the variant, so there is nothing to fetch
    /// separately.
    pub url: Option<String>,
    pub default: bool,
    pub autoselect: bool,
    pub forced: bool,
}

/// A master playlist: the watchable variants plus the alternate tracks they can pair
/// with. I-frame-only streams, video alternates and `CLOSED-CAPTIONS` entries are left
/// out — none of them is something a download can fetch as a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
    pub subtitles: Vec<Rendition>,
}

/// One fetchable piece of a media playlist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub url: String,
    /// Present when the segment is a byte range of a larger resource
    /// (`#EXT-X-BYTERANGE`), in which case `url` is shared with its neighbours.
    pub byte_range: Option<ByteRange>,
    pub duration_ms: u64,
}

/// A media playlist expanded into everything needed to fetch it.
///
/// `init` carries `#EXT-X-MAP` when present. That matters because an fMP4 (CMAF) HLS
/// variant is *already* fragmented MP4 — its init segment plus its media segments
/// concatenated is a playable file with no remuxing at all, so the engine can skip
/// `dl-container` entirely for those streams.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaStream {
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    pub total_duration_ms: u64,
    /// True while the playlist is still growing (no `#EXT-X-ENDLIST`) — a live stream,
    /// which has no meaningful "complete" state.
    pub is_live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaStream),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsError {
    /// The playlist declares encryption. We refuse these by policy and never fetch keys.
    Encrypted,
    Malformed(String),
    Empty,
}

impl core::fmt::Display for HlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HlsError::Encrypted => f.write_str(
                "this stream is encrypted; opendownloader does not download protected streams",
            ),
            HlsError::Malformed(m) => write!(f, "malformed playlist: {m}"),
            HlsError::Empty => f.write_str("playlist contains no segments"),
        }
    }
}

/// Parse a playlist and resolve every URL inside it against `base_url`.
pub fn parse_playlist(text: &str, base_url: &str) -> Result<Playlist, HlsError> {
    if policy::refuse_encrypted(text) {
        return Err(HlsError::Encrypted);
    }

    let (_, parsed) = m3u8_rs::parse_playlist(text.as_bytes())
        .map_err(|e| HlsError::Malformed(format!("{e:?}")))?;

    match parsed {
        m3u8_rs::Playlist::MasterPlaylist(m) => {
            let variants: Vec<Variant> = m
                .variants
                .into_iter()
                // I-frame-only renditions are trick-play tracks, not watchable video.
                .filter(|v| !v.is_i_frame)
                .map(|v| Variant {
                    url: resolve_url(base_url, &v.uri),
                    bandwidth: v.bandwidth,
                    resolution: v.resolution.map(|r| (r.width, r.height)),
                    codecs: v.codecs,
                    audio_group: v.audio,
                    subtitles_group: v.subtitles,
                })
                .collect();
            if variants.is_empty() {
                return Err(HlsError::Empty);
            }

            let mut audio = Vec::new();
            let mut subtitles = Vec::new();
            for alt in m.alternatives {
                let bucket = match alt.media_type {
                    m3u8_rs::AlternativeMediaType::Audio => &mut audio,
                    m3u8_rs::AlternativeMediaType::Subtitles => &mut subtitles,
                    // Video alternates are camera angles muxed into variants, and closed
                    // captions live inside the video stream itself; neither is a
                    // separately fetchable resource.
                    _ => continue,
                };
                bucket.push(Rendition {
                    group_id: alt.group_id,
                    name: alt.name,
                    language: alt.language,
                    url: alt.uri.map(|u| resolve_url(base_url, &u)),
                    default: alt.default,
                    autoselect: alt.autoselect,
                    forced: alt.forced,
                });
            }

            Ok(Playlist::Master(MasterPlaylist {
                variants,
                audio,
                subtitles,
            }))
        }
        m3u8_rs::Playlist::MediaPlaylist(p) => {
            if p.segments.is_empty() {
                return Err(HlsError::Empty);
            }
            // A sub-range with no explicit offset continues from the end of the previous
            // sub-range of the same resource, so this cursor has to be tracked across
            // segments rather than computed per segment.
            let mut range_cursor: u64 = 0;
            let mut segments = Vec::with_capacity(p.segments.len());
            let mut total_duration_ms: u64 = 0;
            let mut init: Option<Segment> = None;

            for s in &p.segments {
                if init.is_none() {
                    if let Some(map) = &s.map {
                        init = Some(Segment {
                            url: resolve_url(base_url, &map.uri),
                            byte_range: map.byte_range.as_ref().map(|b| {
                                let start = b.offset.unwrap_or(0);
                                ByteRange {
                                    start,
                                    end: start + b.length.saturating_sub(1),
                                }
                            }),
                            duration_ms: 0,
                        });
                    }
                }

                let byte_range = s.byte_range.as_ref().map(|b| {
                    let start = b.offset.unwrap_or(range_cursor);
                    let end = start + b.length.saturating_sub(1);
                    range_cursor = end + 1;
                    ByteRange { start, end }
                });
                if s.byte_range.is_none() {
                    range_cursor = 0;
                }

                let duration_ms = (f64::from(s.duration) * 1000.0).round().max(0.0) as u64;
                total_duration_ms += duration_ms;
                segments.push(Segment {
                    url: resolve_url(base_url, &s.uri),
                    byte_range,
                    duration_ms,
                });
            }

            Ok(Playlist::Media(MediaStream {
                init,
                segments,
                total_duration_ms,
                is_live: !p.end_list,
            }))
        }
    }
}

/// Pick a rendition. `prefer_highest` selects the largest bandwidth, otherwise the
/// smallest — the two choices a download UI actually offers.
pub fn select_variant(variants: &[Variant], prefer_highest: bool) -> Option<&Variant> {
    if prefer_highest {
        variants.iter().max_by_key(|v| v.bandwidth)
    } else {
        variants.iter().min_by_key(|v| v.bandwidth)
    }
}

/// Resolve a possibly-relative URL against the playlist's own URL.
///
/// Hand-rolled for the same reason as `policy::host_of`: the `url` crate drags in `idna`
/// and its Unicode tables, which is a lot of wasm for RFC 3986's three easy cases.
pub fn resolve_url(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.is_empty() {
        return base.to_string();
    }
    // Absolute: has a scheme.
    if let Some(i) = rel.find("://") {
        if rel[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return rel.to_string();
        }
    }
    // Scheme-relative: //host/path
    if let Some(rest) = rel.strip_prefix("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }

    let (origin, base_dir) = split_base(base);
    // Root-relative: /path
    if let Some(rest) = rel.strip_prefix('/') {
        return format!("{origin}/{rest}");
    }
    // Path-relative, honouring ./ and ../
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("{origin}/{}", parts.join("/"))
}

/// Split a URL into its origin (`scheme://authority`) and the directory part of its path.
fn split_base(base: &str) -> (String, String) {
    let (scheme, rest) = match base.find("://") {
        Some(i) => (&base[..i], &base[i + 3..]),
        None => ("https", base),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let dir = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    (format!("{scheme}://{authority}"), dir.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n360p.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n720p/index.m3u8\n";

    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
#EXTINF:4.000,\nseg0.ts\n#EXTINF:3.500,\nseg1.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn parses_master_and_resolves_relative_urls() {
        let Playlist::Master(m) = parse_playlist(MASTER, "https://cdn.x/hls/master.m3u8").unwrap()
        else {
            panic!("expected master")
        };
        let v = m.variants;
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].url, "https://cdn.x/hls/360p.m3u8");
        assert_eq!(v[1].url, "https://cdn.x/hls/720p/index.m3u8");
        assert_eq!(v[1].bandwidth, 2_400_000);
        assert_eq!(v[0].resolution, Some((640, 360)));
        assert_eq!(v[0].codecs.as_deref(), Some("avc1.4d401e,mp4a.40.2"));
    }

    #[test]
    fn parses_media_segments_with_durations() {
        let Playlist::Media(s) = parse_playlist(MEDIA, "https://cdn.x/hls/360p.m3u8").unwrap()
        else {
            panic!("expected media")
        };
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].url, "https://cdn.x/hls/seg0.ts");
        assert_eq!(s.segments[0].duration_ms, 4000);
        assert_eq!(s.segments[1].duration_ms, 3500);
        assert_eq!(s.total_duration_ms, 7500);
        assert!(!s.is_live);
        assert!(s.init.is_none());
    }

    #[test]
    fn detects_a_live_playlist_by_the_absent_endlist() {
        let live = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.ts\n";
        let Playlist::Media(s) = parse_playlist(live, "https://cdn.x/a.m3u8").unwrap() else {
            panic!()
        };
        assert!(s.is_live);
    }

    #[test]
    fn selects_highest_and_lowest_bandwidth_variants() {
        let Playlist::Master(m) = parse_playlist(MASTER, "https://cdn.x/m.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(
            select_variant(&m.variants, true).unwrap().bandwidth,
            2_400_000
        );
        assert_eq!(
            select_variant(&m.variants, false).unwrap().bandwidth,
            800_000
        );
    }

    #[test]
    fn encrypted_playlists_are_rejected() {
        let enc = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:4,\nseg0.ts\n";
        assert_eq!(
            parse_playlist(enc, "https://cdn.x/a.m3u8"),
            Err(HlsError::Encrypted)
        );
    }

    #[test]
    fn captures_the_fmp4_init_segment() {
        let fmp4 = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg0.m4s\n#EXT-X-ENDLIST\n";
        let Playlist::Media(s) = parse_playlist(fmp4, "https://cdn.x/v/index.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(s.init.unwrap().url, "https://cdn.x/v/init.mp4");
        assert_eq!(s.segments[0].url, "https://cdn.x/v/seg0.m4s");
    }

    #[test]
    fn byte_range_segments_chain_from_the_previous_offset() {
        let br = "#EXTM3U\n\
#EXT-X-BYTERANGE:1000@0\n#EXTINF:4.0,\nall.ts\n\
#EXT-X-BYTERANGE:500\n#EXTINF:4.0,\nall.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(s) = parse_playlist(br, "https://cdn.x/a.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(
            s.segments[0].byte_range,
            Some(ByteRange { start: 0, end: 999 })
        );
        // No explicit offset: must continue exactly where the previous one ended.
        assert_eq!(
            s.segments[1].byte_range,
            Some(ByteRange {
                start: 1000,
                end: 1499
            })
        );
    }

    #[test]
    fn resolves_absolute_root_relative_and_dotted_urls() {
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "https://other.y/c.ts"),
            "https://other.y/c.ts"
        );
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "/root.ts"),
            "https://cdn.x/root.ts"
        );
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "sub/c.ts"),
            "https://cdn.x/a/sub/c.ts"
        );
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "../up.ts"),
            "https://cdn.x/up.ts"
        );
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "./same.ts"),
            "https://cdn.x/a/same.ts"
        );
        assert_eq!(
            resolve_url("https://cdn.x/a/b.m3u8", "//other.y/c.ts"),
            "https://other.y/c.ts"
        );
    }

    #[test]
    fn i_frame_only_renditions_are_dropped() {
        let m = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000\nmain.m3u8\n\
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=90000,URI=\"iframe.m3u8\"\n";
        let Playlist::Master(m) = parse_playlist(m, "https://cdn.x/a.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(m.variants.len(), 1);
        assert_eq!(m.variants[0].url, "https://cdn.x/main.m3u8");
    }

    const MASTER_WITH_ALTERNATES: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",FORCED=NO,URI=\"subs/en.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,AUDIO=\"aud\",SUBTITLES=\"subs\"\n720p.m3u8\n";

    #[test]
    fn master_alternates_are_split_into_audio_and_subtitles() {
        let Playlist::Master(m) =
            parse_playlist(MASTER_WITH_ALTERNATES, "https://cdn.x/hls/master.m3u8").unwrap()
        else {
            panic!("expected master")
        };
        assert_eq!(
            m.audio,
            vec![Rendition {
                group_id: "aud".into(),
                name: "English".into(),
                language: Some("en".into()),
                url: Some("https://cdn.x/hls/audio/en.m3u8".into()),
                default: true,
                autoselect: true,
                forced: false,
            }]
        );
        assert_eq!(
            m.subtitles,
            vec![Rendition {
                group_id: "subs".into(),
                name: "English".into(),
                language: Some("en".into()),
                url: Some("https://cdn.x/hls/subs/en.m3u8".into()),
                default: false,
                autoselect: false,
                forced: false,
            }]
        );
    }

    #[test]
    fn variants_name_the_rendition_groups_they_pair_with() {
        let Playlist::Master(m) =
            parse_playlist(MASTER_WITH_ALTERNATES, "https://cdn.x/hls/master.m3u8").unwrap()
        else {
            panic!()
        };
        assert_eq!(m.variants[0].audio_group.as_deref(), Some("aud"));
        assert_eq!(m.variants[0].subtitles_group.as_deref(), Some("subs"));
        // The group ids are what lets a caller walk from a variant to its tracks.
        let for_variant: Vec<&Rendition> = m
            .audio
            .iter()
            .filter(|r| Some(&r.group_id) == m.variants[0].audio_group.as_ref())
            .collect();
        assert_eq!(for_variant.len(), 1);
    }

    #[test]
    fn an_alternate_without_a_uri_is_muxed_and_has_no_url() {
        let m = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Main\",DEFAULT=YES\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"aud\"\nmain.m3u8\n";
        let Playlist::Master(m) = parse_playlist(m, "https://cdn.x/a.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(m.audio.len(), 1);
        assert_eq!(m.audio[0].url, None);
        assert_eq!(m.audio[0].language, None);
        assert!(m.audio[0].default);
    }

    #[test]
    fn closed_caption_alternates_are_ignored() {
        let m = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",INSTREAM-ID=\"CC1\"\n\
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"cam\",NAME=\"Wide\",URI=\"wide.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,CLOSED-CAPTIONS=\"cc\",VIDEO=\"cam\"\nmain.m3u8\n";
        let Playlist::Master(m) = parse_playlist(m, "https://cdn.x/a.m3u8").unwrap() else {
            panic!()
        };
        assert!(m.audio.is_empty());
        assert!(m.subtitles.is_empty());
        assert_eq!(m.variants.len(), 1);
    }

    #[test]
    fn a_master_without_alternates_has_empty_rendition_lists() {
        let Playlist::Master(m) = parse_playlist(MASTER, "https://cdn.x/m.m3u8").unwrap() else {
            panic!()
        };
        assert!(m.audio.is_empty());
        assert!(m.subtitles.is_empty());
        assert_eq!(m.variants[0].audio_group, None);
        assert_eq!(m.variants[0].subtitles_group, None);
    }

    #[test]
    fn master_serializes_with_named_fields() {
        let Playlist::Master(m) = parse_playlist(MASTER, "https://cdn.x/m.m3u8").unwrap() else {
            panic!()
        };
        let json = serde_json::to_string(&Playlist::Master(m)).unwrap();
        assert!(json.starts_with("{\"Master\":{\"variants\":["));
        assert!(json.contains("\"audio\":[]"));
        assert!(json.contains("\"subtitles\":[]"));
    }
}
