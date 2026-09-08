//! X (Twitter).
//!
//! **Verified live on 2026-09-05** against the syndication endpoint, which is what the
//! embedded-tweet widget uses and which needs no account, no token and no signing:
//!
//! ```text
//! GET https://cdn.syndication.twimg.com/tweet-result?id=<ID>&lang=en&token=a
//! ```
//!
//! A real response for a post with video returned `mediaDetails[0].video_info.variants`
//! holding one HLS entry and three progressive MP4s at 256, 832 and 2176 kbps, and a
//! post without video returned the same shape with an empty `mediaDetails`. Both cases
//! are covered below from those captures.
//!
//! The `token` parameter is checked for presence rather than value — any non-empty string
//! works — which is why a constant is fine and why this does not rot the way a signed
//! endpoint would.
//!
//! Every variant is a complete file with its own sound, so nothing here is ever merged
//! and `audios` stays empty. The dimensions come out of the URL rather than the JSON:
//! Twitter encodes them in the path as `/vid/<width>x<height>/`, and `aspect_ratio` alone
//! cannot tell 480x270 from 1280x720.

use serde_json::Value;

use super::{
    host_is, safe_filename, AudioChoice, Extraction, Extractor, MediaOption, Need, Request,
    SiteError, Step, Stream, StreamKind, VideoChoice,
};

const CLAIMED: &[&str] = &["twitter.com", "x.com", "t.co", "twimg.com"];

/// Any non-empty token is accepted by the endpoint; it is a presence check, not a secret.
const SYNDICATION: &str = "https://cdn.syndication.twimg.com/tweet-result";

pub fn matches(host: &str) -> bool {
    CLAIMED.iter().any(|d| host_is(host, d))
}

#[derive(Debug, Default)]
pub struct Twitter {
    /// Kept from `start` so a post whose text is empty — an image-and-video reply, say —
    /// still names its file after something.
    status_id: Option<String>,
}

impl Twitter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Twitter {
    fn site(&self) -> &'static str {
        "X"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        let id = status_id(url).ok_or_else(|| {
            SiteError::Unavailable(
                "this link does not name a post. A link to a single post — the one with \
                 /status/ in it — is what can be downloaded."
                    .into(),
            )
        })?;
        self.status_id = Some(id.clone());
        Ok(Step::Need(Need::Fetch(vec![Request::get(format!(
            "{SYNDICATION}?id={id}&lang=en&token=a"
        ))
        .with_header("Referer", "https://twitter.com/")])))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().ok_or_else(shape)?;
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        Ok(Step::Done(parse(&root, self.status_id.as_deref())?))
    }
}

fn shape() -> SiteError {
    SiteError::Shape("X".into())
}

/// The numeric id from any of the shapes a post link comes in.
///
/// `/i/status/<id>`, `/<user>/status/<id>`, and the `/statuses/` spelling older links
/// still use. A trailing `/photo/1` or `?s=20` is ignored, which matters because the
/// share button adds one.
pub fn status_id(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let mut segments = path.split('/').filter(|s| !s.is_empty()).peekable();
    while let Some(segment) = segments.next() {
        if segment == "status" || segment == "statuses" {
            let candidate = segments.next()?;
            let digits: String = candidate.chars().take_while(char::is_ascii_digit).collect();
            return (!digits.is_empty()).then_some(digits);
        }
    }
    None
}

fn parse(root: &Value, status_id: Option<&str>) -> Result<Extraction, SiteError> {
    // A deleted, protected or age-restricted post answers with an error object rather
    // than an HTTP status, so the distinction has to be made here.
    if let Some(message) = root
        .get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(Value::as_str)
    {
        return Err(SiteError::Unavailable(format!(
            "X would not return that post: {message}"
        )));
    }
    if root.get("__typename").and_then(Value::as_str) == Some("TweetTombstone") {
        return Err(SiteError::Unavailable(
            "that post has been deleted, or its author's account is protected.".into(),
        ));
    }

    let handle = root
        .get("user")
        .and_then(|u| u.get("screen_name"))
        .and_then(Value::as_str);
    let title = post_title(root, handle, status_id);

    let media = root
        .get("mediaDetails")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut videos: Vec<VideoChoice> = Vec::new();
    let mut options: Vec<MediaOption> = Vec::new();
    let mut taken: Vec<String> = Vec::new();

    for item in &media {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        // `animated_gif` is a silent MP4 in every respect that matters here.
        if kind != "video" && kind != "animated_gif" {
            continue;
        }
        let variants = item
            .get("video_info")
            .and_then(|v| v.get("variants"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for variant in &variants {
            let url = match variant.get("url").and_then(Value::as_str) {
                Some(u) if !u.is_empty() => u,
                _ => continue,
            };
            let content_type = variant
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let bitrate = variant.get("bitrate").and_then(Value::as_u64);
            // X writes `application/x-mpegURL` with a capital URL, so this has to be
            // case-insensitive — matching the lowercase spelling silently classifies the
            // playlist as a progressive MP4 and offers it as one.
            let hls = content_type.to_ascii_lowercase().contains("mpegurl");
            let (width, height) = dimensions(url);

            let label = if hls {
                "Adaptive stream (HLS)".to_string()
            } else {
                let quality = height
                    .map(|h| format!("{h}p"))
                    .unwrap_or_else(|| "MP4".to_string());
                match bitrate {
                    Some(b) if b > 0 => format!("{quality} · {:.1} Mbps", b as f64 / 1_000_000.0),
                    _ => quality,
                }
            };

            let stream = Stream {
                url: url.to_string(),
                kind: StreamKind::Muxed,
                mime: (!content_type.is_empty()).then(|| content_type.to_string()),
                size: None,
                // video.twimg.com serves without one, but the endpoint's own widget sends
                // it and matching that is free.
                headers: vec![("Referer".into(), "https://twitter.com/".into())],
                max_chunk: None,
            };

            let id = unique(
                &mut taken,
                if hls {
                    "hls".into()
                } else {
                    format!("v{}", bitrate.unwrap_or(0))
                },
            );

            videos.push(VideoChoice {
                id,
                label: label.clone(),
                width,
                height,
                fps: None,
                bitrate,
                codec: (!hls).then(|| "H.264".to_string()),
                size: None,
                stream: stream.clone(),
                // Every variant is a complete file; X never separates picture from sound.
                has_audio: true,
                best: false,
                // Both are decided centrally by `rank_choices`.
                container: None,
                mergeable: false,
            });

            options.push(MediaOption {
                label,
                rank: u64::from(height.unwrap_or(0)),
                streams: vec![stream],
                // MP4 either way: an HLS playlist is remuxed into one by the download
                // engine, so what lands on disk has the same extension as a direct file.
                filename: safe_filename(&title, "mp4"),
                width,
                height,
                duration_ms: item
                    .get("video_info")
                    .and_then(|v| v.get("duration_millis"))
                    .and_then(Value::as_u64),
            });
        }
    }

    if videos.is_empty() {
        return Err(SiteError::Unavailable(
            "that post has no video. Images and text are not downloads this tool makes.".into(),
        ));
    }

    // Highest bitrate first; the HLS entry has none and sorts last, which is where it
    // belongs — it is the fallback for a post whose MP4s are missing.
    options.sort_by_key(|o| std::cmp::Reverse(o.rank));

    let mut extraction = Extraction {
        note: None,
        site: "X".into(),
        title,
        options,
        videos,
        audios: Vec::<AudioChoice>::new(),
        subtitles: Vec::new(),
    };
    extraction.rank_choices();
    Ok(extraction)
}

/// A title from the post's own text, falling back to the author and then the id.
fn post_title(root: &Value, handle: Option<&str>, status_id: Option<&str>) -> String {
    let text = root
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    if !text.is_empty() {
        return text.chars().take(90).collect::<String>().trim().to_string();
    }
    match (handle, status_id) {
        (Some(user), _) => format!("Post by @{user}"),
        (None, Some(id)) => format!("X post {id}"),
        _ => "X post".to_string(),
    }
}

/// Width and height out of the URL path, which is where X states them.
///
/// `.../vid/avc1/1280x720/abc.mp4` and the older `.../vid/1280x720/abc.mp4` both occur.
/// `aspect_ratio` in the JSON describes the shape but not the size, so it cannot answer
/// this on its own.
fn dimensions(url: &str) -> (Option<u32>, Option<u32>) {
    for segment in url.split('/') {
        if let Some((w, h)) = segment.split_once('x') {
            if !w.is_empty()
                && !h.is_empty()
                && w.chars().all(|c| c.is_ascii_digit())
                && h.chars().all(|c| c.is_ascii_digit())
            {
                return (w.parse().ok(), h.parse().ok());
            }
        }
    }
    (None, None)
}

fn unique(taken: &mut Vec<String>, base: String) -> String {
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    taken.push(candidate.clone());
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real 2026-09-05 capture of a post with video.
    const WITH_VIDEO: &str = r#"{
        "__typename": "Tweet",
        "id_str": "1460323737035677698",
        "text": "Introducing a new era for the Twitter Developer Platform!",
        "user": {"screen_name": "XDevelopers", "name": "Developers"},
        "mediaDetails": [{
            "type": "video",
            "video_info": {
                "aspect_ratio": [16, 9],
                "duration_millis": 61000,
                "variants": [
                    {"content_type": "application/x-mpegURL",
                     "url": "https://video.twimg.com/ext_tw_video/1/pu/pl/playlist.m3u8"},
                    {"bitrate": 256000, "content_type": "video/mp4",
                     "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/480x270/a.mp4"},
                    {"bitrate": 832000, "content_type": "video/mp4",
                     "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/640x360/b.mp4"},
                    {"bitrate": 2176000, "content_type": "video/mp4",
                     "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/1280x720/c.mp4"}
                ]
            }
        }]
    }"#;

    /// The real shape of a text-only post: same document, empty `mediaDetails`.
    const NO_VIDEO: &str = r#"{
        "__typename": "Tweet",
        "id_str": "20",
        "text": "just setting up my twttr",
        "user": {"screen_name": "jack"},
        "mediaDetails": []
    }"#;

    fn extract(body: &str) -> Result<Extraction, SiteError> {
        let mut x = Twitter::new();
        x.start("https://x.com/i/status/1460323737035677698")?;
        match x.feed(&[body])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("X asked for a second input"),
        }
    }

    #[test]
    fn the_status_id_is_found_in_every_shape_a_link_comes_in() {
        for url in [
            "https://x.com/i/status/1460323737035677698",
            "https://twitter.com/XDevelopers/status/1460323737035677698",
            "https://twitter.com/XDevelopers/status/1460323737035677698?s=20&t=abc",
            "https://x.com/XDevelopers/status/1460323737035677698/photo/1",
            "https://twitter.com/XDevelopers/statuses/1460323737035677698",
        ] {
            assert_eq!(
                status_id(url).as_deref(),
                Some("1460323737035677698"),
                "{url}"
            );
        }
        assert_eq!(status_id("https://x.com/XDevelopers"), None);
        assert_eq!(status_id("https://x.com/i/status/notanumber"), None);
    }

    #[test]
    fn a_link_naming_no_post_is_refused_with_a_readable_sentence() {
        let mut x = Twitter::new();
        let SiteError::Unavailable(message) = x.start("https://x.com/XDevelopers").unwrap_err()
        else {
            panic!("expected Unavailable");
        };
        assert!(message.contains("/status/"), "{message}");
    }

    #[test]
    fn start_asks_the_syndication_endpoint_with_the_post_id() {
        let mut x = Twitter::new();
        let Step::Need(Need::Fetch(requests)) = x
            .start("https://twitter.com/XDevelopers/status/1460323737035677698")
            .unwrap()
        else {
            panic!("expected a fetch");
        };
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.starts_with(SYNDICATION));
        assert!(requests[0].url.contains("id=1460323737035677698"));
        assert!(requests[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Referer" && v.contains("twitter.com")));
    }

    #[test]
    fn every_variant_becomes_a_choice_ranked_by_size() {
        let e = extract(WITH_VIDEO).unwrap();
        assert_eq!(e.site, "X");
        assert_eq!(
            e.title,
            "Introducing a new era for the Twitter Developer Platform!"
        );
        // Three MP4s plus the HLS fallback.
        assert_eq!(e.videos.len(), 4);
        assert_eq!(e.videos[0].height, Some(720));
        assert_eq!(e.videos[0].label, "720p · 2.2 Mbps");
        assert!(e.videos[0].best, "the largest MP4 should be recommended");
        assert_eq!(e.options[0].height, Some(720));
    }

    #[test]
    fn dimensions_come_from_the_url_because_the_json_only_gives_a_ratio() {
        assert_eq!(
            dimensions("https://video.twimg.com/ext_tw_video/1/pu/vid/1280x720/c.mp4"),
            (Some(1280), Some(720))
        );
        assert_eq!(
            dimensions("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/854x480/d.mp4"),
            (Some(854), Some(480))
        );
        assert_eq!(
            dimensions("https://video.twimg.com/pl/playlist.m3u8"),
            (None, None)
        );
    }

    #[test]
    fn every_variant_is_a_complete_file_so_nothing_is_ever_joined() {
        let e = extract(WITH_VIDEO).unwrap();
        assert!(e.audios.is_empty(), "X never separates picture from sound");
        assert!(e.videos.iter().all(|v| v.has_audio));
        assert!(e.options.iter().all(|o| o.streams.len() == 1));
    }

    #[test]
    fn the_hls_fallback_is_offered_last_rather_than_recommended() {
        let e = extract(WITH_VIDEO).unwrap();
        let hls = e
            .videos
            .iter()
            .find(|v| v.id == "hls")
            .expect("no HLS entry");
        assert!(!hls.best);
        assert_eq!(hls.label, "Adaptive stream (HLS)");
    }

    #[test]
    fn ids_are_unique_and_every_stream_carries_its_referer() {
        let e = extract(WITH_VIDEO).unwrap();
        let mut ids: Vec<&str> = e.videos.iter().map(|v| v.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "ids must be unique");
        assert!(e
            .videos
            .iter()
            .all(|v| v.stream.headers.iter().any(|(k, _)| k == "Referer")));
    }

    #[test]
    fn a_post_with_no_video_says_so_rather_than_returning_nothing() {
        let SiteError::Unavailable(message) = extract(NO_VIDEO).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(message.contains("no video"), "{message}");
    }

    #[test]
    fn a_deleted_or_protected_post_is_named_as_such() {
        let body = r#"{"__typename": "TweetTombstone", "tombstone": {"text": {"text": "gone"}}}"#;
        let SiteError::Unavailable(message) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(message.contains("deleted"), "{message}");

        let errored = r#"{"error": {"message": "Not authorized"}}"#;
        let SiteError::Unavailable(message) = extract(errored).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(message.contains("Not authorized"), "{message}");
    }

    #[test]
    fn a_post_with_no_text_is_named_after_its_author() {
        let body = r#"{
            "__typename": "Tweet", "text": "", "user": {"screen_name": "someone"},
            "mediaDetails": [{"type": "animated_gif", "video_info": {"variants": [
                {"bitrate": 1000, "content_type": "video/mp4",
                 "url": "https://video.twimg.com/tweet_video/640x360/x.mp4"}]}}]
        }"#;
        let e = extract(body).unwrap();
        assert_eq!(e.title, "Post by @someone");
        // An animated GIF is a silent MP4, and downloading it is the same operation.
        assert_eq!(e.videos.len(), 1);
    }

    #[test]
    fn a_response_that_is_not_json_is_a_shape_error_naming_the_site() {
        let mut x = Twitter::new();
        x.start("https://x.com/i/status/20").unwrap();
        assert_eq!(
            x.feed(&["<html>nope</html>"]),
            Err(SiteError::Shape("X".into()))
        );
    }
}
