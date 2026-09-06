//! Twitch clips, via the public GraphQL endpoint.
//!
//! Built against the documented shape of the `VideoAccessToken_Clip` persisted query.
//! **Not verified live** — the assertions below are hand-written fixtures in the
//! endpoint's published shape, not a recording — so a change at Twitch's end surfaces as
//! [`SiteError::Shape`] naming this module.
//!
//! # Clips only, and the honest reason why
//!
//! A clip resolves to a plain MP4: the query hands back `sourceURL` per quality plus a
//! signed access token, and gluing the two together is the whole download. A **VOD** does
//! not work that way. It needs a second signed playlist — a `PlaybackAccessToken` query
//! for the video id, then a `usher.ttvnw.net` master URL carrying that token's signature,
//! then the HLS pipeline — and a **live channel** additionally has no last byte to
//! download to. This build requests neither, so both are refused with a sentence saying
//! precisely that rather than half-implemented into something that fails at the fetch.
//!
//! # The client id is not a credential
//!
//! `kimne78kx3ncx6brgo4mv6wki5h1ko` is the public web client id Twitch's own player sends
//! on every unauthenticated request; it is in the page source of every twitch.tv URL. It
//! identifies the client, it authorises nothing, and no account is involved anywhere in
//! this module.

use serde_json::Value;

use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, Request, SiteError, Step,
    Stream, StreamKind, VideoChoice,
};

/// Twitch's clip CDN serves media to Twitch's own pages.
const REFERER: &str = "https://www.twitch.tv/";

/// Twitch's public web client id. See the module header: this is not a secret.
pub const WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

const GQL_ENDPOINT: &str = "https://gql.twitch.tv/gql";

/// The persisted-query hash for `VideoAccessToken_Clip`. Twitch accepts the operation
/// only by hash, so this is as load-bearing as the operation name beside it.
const CLIP_QUERY_HASH: &str = "36b89d2507fce29e5ca551df756d27c1cfe079e2609642b4390aa4c35796eb11";

const CLAIMED: &[&str] = &["twitch.tv", "clips.twitch.tv", "ttvnw.net"];

pub fn matches(host: &str) -> bool {
    CLAIMED.iter().any(|d| host_is(host, d))
}

/// What a Twitch URL turned out to be.
///
/// Named rather than folded into an `Option<String>` because the two refusals below say
/// different things to the user, and losing that distinction at parse time would make it
/// impossible to say either.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Clip(String),
    /// `twitch.tv/videos/<id>`.
    Vod,
    /// A channel page — live or otherwise.
    Channel,
}

#[derive(Debug, Default)]
pub struct Twitch {
    /// Kept from `start` so a response with no `title` still names its file after
    /// something the user can recognise.
    slug: Option<String>,
}

impl Twitch {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Twitch {
    fn site(&self) -> &'static str {
        "Twitch"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        match target(url) {
            Some(Target::Clip(slug)) => {
                let request = clip_request(&slug);
                self.slug = Some(slug);
                Ok(Step::Need(Need::Fetch(vec![request])))
            }
            Some(Target::Vod) => Err(SiteError::Unavailable(VOD_REFUSAL.into())),
            Some(Target::Channel) => Err(SiteError::Unavailable(CHANNEL_REFUSAL.into())),
            None => Err(SiteError::Unavailable(
                "this Twitch link does not name a clip — open the clip itself and try again".into(),
            )),
        }
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().ok_or_else(shape)?;
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        parse_clip_response(&root, self.slug.as_deref()).map(Step::Done)
    }
}

/// The exact wording matters here: it is the whole of what the user learns, and "not
/// supported" without the reason invites the same bug report every week.
const VOD_REFUSAL: &str =
    "Twitch VODs need a signed HLS playlist that this build does not request, so a past \
     broadcast cannot be saved here. Twitch clips are supported.";

const CHANNEL_REFUSAL: &str =
    "this is a Twitch channel rather than a clip. A live broadcast has no last byte to \
     download to, and past broadcasts need a signed playlist this build does not request. \
     Twitch clips are supported.";

fn shape() -> SiteError {
    SiteError::Shape("Twitch".into())
}

/// Build the one request this extractor makes.
///
/// The body is assembled by hand rather than through `serde_json` because the slug has
/// already been validated as URL-safe characters, so nothing in it can escape a JSON
/// string, and the literal keeps the wire format readable next to the fixture.
fn clip_request(slug: &str) -> Request {
    let body = format!(
        concat!(
            r#"[{{"operationName":"VideoAccessToken_Clip","variables":{{"slug":"{}"}},"#,
            r#""extensions":{{"persistedQuery":{{"version":1,"sha256Hash":"{}"}}}}}}]"#
        ),
        slug, CLIP_QUERY_HASH
    );
    Request::post(GQL_ENDPOINT, body).with_header("Client-ID", WEB_CLIENT_ID)
}

/// Work out what a Twitch URL points at.
fn target(url: &str) -> Option<Target> {
    let host = authority_of(url).to_ascii_lowercase();
    let path = path_of(url);
    let query = query_of(url);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // `clips.twitch.tv/<slug>` and the embed player's `?clip=<slug>`.
    if host_is(&host, "clips.twitch.tv") {
        if segments.first() == Some(&"embed") {
            return query_param(query, "clip")
                .and_then(valid_slug)
                .map(Target::Clip);
        }
        return segments
            .first()
            .copied()
            .and_then(valid_slug)
            .map(Target::Clip);
    }

    // `twitch.tv/<channel>/clip/<slug>`.
    if let Some(index) = segments.iter().position(|s| *s == "clip") {
        return segments
            .get(index + 1)
            .copied()
            .and_then(valid_slug)
            .map(Target::Clip);
    }

    match segments.first().copied() {
        Some("videos") => Some(Target::Vod),
        // Directory, settings and the rest are not channels, but they are not clips
        // either, and the channel refusal already names clips as what does work.
        Some(_) => Some(Target::Channel),
        None => None,
    }
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

fn query_of(url: &str) -> &str {
    url.split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or(q))
        .unwrap_or("")
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Clip slugs are the concatenated-words form Twitch generates plus, on newer clips, a
/// dash and a random tail. Anything outside that alphabet is not a slug, and refusing it
/// here keeps a stray path segment from being posted to the GraphQL endpoint.
fn valid_slug(candidate: &str) -> Option<String> {
    let slug = candidate.split(['?', '#']).next().unwrap_or(candidate);
    let ok = !slug.is_empty()
        && slug.len() <= 100
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| slug.to_string())
}

fn parse_clip_response(root: &Value, slug: Option<&str>) -> Result<Extraction, SiteError> {
    // The endpoint answers a batch, so the payload arrives wrapped in a one-element
    // array. It answers a bare object to a non-batched request, and both shapes turn up
    // in the wild depending on how the query was sent.
    let payload = match root {
        Value::Array(items) => items.first().ok_or_else(shape)?,
        other => other,
    };

    if let Some(message) = graphql_error(payload) {
        return Err(SiteError::Unavailable(message));
    }

    let data = payload.get("data").ok_or_else(shape)?;
    let clip = data.get("clip").ok_or_else(shape)?;
    if clip.is_null() {
        return Err(SiteError::Unavailable(
            "Twitch has no clip with that name — it may have been deleted, or the link may be \
             mistyped"
                .into(),
        ));
    }

    let title = clip
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            clip.pointer("/broadcaster/displayName")
                .and_then(Value::as_str)
                .map(|name| format!("Clip of {name}"))
        })
        .or_else(|| slug.map(str::to_string))
        .unwrap_or_else(|| "clip".to_string());
    let duration_ms = clip
        .get("durationSeconds")
        .and_then(Value::as_f64)
        .filter(|d| *d > 0.0)
        .map(|d| (d * 1000.0) as u64);

    // Without the token the CDN answers 403. Producing options that cannot be fetched
    // would be worse than saying the token is missing, so this is a hard requirement
    // rather than an enrichment.
    let token = clip.get("playbackAccessToken").ok_or_else(shape)?;
    let signature = token
        .get("signature")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(shape)?;
    let value = token
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(shape)?;

    let mut qualities: Vec<Quality> = clip
        .get("videoQualities")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Quality::parse).collect())
        .unwrap_or_default();
    // Tallest first. `videoQualities` arrives in whatever order the API felt like, and
    // the ranking below cannot reorder these for itself — see the note on `width`.
    qualities.sort_by(|a, b| {
        b.height
            .cmp(&a.height)
            .then(b.frame_rate.unwrap_or(0).cmp(&a.frame_rate.unwrap_or(0)))
    });

    if qualities.is_empty() {
        return Err(SiteError::Unavailable(
            "Twitch listed no downloadable qualities for this clip".into(),
        ));
    }

    let mut options = Vec::new();
    let mut videos = Vec::new();
    for (index, quality) in qualities.iter().enumerate() {
        let stream = Stream {
            url: signed_url(&quality.source_url, signature, value),
            kind: StreamKind::Muxed,
            mime: Some("video/mp4".into()),
            size: None,
            headers: vec![("Referer".into(), REFERER.into())],
            max_chunk: None,
        };
        options.push(MediaOption {
            label: quality.label(),
            rank: u64::from(quality.height.unwrap_or(0)).max(1),
            streams: vec![stream.clone()],
            filename: safe_filename(&title, "mp4"),
            width: None,
            height: quality.height,
            duration_ms,
        });
        videos.push(VideoChoice {
            // Both are decided centrally by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
            id: format!("clip-{index}"),
            label: quality.label(),
            // Twitch names a clip quality by height and never states a width, so there is
            // no honest width to report. `rank_choices` compares pixel counts and ties
            // all of these at zero as a result, which is why they are sorted tallest-first
            // above: the sort there is stable, so insertion order is what orders them.
            width: None,
            height: quality.height,
            fps: quality.frame_rate,
            bitrate: None,
            codec: None,
            size: None,
            stream,
            // A clip is a finished MP4 with its sound in it.
            has_audio: true,
            best: false,
        });
    }

    options.sort_by_key(|o| core::cmp::Reverse(o.rank));

    let mut extraction = Extraction {
        site: "Twitch".into(),
        title,
        options,
        videos,
        // A clip has no separate audio rendition.
        audios: Vec::new(),
        subtitles: Vec::new(),
    };
    extraction.rank_choices();
    Ok(extraction)
}

/// One entry of `videoQualities`.
#[derive(Debug, Clone)]
struct Quality {
    height: Option<u32>,
    frame_rate: Option<u32>,
    source_url: String,
}

impl Quality {
    fn parse(v: &Value) -> Option<Self> {
        let source_url = v.get("sourceURL")?.as_str()?.trim();
        if source_url.is_empty() {
            return None;
        }
        Some(Self {
            // `quality` is the height as a bare string: "1080", "720", "480", "360".
            height: v
                .get("quality")
                .and_then(Value::as_str)
                .and_then(|q| q.trim().parse().ok()),
            frame_rate: v
                .get("frameRate")
                .and_then(Value::as_f64)
                .filter(|f| *f > 0.0)
                .map(|f| f.round() as u32),
            source_url: source_url.to_string(),
        })
    }

    /// "1080p60 · MP4", the same shape every other site in this crate writes.
    fn label(&self) -> String {
        let quality = match self.height {
            Some(h) => format!("{h}p"),
            None => "video".to_string(),
        };
        let quality = match self.frame_rate {
            Some(fps) if fps > 30 => format!("{quality}{fps}"),
            _ => quality,
        };
        format!("{quality} · MP4")
    }
}

/// Glue the access token onto a clip's source URL.
///
/// The signature is a hex digest and needs no escaping; the token is a JSON document full
/// of braces, quotes and colons, and pasting it in raw produces a URL the CDN rejects.
fn signed_url(source_url: &str, signature: &str, token: &str) -> String {
    let separator = if source_url.contains('?') { '&' } else { '?' };
    format!(
        "{source_url}{separator}sig={}&token={}",
        percent_encode(signature),
        percent_encode(token)
    )
}

/// Percent-encode everything outside RFC 3986's unreserved set.
///
/// Hand-written rather than pulled in as a dependency: this crate compiles to wasm and
/// every added crate is added to the extension's shipped bytes, and the whole of what is
/// needed here is twenty lines with a test beside it.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => {
                out.push('%');
                out.push(hex_digit(other >> 4));
                out.push(hex_digit(other & 0x0f));
            }
        }
    }
    out
}

/// Upper-case, because that is the form RFC 3986 says to produce.
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

/// GraphQL puts failures in a sibling of `data` rather than in a status code, so a
/// perfectly successful-looking 200 can carry nothing but an error.
fn graphql_error(payload: &Value) -> Option<String> {
    let errors = payload.get("errors")?.as_array()?;
    let message = errors
        .iter()
        .find_map(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
        })
        .unwrap_or("Twitch refused this clip");
    Some(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the batched persisted query answers with.
    const CLIP: &str = r#"[{
        "data": {
            "clip": {
                "id": "1234567890",
                "slug": "AwkwardHelplessSalamanderSwiftRage",
                "title": "An improbable shot",
                "durationSeconds": 28,
                "broadcaster": {"displayName": "somestreamer"},
                "videoQualities": [
                    {"__typename": "ClipVideoQuality", "frameRate": 30, "quality": "480",
                     "sourceURL": "https://production.assets.clips.twitchcdn.net/v2/480.mp4"},
                    {"__typename": "ClipVideoQuality", "frameRate": 60, "quality": "1080",
                     "sourceURL": "https://production.assets.clips.twitchcdn.net/v2/1080.mp4"},
                    {"__typename": "ClipVideoQuality", "frameRate": 60, "quality": "720",
                     "sourceURL": "https://production.assets.clips.twitchcdn.net/v2/720.mp4"}
                ],
                "playbackAccessToken": {
                    "__typename": "PlaybackAccessToken",
                    "signature": "6b1f0b2a9c",
                    "value": "{\"clip_uri\":\"x\",\"expires\":123}"
                }
            }
        },
        "extensions": {"durationMilliseconds": 41}
    }]"#;

    fn extract(body: &str) -> Result<Extraction, SiteError> {
        let mut twitch = Twitch::new();
        twitch
            .start("https://www.twitch.tv/somestreamer/clip/AwkwardHelplessSalamanderSwiftRage")
            .unwrap();
        match twitch.feed(&[body])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("the extractor asked for a second fetch"),
        }
    }

    fn labels(e: &Extraction) -> Vec<String> {
        e.options.iter().map(|o| o.label.clone()).collect()
    }

    #[test]
    fn every_shape_of_clip_link_yields_the_same_slug() {
        let expected = Some(Target::Clip("AwkwardHelplessSalamanderSwiftRage".into()));
        assert_eq!(
            target("https://www.twitch.tv/somestreamer/clip/AwkwardHelplessSalamanderSwiftRage"),
            expected
        );
        assert_eq!(
            target("https://clips.twitch.tv/AwkwardHelplessSalamanderSwiftRage"),
            expected
        );
        assert_eq!(
            target(
                "https://m.twitch.tv/somestreamer/clip/AwkwardHelplessSalamanderSwiftRage?filter=clips"
            ),
            expected
        );
        assert_eq!(
            target(
                "https://clips.twitch.tv/embed?clip=AwkwardHelplessSalamanderSwiftRage&parent=x"
            ),
            expected
        );
    }

    #[test]
    fn a_vod_is_refused_with_the_reason_rather_than_half_implemented() {
        assert_eq!(
            target("https://www.twitch.tv/videos/123456789"),
            Some(Target::Vod)
        );
        let err = Twitch::new()
            .start("https://www.twitch.tv/videos/123456789")
            .unwrap_err();
        let SiteError::Unavailable(msg) = err else {
            panic!("expected Unavailable, got {err:?}");
        };
        assert_eq!(msg, VOD_REFUSAL);
        assert!(msg.contains("signed HLS playlist"), "{msg}");
        assert!(msg.contains("clips are supported"), "{msg}");
    }

    #[test]
    fn a_channel_page_says_why_a_live_stream_is_not_a_download() {
        assert_eq!(
            target("https://www.twitch.tv/somestreamer"),
            Some(Target::Channel)
        );
        let SiteError::Unavailable(msg) = Twitch::new()
            .start("https://www.twitch.tv/somestreamer")
            .unwrap_err()
        else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("no last byte"), "{msg}");
    }

    #[test]
    fn a_bare_host_names_nothing_at_all() {
        assert_eq!(target("https://www.twitch.tv/"), None);
        let SiteError::Unavailable(msg) =
            Twitch::new().start("https://www.twitch.tv/").unwrap_err()
        else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("does not name a clip"), "{msg}");
    }

    #[test]
    fn the_only_request_is_the_persisted_clip_query() {
        let mut twitch = Twitch::new();
        let step = twitch
            .start("https://clips.twitch.tv/AwkwardHelplessSalamanderSwiftRage")
            .unwrap();
        let Step::Need(Need::Fetch(reqs)) = step else {
            panic!("expected a fetch");
        };
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].url, GQL_ENDPOINT);
        assert!(reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Client-ID" && v == WEB_CLIENT_ID));
        // It must be valid JSON in the batch shape, not merely a plausible string.
        let body = reqs[0].body.as_deref().unwrap();
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed[0]["operationName"], "VideoAccessToken_Clip");
        assert_eq!(
            parsed[0]["variables"]["slug"],
            "AwkwardHelplessSalamanderSwiftRage"
        );
        assert_eq!(
            parsed[0]["extensions"]["persistedQuery"]["sha256Hash"],
            CLIP_QUERY_HASH
        );
        assert_eq!(parsed[0]["extensions"]["persistedQuery"]["version"], 1);
    }

    #[test]
    fn a_clip_yields_one_option_per_quality_tallest_first() {
        let e = extract(CLIP).unwrap();
        assert_eq!(e.site, "Twitch");
        assert_eq!(e.title, "An improbable shot");
        assert_eq!(
            labels(&e),
            vec!["1080p60 · MP4", "720p60 · MP4", "480p · MP4"]
        );
        assert_eq!(e.options[0].height, Some(1080));
        assert_eq!(e.options[0].duration_ms, Some(28_000));
        assert!(e.options[0].filename.ends_with(".mp4"));
    }

    #[test]
    fn the_download_url_is_the_source_with_the_token_appended_and_escaped() {
        let e = extract(CLIP).unwrap();
        assert_eq!(
            e.options[0].streams[0].url,
            "https://production.assets.clips.twitchcdn.net/v2/1080.mp4\
             ?sig=6b1f0b2a9c&token=%7B%22clip_uri%22%3A%22x%22%2C%22expires%22%3A123%7D"
        );
    }

    #[test]
    fn a_source_url_that_already_has_a_query_gets_an_ampersand_not_a_second_question_mark() {
        let signed = signed_url("https://cdn.tv/clip.mp4?v=2", "abc", "tok en");
        assert_eq!(signed, "https://cdn.tv/clip.mp4?v=2&sig=abc&token=tok%20en");
        assert_eq!(signed.matches('?').count(), 1);
    }

    #[test]
    fn percent_encoding_escapes_everything_a_json_token_contains() {
        assert_eq!(
            percent_encode(r#"{"a":"b c","d":[1,2]}"#),
            "%7B%22a%22%3A%22b%20c%22%2C%22d%22%3A%5B1%2C2%5D%7D"
        );
        // The unreserved set survives untouched, so a signature is not mangled.
        assert_eq!(percent_encode("aZ09-_.~"), "aZ09-_.~");
        // Multi-byte characters are encoded per UTF-8 byte, upper-case, as RFC 3986 says.
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("/"), "%2F");
    }

    #[test]
    fn exactly_one_video_choice_is_marked_best_and_it_is_the_tallest() {
        let e = extract(CLIP).unwrap();
        assert_eq!(e.videos.iter().filter(|v| v.best).count(), 1);
        assert_eq!(e.best_video().unwrap().height, Some(1080));
        assert_eq!(e.best_video().unwrap().fps, Some(60));
        assert!(e.audios.is_empty(), "a clip is a single muxed file");
    }

    #[test]
    fn every_video_choice_has_a_unique_id_and_says_it_already_has_sound() {
        let e = extract(CLIP).unwrap();
        let mut ids: Vec<&str> = e.videos.iter().map(|v| v.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate video choice id");
        assert!(e.videos.iter().all(|v| v.has_audio));
    }

    #[test]
    fn every_stream_carries_the_referer_twitch_wants() {
        let e = extract(CLIP).unwrap();
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
    fn a_deleted_clip_is_reported_as_unavailable_rather_than_as_a_shape_error() {
        let body = r#"[{"data": {"clip": null}, "extensions": {}}]"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("no clip with that name"), "{msg}");
    }

    #[test]
    fn a_graphql_error_is_reported_in_twitchs_own_words() {
        let body = r#"[{"errors": [{"message": "service timeout"}], "data": null}]"#;
        assert_eq!(
            extract(body).unwrap_err(),
            SiteError::Unavailable("service timeout".into())
        );
    }

    #[test]
    fn an_unbatched_object_response_parses_the_same_way_as_the_array() {
        let unbatched: Value = serde_json::from_str(CLIP).unwrap();
        let e = extract(&unbatched[0].to_string()).unwrap();
        assert_eq!(e.options.len(), 3);
    }

    #[test]
    fn a_clip_with_no_access_token_is_a_shape_error_rather_than_a_broken_url() {
        let mut v: Value = serde_json::from_str(CLIP).unwrap();
        v[0]["data"]["clip"]["playbackAccessToken"] = Value::Null;
        assert_eq!(extract(&v.to_string()).unwrap_err(), shape());
    }

    #[test]
    fn a_clip_with_no_qualities_says_so() {
        let mut v: Value = serde_json::from_str(CLIP).unwrap();
        v[0]["data"]["clip"]["videoQualities"] = serde_json::json!([]);
        let SiteError::Unavailable(msg) = extract(&v.to_string()).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("no downloadable qualities"), "{msg}");
    }

    #[test]
    fn a_clip_with_no_title_is_named_after_its_broadcaster() {
        let mut v: Value = serde_json::from_str(CLIP).unwrap();
        v[0]["data"]["clip"]["title"] = Value::String(String::new());
        let e = extract(&v.to_string()).unwrap();
        assert_eq!(e.title, "Clip of somestreamer");
    }

    #[test]
    fn a_title_with_a_slash_cannot_escape_the_download_directory() {
        let mut v: Value = serde_json::from_str(CLIP).unwrap();
        v[0]["data"]["clip"]["title"] = Value::String("AC/DC — live".into());
        let e = extract(&v.to_string()).unwrap();
        assert!(
            !e.options[0].filename.contains('/'),
            "{}",
            e.options[0].filename
        );
    }

    #[test]
    fn a_body_that_is_not_a_clip_response_at_all_is_a_shape_error() {
        assert_eq!(extract("not json").unwrap_err(), shape());
        assert_eq!(extract("[]").unwrap_err(), shape());
        assert_eq!(extract("{}").unwrap_err(), shape());
        assert_eq!(Twitch::new().feed(&[]).unwrap_err(), shape());
    }

    #[test]
    fn only_twitchs_own_hosts_match() {
        assert!(matches("twitch.tv"));
        assert!(matches("www.twitch.tv"));
        assert!(matches("m.twitch.tv"));
        assert!(matches("clips.twitch.tv"));
        assert!(matches("usher.ttvnw.net"));
        assert!(!matches("nottwitch.tv"));
        assert!(!matches("twitch.tv.evil.test"));
    }
}
