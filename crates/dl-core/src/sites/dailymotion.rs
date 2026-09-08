//! Dailymotion, via the player's metadata endpoint.
//!
//! Built against the documented shape of
//! `https://www.dailymotion.com/player/metadata/video/<ID>` and against one captured
//! error response (2026-09-05, a 404 for a missing id). **The success path has not been
//! verified live** — the capture that exists is the failure — so the parser is written to
//! the endpoint's published shape and every assertion below is a fixture rather than a
//! recording. If Dailymotion has moved on, this fails with [`SiteError::Shape`] naming
//! itself, which is the bug report.
//!
//! # Why an endpoint rather than the page
//!
//! [`super::generic`] claims `dailymotion.com` and finds nothing useful: the watch page
//! is a React shell whose `og:video` points at the embed player, not a file. The
//! metadata endpoint needs no authentication and answers with the whole quality ladder,
//! so one request replaces a page read that was never going to work.
//!
//! # The ladder, and the odd rung
//!
//! `qualities` is keyed by height as a bare string — `"240"`, `"380"`, `"480"`, `"720"`,
//! `"1080"` — plus `"auto"`, which is not a height at all but the HLS master. The `380`
//! is not a typo; it is Dailymotion's own ladder. Each key holds an array of
//! `{type, url}`, MP4 for the progressive rungs and `application/x-mpegURL` for `auto`.

use serde_json::Value;

use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, Request, SiteError, Step,
    Stream, StreamKind, VideoChoice,
};

/// Dailymotion's CDN serves media to Dailymotion's own pages.
const REFERER: &str = "https://www.dailymotion.com/";

/// The `qualities` key that is a playlist rather than a height.
const ADAPTIVE_KEY: &str = "auto";

const CLAIMED: &[&str] = &["dailymotion.com", "dai.ly"];

pub fn matches(host: &str) -> bool {
    CLAIMED.iter().any(|d| host_is(host, d))
}

#[derive(Debug, Default)]
pub struct Dailymotion {
    /// Kept from `start` so a response with no usable `title` still names its file after
    /// something the user can recognise.
    video_id: Option<String>,
}

impl Dailymotion {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Dailymotion {
    fn site(&self) -> &'static str {
        "Dailymotion"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        let id = video_id(url).ok_or_else(|| {
            SiteError::Unavailable(
                "this Dailymotion link does not name a video — open the video itself and try again"
                    .into(),
            )
        })?;
        let request = metadata_request(&id);
        self.video_id = Some(id);
        Ok(Step::Need(Need::Fetch(vec![request])))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().ok_or_else(shape)?;
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        parse_metadata(&root, self.video_id.as_deref()).map(Step::Done)
    }
}

fn shape() -> SiteError {
    SiteError::Shape("Dailymotion".into())
}

/// The one request this extractor makes. The id has already been validated as
/// alphanumeric, so nothing in it can escape the path.
fn metadata_request(video_id: &str) -> Request {
    Request::get(format!(
        "https://www.dailymotion.com/player/metadata/video/{video_id}"
    ))
    .with_header("Referer", REFERER)
}

/// Pull the video id out of a Dailymotion link.
///
/// Both shapes put the id in a path segment: `dailymotion.com/video/<id>` (and its
/// `/embed/video/<id>` twin) or `dai.ly/<id>`. Old links append a slug behind an
/// underscore, which the endpoint does not want.
pub fn video_id(url: &str) -> Option<String> {
    let path = path_of(url);

    if let Some(rest) = path.split_once("/video/").map(|(_, r)| r) {
        return first_segment(rest).and_then(valid_id);
    }

    // `dai.ly/<id>` puts the id where a path would be. Checking the host rather than
    // simply taking the first segment keeps `dailymotion.com/us/` from being read as an
    // id — the shortener is the only shape where a bare first segment means a video.
    if host_is(&authority_of(url).to_ascii_lowercase(), "dai.ly") {
        return first_segment(path).and_then(valid_id);
    }

    None
}

fn authority_of(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
}

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

/// Dailymotion ids are short alphanumeric strings; the trailing `_slug` on legacy links
/// is not part of the id and the endpoint 404s when it is included.
fn valid_id(candidate: &str) -> Option<String> {
    let id = candidate.split('_').next().unwrap_or(candidate);
    let ok = !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then(|| id.to_string())
}

fn parse_metadata(root: &Value, id: Option<&str>) -> Result<Extraction, SiteError> {
    if let Some(message) = error_message(root) {
        return Err(SiteError::Unavailable(message));
    }
    if root
        .get("is_password_protected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(SiteError::Unavailable(
            "this Dailymotion video is password-protected, and the password is not something \
             opendownloader can supply"
                .into(),
        ));
    }

    let title = root
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| id.map(str::to_string))
        .unwrap_or_else(|| "video".to_string());
    let duration_ms = root
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|d| *d > 0.0)
        .map(|d| (d * 1000.0) as u64);

    let qualities = root
        .get("qualities")
        .and_then(Value::as_object)
        .ok_or_else(shape)?;

    // Progressive rungs first, tallest first. `qualities` arrives as an unordered map, so
    // the ladder is rebuilt here rather than trusted to come out in a useful order.
    let mut heights: Vec<u32> = qualities.keys().filter_map(|k| k.parse().ok()).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));

    let mut options = Vec::new();
    let mut videos = Vec::new();

    for height in heights {
        let Some(url) = first_url_of_type(qualities.get(&height.to_string()), "video/") else {
            continue;
        };
        let label = format!("{height}p · MP4");
        let stream = progressive_stream(&url);
        options.push(MediaOption {
            label: label.clone(),
            rank: u64::from(height).max(2),
            streams: vec![stream.clone()],
            filename: safe_filename(&title, "mp4"),
            width: None,
            height: Some(height),
            duration_ms,
        });
        videos.push(VideoChoice {
            id: format!("mp4-{height}"),
            label,
            // Dailymotion names the rung by height and never states a width, so there is
            // no honest width to report. `rank_choices` compares pixel counts and ties
            // every one of these at zero as a result — which is why they are pushed
            // tallest-first and the sort is stable: insertion order is what actually
            // orders them, and it is already the right order.
            width: None,
            height: Some(height),
            fps: None,
            bitrate: None,
            codec: None,
            size: None,
            stream,
            // Every Dailymotion rendition is a finished file with its sound in it.
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`.
            container: None,
            mergeable: false,
        });
    }

    // The HLS master, as the fallback. It is last because it costs a playlist walk and a
    // segment merge to arrive at a picture one of the rungs above already offers — but it
    // is also the only entry present on videos whose progressive ladder is withheld.
    if let Some(url) = first_url_of_type(qualities.get(ADAPTIVE_KEY), "application/") {
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
            rank: 1,
            streams: vec![stream.clone()],
            filename: safe_filename(&title, "mp4"),
            width: None,
            height: None,
            duration_ms,
        });
        videos.push(VideoChoice {
            id: "hls".into(),
            label: "HLS stream (adaptive)".into(),
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            codec: None,
            size: None,
            stream,
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`.
            container: None,
            mergeable: false,
        });
    }

    if options.is_empty() {
        // A 200 with an empty ladder is Dailymotion's way of saying no, and it says it
        // without an `error` block. Reporting an empty extraction here would show the
        // user a working-looking dialog with nothing in it.
        return Err(SiteError::Unavailable(
            "Dailymotion listed no playable renditions for this video — it is usually blocked \
             in this country, or restricted to the site it is embedded on"
                .into(),
        ));
    }

    let mut extraction = Extraction {
        note: None,
        site: "Dailymotion".into(),
        title,
        options,
        videos,
        // Dailymotion offers no standalone audio rendition; its sound is inside every
        // file it lists.
        audios: Vec::new(),
        subtitles: Vec::new(),
    };
    extraction.rank_choices();
    Ok(extraction)
}

/// The first entry of one `qualities` rung whose `type` starts with `prefix`.
///
/// A rung is an array because Dailymotion sometimes lists the same rendition on more than
/// one CDN. Any of them plays; the first is as good as the rest.
fn first_url_of_type(rung: Option<&Value>, prefix: &str) -> Option<String> {
    rung?.as_array()?.iter().find_map(|entry| {
        let ty = entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !ty.to_ascii_lowercase().starts_with(prefix) {
            return None;
        }
        entry
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    })
}

fn progressive_stream(url: &str) -> Stream {
    Stream {
        url: url.to_string(),
        kind: StreamKind::Muxed,
        mime: Some("video/mp4".into()),
        size: None,
        headers: vec![("Referer".into(), REFERER.into())],
        max_chunk: None,
    }
}

/// Dailymotion's error block, in its own words where it has any.
///
/// The captured 404 carries `message` and no `title`; the player's other errors carry
/// both. Whichever is present is passed through, because a sentence written by the site
/// about the video the user asked for beats anything general written here.
fn error_message(root: &Value) -> Option<String> {
    let error = root.get("error")?;
    if let Some(text) = error.as_str().map(str::trim).filter(|t| !t.is_empty()) {
        return Some(text.to_string());
    }
    let field = |key: &str| {
        error
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Some(match (field("title"), field("message")) {
        (Some(title), Some(message)) if title != message => format!("{title} — {message}"),
        (Some(title), _) => title,
        (None, Some(message)) => message,
        (None, None) => "Dailymotion refused to play this video".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The metadata shape the player receives: the full ladder, an `auto` HLS entry, and
    /// one rung listed on two CDNs.
    const METADATA: &str = r#"{
        "id": "x2hwqn9",
        "title": "A video with a ladder",
        "duration": 213,
        "owner": {"screenname": "someone"},
        "is_password_protected": false,
        "qualities": {
            "240": [{"type": "video/mp4", "url": "https://cdn.dm/240.mp4"}],
            "380": [{"type": "video/mp4", "url": "https://cdn.dm/380.mp4"}],
            "480": [{"type": "video/mp4", "url": "https://cdn.dm/480.mp4"}],
            "720": [{"type": "video/mp4", "url": "https://cdn.dm/720.mp4"},
                    {"type": "video/mp4", "url": "https://cdn2.dm/720.mp4"}],
            "1080": [{"type": "video/mp4", "url": "https://cdn.dm/1080.mp4"}],
            "auto": [{"type": "application/x-mpegURL", "url": "https://cdn.dm/master.m3u8"}]
        }
    }"#;

    /// The real 404 captured on 2026-09-05, trimmed to the keys this module reads.
    const NOT_FOUND: &str = r#"{
        "error": {
            "more_info": "https://developer.dailymotion.com/api#error-codes",
            "code": "404",
            "message": "Can't find object video for `id' parameter",
            "type": "not_found",
            "error_data": {"reason": "object_not_found", "object_type": "video"}
        },
        "data_center": "dc3",
        "access_id": "x2hwqn9",
        "is_password_protected": false
    }"#;

    fn extract(body: &str) -> Result<Extraction, SiteError> {
        let mut dm = Dailymotion::new();
        dm.start("https://www.dailymotion.com/video/x2hwqn9")
            .unwrap();
        match dm.feed(&[body])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("the extractor asked for a second fetch"),
        }
    }

    fn labels(e: &Extraction) -> Vec<String> {
        e.options.iter().map(|o| o.label.clone()).collect()
    }

    #[test]
    fn every_shape_of_dailymotion_link_yields_the_same_video_id() {
        let expected = Some("x2hwqn9".to_string());
        assert_eq!(
            video_id("https://www.dailymotion.com/video/x2hwqn9"),
            expected
        );
        assert_eq!(video_id("https://dai.ly/x2hwqn9"), expected);
        assert_eq!(
            video_id("https://www.dailymotion.com/embed/video/x2hwqn9?autoplay=1"),
            expected
        );
        // A legacy link glues a slug onto the id; the endpoint 404s if it is kept.
        assert_eq!(
            video_id("https://www.dailymotion.com/video/x2hwqn9_some-old-slug_fun"),
            expected
        );
    }

    #[test]
    fn a_link_that_names_no_video_is_refused_with_a_sentence() {
        assert_eq!(video_id("https://www.dailymotion.com/us"), None);
        assert_eq!(video_id("https://www.dailymotion.com/video/"), None);
        let err = Dailymotion::new()
            .start("https://www.dailymotion.com/channel/news")
            .unwrap_err();
        match err {
            SiteError::Unavailable(msg) => assert!(msg.contains("does not name a video"), "{msg}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_only_request_is_a_get_of_the_player_metadata() {
        let mut dm = Dailymotion::new();
        let step = dm.start("https://dai.ly/x2hwqn9").unwrap();
        let Step::Need(Need::Fetch(reqs)) = step else {
            panic!("expected a fetch");
        };
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(
            reqs[0].url,
            "https://www.dailymotion.com/player/metadata/video/x2hwqn9"
        );
        assert!(reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Referer" && v == REFERER));
    }

    #[test]
    fn the_whole_ladder_is_offered_tallest_first_with_hls_last() {
        let e = extract(METADATA).unwrap();
        assert_eq!(e.site, "Dailymotion");
        assert_eq!(e.title, "A video with a ladder");
        assert_eq!(
            labels(&e),
            vec![
                "1080p · MP4",
                "720p · MP4",
                "480p · MP4",
                "380p · MP4",
                "240p · MP4",
                "HLS stream (adaptive)"
            ]
        );
        assert_eq!(e.options[0].duration_ms, Some(213_000));
        assert_eq!(e.options[0].streams[0].url, "https://cdn.dm/1080.mp4");
        assert_eq!(
            e.options[5].streams[0].mime.as_deref(),
            Some("application/x-mpegURL")
        );
    }

    #[test]
    fn a_rung_listed_on_two_cdns_is_offered_once() {
        let e = extract(METADATA).unwrap();
        let seven_twenty: Vec<&MediaOption> =
            e.options.iter().filter(|o| o.height == Some(720)).collect();
        assert_eq!(seven_twenty.len(), 1);
        assert_eq!(seven_twenty[0].streams[0].url, "https://cdn.dm/720.mp4");
    }

    #[test]
    fn exactly_one_video_choice_is_marked_best_and_it_is_the_tallest() {
        let e = extract(METADATA).unwrap();
        assert_eq!(e.videos.iter().filter(|v| v.best).count(), 1);
        assert_eq!(e.best_video().unwrap().id, "mp4-1080");
        assert!(e.audios.is_empty(), "Dailymotion muxes its audio in");
    }

    #[test]
    fn every_video_choice_has_a_unique_id_and_says_it_already_has_sound() {
        let e = extract(METADATA).unwrap();
        let mut ids: Vec<&str> = e.videos.iter().map(|v| v.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate video choice id");
        assert!(e.videos.iter().all(|v| v.has_audio));
    }

    #[test]
    fn every_stream_carries_the_referer_dailymotion_wants() {
        let e = extract(METADATA).unwrap();
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
        assert!(e
            .videos
            .iter()
            .all(|v| v.stream.headers.iter().any(|(k, _)| k == "Referer")));
    }

    #[test]
    fn a_missing_video_is_reported_in_dailymotions_own_words() {
        let SiteError::Unavailable(msg) = extract(NOT_FOUND).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert_eq!(msg, "Can't find object video for `id' parameter");
    }

    #[test]
    fn an_error_with_both_a_title_and_a_message_reads_as_one_sentence() {
        let body = r#"{"error": {"title": "Video unavailable",
                                 "message": "This content is not available in your country.",
                                 "code": "DM007"}}"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert_eq!(
            msg,
            "Video unavailable — This content is not available in your country."
        );
    }

    #[test]
    fn an_empty_ladder_is_explained_rather_than_returned_as_an_empty_extraction() {
        let body = r#"{"id": "x2hwqn9", "title": "Blocked", "duration": 10, "qualities": {}}"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("no playable renditions"), "{msg}");
    }

    #[test]
    fn a_password_protected_video_says_so_before_looking_at_the_ladder() {
        let body = r#"{"id": "x2hwqn9", "title": "Locked", "is_password_protected": true,
                       "qualities": {}}"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("password-protected"), "{msg}");
    }

    #[test]
    fn an_hls_only_response_is_still_a_download() {
        // Some videos are served adaptive-only. One option beats none.
        let body = r#"{"id": "x2hwqn9", "title": "Adaptive only", "duration": 30,
                       "qualities": {"auto": [{"type": "application/x-mpegURL",
                                               "url": "https://cdn.dm/master.m3u8"}]}}"#;
        let e = extract(body).unwrap();
        assert_eq!(labels(&e), vec!["HLS stream (adaptive)"]);
        assert!(e.best_video().unwrap().best);
    }

    #[test]
    fn a_title_with_a_slash_cannot_escape_the_download_directory() {
        let mut v: Value = serde_json::from_str(METADATA).unwrap();
        v["title"] = Value::String("AC/DC — live".into());
        let e = extract(&v.to_string()).unwrap();
        assert!(
            !e.options[0].filename.contains('/'),
            "{}",
            e.options[0].filename
        );
    }

    #[test]
    fn a_body_that_is_not_metadata_at_all_is_a_shape_error() {
        assert_eq!(extract("not json").unwrap_err(), shape());
        assert_eq!(extract("{}").unwrap_err(), shape());
        assert_eq!(Dailymotion::new().feed(&[]).unwrap_err(), shape());
    }

    #[test]
    fn only_dailymotions_own_hosts_match() {
        assert!(matches("dailymotion.com"));
        assert!(matches("www.dailymotion.com"));
        assert!(matches("dai.ly"));
        assert!(!matches("notdailymotion.com"));
        assert!(!matches("dailymotion.com.evil.test"));
    }
}
