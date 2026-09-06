//! Vimeo, via the player's own `config` endpoint.
//!
//! Verified live on 2026-09-05 against a captured
//! `https://player.vimeo.com/video/76979871/config` response, which is the shape every
//! assertion below is written against.
//!
//! # Why this file exists at all
//!
//! [`super::generic`] already claims `vimeo.com`, and on most sites reading the page is
//! enough. Not here: Vimeo publishes an `og:video`, but it declares it
//! `og:video:type: text/html` because it points at the *player page* rather than a file.
//! The generic reader honours that declaration and skips the whole `og:` family, so
//! without this module a Vimeo link yields nothing at all. One request to the player's
//! own config is what turns that into a list of renditions.
//!
//! # Progressive files are the good case, and they are increasingly absent
//!
//! `request.files.progressive[]` holds plain MP4 URLs — one fetch, no playlist, no
//! merge. The captured config has **an empty progressive array**: modern Vimeo serves
//! adaptive only, and the progressive ladder survives mostly on older uploads. So this
//! module treats progressive as a bonus rather than the plan, and always has the HLS
//! master to fall back to.
//!
//! # HLS, not DASH
//!
//! `request.files.dash` sits right next to `hls` and describes the same renditions. HLS
//! wins because this codebase has a tested playlist parser and segment planner in
//! [`crate::hls`] and no DASH manifest parser whatsoever — offering a `.mpd` would be
//! offering a download that cannot start.
//!
//! # What this module deliberately does not decide
//!
//! The captured config's HLS master is DRM-protected (its path carries `/drm/cbcs,…`,
//! and `request.drm` lists Widevine, PlayReady and FairPlay license URLs). Refusing that
//! is not this module's job and cannot be: an extractor performs no I/O, so it cannot
//! know what the playlist actually contains. [`crate::hls`] refuses an encrypted
//! playlist when it reads one, which is the place that has the evidence.

use serde_json::Value;

use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, Request, SiteError, Step,
    Stream, StreamKind, SubtitleTrack, VideoChoice,
};

/// Vimeo's CDNs serve media to Vimeo's own pages. The Referer costs nothing when it is
/// not checked and is the whole difference between 200 and 403 when it is.
const REFERER: &str = "https://vimeo.com/";

/// Hosts this extractor claims.
///
/// `player.vimeo.com` is already covered by the boundary-aware suffix match on
/// `vimeo.com`. It is listed anyway because embedded players are the common case, and a
/// reader scanning this list should not have to re-derive the suffix rule to see that
/// they are handled.
const CLAIMED: &[&str] = &["vimeo.com", "player.vimeo.com", "vimeocdn.com"];

pub fn matches(host: &str) -> bool {
    CLAIMED.iter().any(|d| host_is(host, d))
}

#[derive(Debug, Default)]
pub struct Vimeo {
    /// Kept from `start` so a config with no `video.title` still names its file after
    /// something the user can recognise.
    video_id: Option<String>,
}

impl Vimeo {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Vimeo {
    fn site(&self) -> &'static str {
        "Vimeo"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        let id = video_id(url).ok_or_else(|| {
            SiteError::Unavailable(
                "this Vimeo link does not name a video — open the video itself and try again"
                    .into(),
            )
        })?;
        let request = config_request(&id);
        self.video_id = Some(id);
        Ok(Step::Need(Need::Fetch(vec![request])))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().ok_or_else(shape)?;
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        parse_config(&root, self.video_id.as_deref()).map(Step::Done)
    }
}

fn shape() -> SiteError {
    SiteError::Shape("Vimeo".into())
}

/// The one request this extractor makes.
///
/// The id has already been validated as digits, so nothing in it can escape the path.
fn config_request(video_id: &str) -> Request {
    Request::get(format!("https://player.vimeo.com/video/{video_id}/config"))
        .with_header("Referer", REFERER)
}

/// Pull the numeric video id out of any shape of Vimeo link.
///
/// The explicit markers are tried before the bare first segment, because a group URL
/// (`/groups/<g>/videos/<id>`) puts a name where a plain link puts the id, and a channel
/// URL puts two.
pub fn video_id(url: &str) -> Option<String> {
    let path = path_of(url);

    // `player.vimeo.com/video/<id>` and `vimeo.com/groups/<g>/videos/<id>`.
    for marker in ["/video/", "/videos/"] {
        if let Some(rest) = path.split_once(marker).map(|(_, r)| r) {
            if let Some(id) = first_segment(rest).and_then(numeric_id) {
                return Some(id);
            }
        }
    }

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // `vimeo.com/channels/<name>/<id>`.
    if segments.first() == Some(&"channels") {
        return segments.get(2).copied().and_then(numeric_id);
    }

    // `vimeo.com/<id>`, optionally followed by an unlisted hash the config does not need.
    segments.first().copied().and_then(numeric_id)
}

/// The path of a URL, scheme and authority and query removed.
fn path_of(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let rest = match after_scheme.find(['/', '?', '#']) {
        Some(i) => &after_scheme[i..],
        None => "",
    };
    rest.split(['?', '#']).next().unwrap_or("")
}

fn first_segment(s: &str) -> Option<&str> {
    let seg = s.trim_start_matches('/').split(['/', '?', '#']).next()?;
    (!seg.is_empty()).then_some(seg)
}

/// Vimeo ids are decimal and nothing else, so a channel slug or a `/settings` path fails
/// here at `start` rather than fetching a 404 and failing at `feed`.
fn numeric_id(candidate: &str) -> Option<String> {
    let ok = !candidate.is_empty()
        && candidate.len() <= 12
        && candidate.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| candidate.to_string())
}

fn parse_config(root: &Value, id: Option<&str>) -> Result<Extraction, SiteError> {
    // Private and deleted videos answer with a bare `{"message": …}` and no `video` at
    // all. Vimeo's own sentence is better than anything invented here, so it is passed
    // through unchanged.
    if let Some(message) = error_message(root) {
        return Err(SiteError::Unavailable(message));
    }

    let video = root.get("video").ok_or_else(shape)?;
    if let Some(why) = privacy_refusal(video.get("privacy").and_then(Value::as_str)) {
        return Err(SiteError::Unavailable(why));
    }

    let title = video
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| id.map(str::to_string))
        .unwrap_or_else(|| "video".to_string());
    let duration_ms = video
        .get("duration")
        .and_then(Value::as_u64)
        .map(|s| s * 1000);
    let native_width = dimension(video.get("width"));
    let native_height = dimension(video.get("height"));

    let files = root.pointer("/request/files").ok_or_else(shape)?;

    // A DRM video is refused here rather than at download time. Vimeo describes it
    // plainly — `request.drm` carries the FairPlay, Widevine and PlayReady licence
    // URLs, and every playlist it nominates is served from a `/drm/` path whose media
    // playlists carry `#EXT-X-KEY:METHOD=SAMPLE-AES,URI="skd://drm"`. There is no
    // unprotected rendition to fall back to: the `fallback_url`s go through `/drm/` too,
    // and `progressive` is empty.
    //
    // Extraction used to succeed on these and offer an "HLS stream (adaptive)" that
    // could only fail once the download reached the first key line. Offering a choice
    // that cannot work is worse than saying so: it costs a click, a wait and a failure
    // to learn what the config stated up front.
    if is_drm_protected(root, files) {
        return Err(SiteError::Unavailable(format!(
            "\"{title}\" is a DRM-protected Vimeo video. Vimeo serves it only through \
             Widevine, PlayReady or FairPlay, and OpenDownloader does not break DRM on \
             any site. Nothing here can download it."
        )));
    }

    let mut options = Vec::new();
    let mut videos = Vec::new();

    let progressive: Vec<Progressive> = files
        .get("progressive")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Progressive::parse).collect())
        .unwrap_or_default();

    for (i, file) in progressive.iter().enumerate() {
        let stream = file.stream();
        options.push(MediaOption {
            label: file.label(),
            // Height is the natural rank, and a progressive file is a single fetch with
            // nothing to merge, so it outranks the adaptive fallback below at any size.
            rank: u64::from(file.height.unwrap_or(0)).max(2),
            streams: vec![stream.clone()],
            filename: safe_filename(&title, "mp4"),
            width: file.width,
            height: file.height,
            duration_ms,
        });
        videos.push(VideoChoice {
            id: format!("progressive-{i}"),
            label: file.label(),
            width: file.width,
            height: file.height,
            fps: file.fps,
            bitrate: None,
            codec: None,
            size: None,
            stream,
            // A progressive file is a finished MP4 with its sound in it.
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`.
            container: None,
            mergeable: false,
        });
    }

    // The master playlist, offered as one option. Everything past this point — variant
    // selection, segment planning, the refusal of encrypted playlists — belongs to
    // `crate::hls`, which already does all of it; handing over a master URL is the whole
    // of this module's job.
    if let Some(url) = hls_url(files) {
        let stream = Stream {
            url,
            kind: StreamKind::Muxed,
            mime: Some("application/x-mpegURL".into()),
            size: None,
            headers: vec![("Referer".into(), REFERER.into())],
            max_chunk: None,
        };
        options.push(MediaOption {
            label: "HLS stream (adaptive)".into(),
            // Below every progressive file: same picture, more work to get it.
            rank: 1,
            streams: vec![stream.clone()],
            filename: safe_filename(&title, "mp4"),
            width: native_width,
            height: native_height,
            duration_ms,
        });
        videos.push(VideoChoice {
            id: "hls".into(),
            label: "HLS stream (adaptive)".into(),
            width: native_width,
            height: native_height,
            fps: dimension(video.get("fps")),
            bitrate: None,
            codec: None,
            size: None,
            stream,
            // `separate_av` is true in the master, but that separation is internal to the
            // playlist and `crate::hls` resolves it. From out here it is one URL that
            // yields picture and sound, which is what `has_audio` is asking.
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`.
            container: None,
            mergeable: false,
        });
    }

    if options.is_empty() {
        return Err(SiteError::Unavailable(
            "Vimeo returned no playable files for this video — it may be restricted to \
             its own site, or available only to people it has been shared with"
                .into(),
        ));
    }

    options.sort_by_key(|o| core::cmp::Reverse(o.rank));

    let mut extraction = Extraction {
        site: "Vimeo".into(),
        title,
        options,
        videos,
        // Vimeo never hands the player a standalone audio rendition here; its audio
        // lives inside the progressive files and inside the HLS master.
        audios: Vec::new(),
        subtitles: text_tracks(root),
    };
    // Progressive entries were pushed before the HLS one and the sort is stable, so a
    // progressive file wins a tie against the adaptive stream at the same size.
    extraction.rank_choices();
    Ok(extraction)
}

/// One entry of `request.files.progressive` that is actually fetchable.
#[derive(Debug, Clone)]
struct Progressive {
    quality: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    mime: Option<String>,
    fps: Option<u32>,
    url: String,
}

impl Progressive {
    fn parse(v: &Value) -> Option<Self> {
        let url = v.get("url")?.as_str()?.trim();
        if url.is_empty() {
            return None;
        }
        Some(Self {
            quality: v
                .get("quality")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(str::to_string),
            width: dimension(v.get("width")),
            height: dimension(v.get("height")),
            mime: v
                .get("mime")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string),
            fps: dimension(v.get("fps")),
            url: url.to_string(),
        })
    }

    /// "720p60 · MP4" — Vimeo's own quality string, with the frame rate folded in the way
    /// every other site in this crate writes it.
    fn label(&self) -> String {
        let quality = match (&self.quality, self.height) {
            (Some(q), _) => q.clone(),
            (None, Some(h)) => format!("{h}p"),
            (None, None) => "video".to_string(),
        };
        let quality = match self.fps {
            Some(fps) if fps > 30 => format!("{quality}{fps}"),
            _ => quality,
        };
        format!("{quality} · {}", container_label(self.mime.as_deref()))
    }

    fn stream(&self) -> Stream {
        Stream {
            url: self.url.clone(),
            kind: StreamKind::Muxed,
            mime: self.mime.clone(),
            size: None,
            headers: vec![("Referer".into(), REFERER.into())],
            max_chunk: None,
        }
    }
}

/// "MP4" from `video/mp4`, and the subtype upper-cased for anything else — displayed
/// only, never parsed back.
fn container_label(mime: Option<&str>) -> String {
    match mime.and_then(|m| m.split('/').nth(1)) {
        Some(sub) if !sub.is_empty() => sub.split(';').next().unwrap_or(sub).to_ascii_uppercase(),
        _ => "MP4".to_string(),
    }
}

/// Vimeo writes pixel counts as numbers and, on some fields of some responses, as
/// strings. Accept both rather than guessing per field.
fn dimension(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
    .map(|n| n as u32)
}

/// Whether this config describes a DRM-protected video.
///
/// Two signals, either of which is enough, because they can appear apart. `request.drm`
/// is Vimeo stating it outright — an object of licence servers. The `/drm/` path segment
/// is the same fact expressed in the URL, and it is present on configs whose `drm` key
/// is absent or empty.
///
/// Deliberately not a check of the playlist body: that would mean fetching a stream to
/// learn it cannot be used, when the config already said so.
fn is_drm_protected(root: &Value, files: &Value) -> bool {
    let declared = root
        .pointer("/request/drm")
        .is_some_and(|d| d.is_object() && d.as_object().is_some_and(|o| !o.is_empty()));
    if declared {
        return true;
    }
    // Any nominated playlist served from a `/drm/` path. Checked across both protocols
    // and every CDN, since the default is only one of several the client may pick.
    ["hls", "dash"].iter().any(|proto| {
        files
            .pointer(&format!("/{proto}/cdns"))
            .and_then(Value::as_object)
            .is_some_and(|cdns| {
                cdns.values().any(|cdn| {
                    cdn.as_object().is_some_and(|entry| {
                        entry
                            .values()
                            .any(|v| v.as_str().is_some_and(|u| u.contains("/drm/")))
                    })
                })
            })
    })
}

/// The master playlist URL, from the CDN the config itself nominates.
///
/// `default_cdn` is Vimeo's own choice for this viewer — the geographically sensible one
/// — so it is honoured rather than second-guessed, and any listed CDN will do when it is
/// missing. `url` carries every codec; `avc_url` is the same ladder pruned to AVC and
/// `fallback_url` a lower ceiling, so both are acceptable stand-ins and neither is
/// preferred while `url` exists.
fn hls_url(files: &Value) -> Option<String> {
    let hls = files.get("hls")?;
    let cdns = hls.get("cdns")?.as_object()?;
    let entry = hls
        .get("default_cdn")
        .and_then(Value::as_str)
        .and_then(|name| cdns.get(name))
        .or_else(|| cdns.values().next())?;
    ["url", "avc_url", "fallback_url"].iter().find_map(|key| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    })
}

/// Vimeo's error bodies, in the two shapes it uses.
fn error_message(root: &Value) -> Option<String> {
    for key in ["message", "error"] {
        if let Some(message) = root
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return Some(message.to_string());
        }
    }
    None
}

/// Refuse the privacy settings that mean "you are not getting this", and only those.
///
/// An unrecognised value is *not* refused. A config that arrived at all still carries
/// whatever files Vimeo was willing to serve, so treating a label this code has not seen
/// before as a refusal would break working videos for the sake of tidiness — and the
/// "no playable files" path already catches the case where the refusal was real.
fn privacy_refusal(privacy: Option<&str>) -> Option<String> {
    let value = privacy?.trim().to_ascii_lowercase();
    let message = match value.as_str() {
        "password" => {
            "this Vimeo video is password-protected, and the password is not something \
                       opendownloader can supply"
        }
        "nobody" | "disable" => {
            "this Vimeo video is private — its owner has not published it to anyone"
        }
        "contacts" | "users" | "ptv" => {
            "this Vimeo video is restricted to signed-in members, so it cannot be fetched here"
        }
        _ => return None,
    };
    Some(message.to_string())
}

/// `request.text_tracks[]`, which Vimeo serves as ready-made WebVTT.
fn text_tracks(root: &Value) -> Vec<SubtitleTrack> {
    let Some(tracks) = root
        .pointer("/request/text_tracks")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    tracks
        .iter()
        .filter_map(|t| {
            let url = t.get("url").and_then(Value::as_str).map(str::trim)?;
            if url.is_empty() {
                return None;
            }
            let language = t.get("lang").and_then(Value::as_str).map(str::to_string);
            let label = t
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .or_else(|| language.clone())
                .unwrap_or_else(|| "Subtitles".to_string());
            Some(SubtitleTrack {
                label,
                language,
                // The captions endpoint is relative-free and already signed; it is only
                // ever `…/captions/<id>.vtt?expires=…&sig=…`.
                url: url.to_string(),
                format: "vtt".into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of the real `player.vimeo.com/video/76979871/config` captured on
    /// 2026-09-05. URLs shortened and the irrelevant three quarters (embed settings, A/B
    /// tests, DRM licence URLs, thumbnail sprites) dropped; every key this module reads
    /// is byte-for-byte the shape that was served, **including the empty
    /// `progressive` array**, which is the ordinary modern case rather than an edge one.
    const CONFIG: &str = r#"{
        "cdn_url": "https://f.vimeocdn.com",
        "request": {
            "files": {
                "dash": {
                    "cdns": {
                        "akfire_interconnect_quic": {
                            "url": "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/playlist.mpd",
                            "origin": "akamai"
                        }
                    },
                    "default_cdn": "akfire_interconnect_quic",
                    "separate_av": true
                },
                "hls": {
                    "captions": "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/captions.m3u8",
                    "cdns": {
                        "akfire_interconnect_quic": {
                            "avc_url": "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/avc/playlist.m3u8",
                            "fallback_url": "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/sd/playlist.m3u8",
                            "origin": "akamai",
                            "url": "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/av/playlist.m3u8"
                        },
                        "fastly_skyfire": {
                            "url": "https://vod-adaptive-fs.vimeocdn.com/exp=1788625312/av/playlist.m3u8",
                            "origin": "fastly"
                        }
                    },
                    "default_cdn": "akfire_interconnect_quic",
                    "separate_av": true
                },
                "progressive": []
            },
            "text_tracks": [
                {"id": 170, "lang": "de", "url": "https://captions.vimeo.com/captions/170.vtt?sig=baa36",
                 "kind": "subtitles", "label": "Deutsch", "default": true},
                {"id": 140662, "lang": "en", "url": "https://captions.vimeo.com/captions/140662.vtt?sig=c71f0",
                 "kind": "captions", "label": "English", "default": false}
            ],
            "expires": 3600
        },
        "player_url": "player.vimeo.com",
        "video": {
            "id": 76979871,
            "title": "The New Vimeo Player (You Know, For Videos)",
            "width": 1280,
            "height": 720,
            "duration": 62,
            "url": "https://vimeo.com/76979871",
            "privacy": "anybody",
            "fps": 24,
            "owner": {"id": 152184, "name": "Vimeo"}
        }
    }"#;

    /// The same config with the progressive ladder an older upload still carries.
    fn config_with_progressive() -> String {
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["request"]["files"]["progressive"] = serde_json::json!([
            {"profile": "165", "quality": "540p", "width": 960, "height": 540,
             "mime": "video/mp4", "fps": 24, "url": "https://player.vimeo.com/progressive/540p.mp4"},
            {"profile": "113", "quality": "720p", "width": 1280, "height": 720,
             "mime": "video/mp4", "fps": 60, "url": "https://player.vimeo.com/progressive/720p.mp4"},
            {"profile": "112", "quality": "360p", "width": 640, "height": 360,
             "mime": "video/mp4", "fps": 24, "url": ""}
        ]);
        v.to_string()
    }

    fn extract(body: &str) -> Result<Extraction, SiteError> {
        let mut vimeo = Vimeo::new();
        vimeo.start("https://vimeo.com/76979871").unwrap();
        match vimeo.feed(&[body])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("the extractor asked for a second fetch"),
        }
    }

    fn labels(e: &Extraction) -> Vec<String> {
        e.options.iter().map(|o| o.label.clone()).collect()
    }

    #[test]
    fn every_shape_of_vimeo_link_yields_the_same_video_id() {
        let expected = Some("76979871".to_string());
        assert_eq!(video_id("https://vimeo.com/76979871"), expected);
        assert_eq!(
            video_id("https://vimeo.com/channels/staffpicks/76979871"),
            expected
        );
        assert_eq!(
            video_id("https://vimeo.com/groups/motion/videos/76979871"),
            expected
        );
        assert_eq!(
            video_id("https://player.vimeo.com/video/76979871?h=8272103f6e"),
            expected
        );
        // An unlisted link puts the hash after the id; the config does not want it.
        assert_eq!(video_id("https://vimeo.com/76979871/8272103f6e"), expected);
    }

    #[test]
    fn a_link_that_names_no_video_is_refused_with_a_sentence() {
        assert_eq!(video_id("https://vimeo.com/staffpicks"), None);
        assert_eq!(video_id("https://vimeo.com/channels/staffpicks"), None);
        assert_eq!(video_id("https://vimeo.com/"), None);
        let err = Vimeo::new().start("https://vimeo.com/upgrade").unwrap_err();
        match err {
            SiteError::Unavailable(msg) => assert!(msg.contains("does not name a video"), "{msg}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_only_request_is_a_get_of_the_player_config() {
        let mut vimeo = Vimeo::new();
        let step = vimeo
            .start("https://player.vimeo.com/video/76979871")
            .unwrap();
        let Step::Need(Need::Fetch(reqs)) = step else {
            panic!("expected a fetch");
        };
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(
            reqs[0].url,
            "https://player.vimeo.com/video/76979871/config"
        );
        assert!(reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Referer" && v == REFERER));
        assert_eq!(reqs[0].body, None);
    }

    /// Vimeo stating DRM outright, in `request.drm`.
    const DRM_DECLARED: &str = r#"{"video":{"title":"Protected","duration":47},
"request":{"drm":{"fairplay":{"license_url":"https://vimeo.test/fp"}},
"files":{"hls":{"default_cdn":"a","cdns":{"a":{"url":"https://cdn.test/av/playlist.m3u8"}}}}}}"#;

    /// The same fact in the URL, on a config that carries no `drm` key.
    const DRM_IN_PATH: &str = r#"{"video":{"title":"Protected","duration":47},
"request":{"files":{"hls":{"default_cdn":"a","cdns":{"a":
{"url":"https://cdn.test/v2/playlist/drm/cbcs,derivedv2,abc/av/playlist.m3u8"}}}}}}"#;

    /// A DRM video is refused at extraction, not left to fail mid-download.
    ///
    /// Both signals are checked because they appear apart: a live config for
    /// `vimeo.com/1086925006` carried both, and the trimmed fixture above carries
    /// neither. Offering an "HLS stream" that can only fail at its first key line costs
    /// a click, a wait and a failure to learn what the config already stated.
    #[test]
    fn a_drm_video_is_refused_with_a_reason_rather_than_offered() {
        for (name, config) in [("declared", DRM_DECLARED), ("in the path", DRM_IN_PATH)] {
            let err = extract(config).expect_err(&format!("{name} should be refused"));
            match err {
                SiteError::Unavailable(m) => {
                    assert!(m.contains("DRM-protected"), "{name}: {m}");
                    assert!(m.contains("Protected"), "{name} should name the video: {m}");
                }
                other => panic!("{name}: expected Unavailable, got {other:?}"),
            }
        }
    }

    /// A config with neither signal is untouched by the DRM check.
    #[test]
    fn an_ordinary_config_is_not_mistaken_for_drm() {
        // The captured fixture has no `drm` key and no `/drm/` path, and must still
        // extract — a DRM check that catches everything would be worse than none.
        assert!(extract(CONFIG).is_ok());
    }

    #[test]
    fn an_adaptive_only_config_still_offers_the_hls_master() {
        // The captured 2026 config has no progressive files at all. If this path were
        // treated as a failure, the real Vimeo of today would extract to nothing.
        let e = extract(CONFIG).unwrap();
        assert_eq!(e.site, "Vimeo");
        assert_eq!(e.title, "The New Vimeo Player (You Know, For Videos)");
        assert_eq!(labels(&e), vec!["HLS stream (adaptive)"]);
        assert_eq!(e.options[0].duration_ms, Some(62_000));
        assert_eq!(e.options[0].height, Some(720));
        assert!(e.options[0].filename.ends_with(".mp4"));
        assert_eq!(
            e.options[0].streams[0].url,
            "https://vod-adaptive-ak.vimeocdn.com/exp=1788625312/av/playlist.m3u8"
        );
        assert_eq!(
            e.options[0].streams[0].mime.as_deref(),
            Some("application/x-mpegURL")
        );
    }

    #[test]
    fn progressive_files_are_offered_ahead_of_the_adaptive_fallback() {
        let e = extract(&config_with_progressive()).unwrap();
        // Two usable progressive files — the third has an empty URL — plus the master.
        assert_eq!(
            labels(&e),
            vec!["720p60 · MP4", "540p · MP4", "HLS stream (adaptive)"]
        );
        assert_eq!(e.options[0].width, Some(1280));
        assert_eq!(
            e.options[0].streams[0].url,
            "https://player.vimeo.com/progressive/720p.mp4"
        );
        assert_eq!(e.options[0].streams[0].kind, StreamKind::Muxed);
    }

    #[test]
    fn the_dash_manifest_is_never_offered_because_nothing_here_can_read_one() {
        let e = extract(CONFIG).unwrap();
        for option in &e.options {
            for stream in &option.streams {
                assert!(!stream.url.ends_with(".mpd"), "{}", stream.url);
            }
        }
    }

    #[test]
    fn exactly_one_video_choice_is_marked_best_and_it_is_the_largest() {
        let e = extract(&config_with_progressive()).unwrap();
        assert_eq!(e.videos.iter().filter(|v| v.best).count(), 1);
        assert!(e.videos[0].best);
        assert_eq!(e.videos[0].height, Some(720));
        assert!(e.audios.is_empty(), "Vimeo muxes its audio in");
        assert_eq!(e.best_audio(), None);
    }

    #[test]
    fn a_progressive_file_wins_a_tie_against_the_adaptive_stream_of_the_same_size() {
        // Both are 1280x720 here. The single-file fetch should still be the recommended
        // one, which relies on the ranking sort being stable and on progressive being
        // pushed first.
        let e = extract(&config_with_progressive()).unwrap();
        assert_eq!(e.best_video().unwrap().id, "progressive-1");
    }

    #[test]
    fn every_video_choice_has_a_unique_id() {
        let e = extract(&config_with_progressive()).unwrap();
        let mut ids: Vec<&str> = e.videos.iter().map(|v| v.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate video choice id");
    }

    #[test]
    fn every_stream_carries_the_referer_vimeos_cdn_wants() {
        let e = extract(&config_with_progressive()).unwrap();
        for option in &e.options {
            for stream in &option.streams {
                assert!(
                    stream
                        .headers
                        .iter()
                        .any(|(k, v)| k == "Referer" && v == REFERER),
                    "{} is missing the Referer",
                    option.label
                );
            }
        }
        for choice in &e.videos {
            assert!(choice.stream.headers.iter().any(|(k, _)| k == "Referer"));
        }
    }

    #[test]
    fn every_progressive_choice_says_it_already_has_sound() {
        let e = extract(&config_with_progressive()).unwrap();
        assert!(e.videos.iter().all(|v| v.has_audio));
    }

    #[test]
    fn a_deleted_video_is_reported_in_vimeos_own_words() {
        let body = r#"{"message": "The requested video could not be found.", "error_code": 4101}"#;
        assert_eq!(
            extract(body).unwrap_err(),
            SiteError::Unavailable("The requested video could not be found.".into())
        );
    }

    #[test]
    fn a_password_protected_video_says_what_is_wrong_with_it() {
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["video"]["privacy"] = Value::String("password".into());
        let SiteError::Unavailable(msg) = extract(&v.to_string()).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("password-protected"), "{msg}");
    }

    #[test]
    fn an_unrecognised_privacy_label_does_not_refuse_a_video_that_has_files() {
        // Refusing on a label this code has never seen would break working videos, and
        // the empty-files path below already covers the case where it mattered.
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["video"]["privacy"] = Value::String("some_new_tier".into());
        assert!(extract(&v.to_string()).is_ok());
    }

    #[test]
    fn a_config_with_nothing_playable_says_so_instead_of_extracting_nothing() {
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["request"]["files"] = serde_json::json!({"progressive": []});
        let SiteError::Unavailable(msg) = extract(&v.to_string()).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("no playable files"), "{msg}");
    }

    #[test]
    fn text_tracks_become_subtitle_tracks_without_being_asked_to_convert() {
        let e = extract(CONFIG).unwrap();
        assert_eq!(e.subtitles.len(), 2);
        assert_eq!(e.subtitles[0].label, "Deutsch");
        assert_eq!(e.subtitles[0].language.as_deref(), Some("de"));
        assert_eq!(e.subtitles[0].format, "vtt");
        assert_eq!(e.subtitles[1].label, "English");
    }

    #[test]
    fn a_title_with_a_slash_cannot_escape_the_download_directory() {
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["video"]["title"] = Value::String("AC/DC — live".into());
        let e = extract(&v.to_string()).unwrap();
        assert!(
            !e.options[0].filename.contains('/'),
            "{}",
            e.options[0].filename
        );
    }

    #[test]
    fn a_body_that_is_not_a_config_at_all_is_a_shape_error() {
        assert_eq!(extract("not json").unwrap_err(), shape());
        assert_eq!(extract("{}").unwrap_err(), shape());
        assert_eq!(Vimeo::new().feed(&[]).unwrap_err(), shape());
    }

    #[test]
    fn the_cdn_the_config_nominates_is_the_one_used() {
        let mut v: Value = serde_json::from_str(CONFIG).unwrap();
        v["request"]["files"]["hls"]["default_cdn"] = Value::String("fastly_skyfire".into());
        let e = extract(&v.to_string()).unwrap();
        assert_eq!(
            e.options[0].streams[0].url,
            "https://vod-adaptive-fs.vimeocdn.com/exp=1788625312/av/playlist.m3u8"
        );
    }

    #[test]
    fn only_vimeos_own_hosts_match() {
        assert!(matches("vimeo.com"));
        assert!(matches("www.vimeo.com"));
        assert!(matches("player.vimeo.com"));
        assert!(matches("vod-adaptive-ak.vimeocdn.com"));
        assert!(!matches("notvimeo.com"));
        assert!(!matches("vimeo.com.evil.test"));
    }
}
