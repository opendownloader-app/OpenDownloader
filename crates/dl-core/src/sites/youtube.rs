//! YouTube, via the iOS InnerTube player endpoint.
//!
//! # Why there is no signature decipherer here
//!
//! Every YouTube downloader ever written contains one: a little JavaScript interpreter
//! that reads `base.js`, finds the transform function, and unscrambles
//! `signatureCipher`. As of a live check on 2026-09-05 that code would be dead weight.
//! The web page's `ytInitialPlayerResponse.streamingData` no longer carries format URLs
//! at all — the adaptive formats arrive with **no `url` and no `signatureCipher`**, next
//! to a `serverAbrStreamingUrl`. The web player now asks the server for byte ranges over
//! SABR rather than fetching a rendition itself, so there is no signature left to solve.
//!
//! # What does work
//!
//! The iOS client's `youtubei/v1/player` response still hands back plain URLs: verified
//! 22 video and 10 audio formats, none carrying an `n` throttle parameter, all honouring
//! `Range` (`206` with a correct `Content-Range`). So this extractor makes exactly one
//! request, pretending — accurately, in terms of what it then does with the answer — to
//! be the iOS app.
//!
//! The catch is that those URLs are *issued to* the iOS client. Fetching them with a
//! desktop browser's User-Agent can be rejected, so every [`Stream`] this module produces
//! carries [`IOS_USER_AGENT`] in its headers, and the host must send it verbatim.

use serde_json::Value;

// The label primitives live in `bilibili`, which is where this directory keeps the helpers
// more than one site needs — see the header of that file. Two implementations of "how a
// bitrate reads" is exactly the divergence that makes one picker look unlike the next.
use super::bilibili::{bitrate_text, friendly_codec, id_token, join_parts, unique_id};
use super::{
    host_is, safe_filename, AudioChoice, Extraction, Extractor, MediaOption, Need, Request,
    SiteError, Step, Stream, StreamKind, SubtitleTrack, VideoChoice,
};

/// The User-Agent the format URLs are issued to. Sending anything else risks a 403, so
/// it is attached to every stream rather than left to the host to remember.
/// How much of a Google media URL to ask for in one request.
///
/// 1 MiB, because that is what YouTube's own player asks for. Requesting 8 MiB — the
/// engine's ordinary chunk — is answered `403`, which reads like an authorisation
/// failure rather than a size complaint and is worth knowing before debugging it.
///
/// # The wall past the first couple of megabytes
///
/// These URLs serve **only their first ~2 MiB**. Offsets 0 and 1 MiB answer `206`; 5, 10,
/// 15, 25 and 30 MiB all answer `403`, on a freshly issued URL, at a 64 KiB read size,
/// with the highest offset requested *first* so that neither request count nor bytes
/// already transferred can explain it. It is a property of the offset and nothing else.
///
/// An earlier version of this note recorded the same measurement from a datacenter IP and
/// concluded it was Google declining to serve bulk media to datacenters, adding that "a
/// browser on an ordinary connection... streams to completion". **That is not true, and
/// it sent a user chasing their own network.** Re-measured on 2026-09-07 from an ordinary
/// domestic connection: the wall is identical. The error text it justified told people to
/// try again from a home connection, which is where they already were.
///
/// What changed is how YouTube delivers video. `streamingData.formats` — the muxed
/// progressive entries — now comes back **empty**, `serverAbrStreamingUrl` is present, and
/// the `adaptiveFormats` URLs are vestigial: enough to start playback, not enough to read
/// a file. Checked against three unrelated videos, all the same shape.
///
/// So a long video cannot be completed through these addresses, by any chunk size, retry
/// schedule or connection. Reading the rest means speaking the newer streaming protocol,
/// which is a deliberate restriction on Google's side rather than a gap in this code, and
/// is not something this extractor tries to talk its way around.
///
/// The chunk cap below is still correct and still required: 8 MiB is refused outright, so
/// even the part that does serve needs 1 MiB reads. What the `403` handling can do is
/// retry a genuine throttle and then say plainly what happened — see the message in
/// `fetch-retry.ts`, which now describes this instead of blaming the connection.
pub const MAX_RANGE_BYTES: u64 = 1024 * 1024;

pub const IOS_USER_AGENT: &str =
    "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X)";

/// The InnerTube player endpoint. The key is the long-published public web/iOS API key —
/// it identifies the client, it is not a credential.
const PLAYER_ENDPOINT: &str = "https://www.youtube.com/youtubei/v1/player?key=AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8&prettyPrint=false";

/// Client version and device, kept next to the User-Agent because the two must agree —
/// InnerTube cross-checks them and answers `LOGIN_REQUIRED` when they do not.
const IOS_CLIENT_VERSION: &str = "20.10.4";
const IOS_DEVICE_MODEL: &str = "iPhone16,2";
const IOS_OS_VERSION: &str = "18.3.2.22D82";

/// A YouTube video id is always eleven characters of the URL-safe base64 alphabet.
const VIDEO_ID_LEN: usize = 11;

pub fn matches(host: &str) -> bool {
    host_is(host, "youtube.com")
        || host_is(host, "youtu.be")
        || host_is(host, "youtube-nocookie.com")
}

#[derive(Debug, Default)]
pub struct YouTube {
    /// Kept from `start` so a response with no `videoDetails.title` still names its file
    /// after something the user can recognise.
    video_id: Option<String>,
}

impl YouTube {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for YouTube {
    fn site(&self) -> &'static str {
        "YouTube"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        let id = video_id(url).ok_or_else(|| {
            SiteError::Unavailable(
                "this YouTube link does not name a video — open the video itself and try again"
                    .into(),
            )
        })?;
        let request = player_request(&id);
        self.video_id = Some(id);
        Ok(Step::Need(Need::Fetch(vec![request])))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().ok_or_else(shape)?;
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        parse_player_response(&root, self.video_id.as_deref()).map(Step::Done)
    }
}

fn shape() -> SiteError {
    SiteError::Shape("YouTube".into())
}

/// Build the one request this extractor makes.
///
/// The body is assembled by hand rather than through `serde_json` because the id has
/// already been validated as eleven URL-safe base64 characters, so nothing in it can
/// escape a JSON string, and the literal keeps the wire format readable next to the
/// captured fixture.
fn player_request(video_id: &str) -> Request {
    let body = format!(
        concat!(
            r#"{{"context":{{"client":{{"clientName":"IOS","clientVersion":"{}","#,
            r#""deviceModel":"{}","osName":"iOS","osVersion":"{}"}}}},"#,
            r#""videoId":"{}","contentCheckOk":true,"racyCheckOk":true}}"#
        ),
        IOS_CLIENT_VERSION, IOS_DEVICE_MODEL, IOS_OS_VERSION, video_id
    );
    // `Origin` matters as much as the user agent: InnerTube answers 403 to every origin
    // but YouTube's own, which includes the `chrome-extension://…` an extension page
    // sends by default. Both headers are on the Fetch standard's forbidden list, so the
    // host applies them out of band — see `ExtractOptions.applyRequestHeaders`.
    Request::post(PLAYER_ENDPOINT, body)
        .with_header("User-Agent", IOS_USER_AGENT)
        .with_header("Origin", "https://www.youtube.com")
}

/// Pull the eleven-character video id out of any shape of YouTube link.
///
/// The query parameter is tried first because `watch?v=` is the canonical form and the
/// path markers below can also appear in a playlist or channel URL that merely *mentions*
/// a video.
pub fn video_id(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let (authority, rest) = match after_scheme.find(['/', '?', '#']) {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, ""),
    };
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let query = rest
        .split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or(q))
        .unwrap_or("");

    if let Some(id) = query_param(query, "v").and_then(valid_id) {
        return Some(id);
    }

    // `/shorts/ID`, `/embed/ID`, `/live/ID`, and the legacy `/v/ID`.
    for marker in ["/shorts/", "/embed/", "/live/", "/v/"] {
        if let Some(after) = path.split_once(marker).map(|(_, r)| r) {
            if let Some(id) = first_segment(after).and_then(valid_id) {
                return Some(id);
            }
        }
    }

    // youtu.be puts the id where a path would be: `youtu.be/ID`.
    if host_is(&authority.to_ascii_lowercase(), "youtu.be") {
        if let Some(id) = first_segment(path.trim_start_matches('/')).and_then(valid_id) {
            return Some(id);
        }
    }

    None
}

fn first_segment(s: &str) -> Option<&str> {
    let seg = s.split(['/', '?', '#', '&']).next()?;
    (!seg.is_empty()).then_some(seg)
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Accept only what YouTube actually issues, so a stray `v=PLxxxx` playlist id or a
/// truncated share link fails loudly at `start` instead of fetching a 404.
fn valid_id(candidate: &str) -> Option<String> {
    let id = candidate.split(['&', '#']).next().unwrap_or(candidate);
    let ok = id.len() == VIDEO_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| id.to_string())
}

/// One entry of `adaptiveFormats` or `formats` that is actually fetchable.
#[derive(Debug, Clone)]
struct Format {
    /// YouTube's own number for this rendition, and the only identifier it publishes that
    /// is stable across a response. It becomes the choice id, so a UI can name the exact
    /// pairing the user picked.
    itag: u64,
    url: String,
    /// The full `mimeType`, codecs parameter included — displayed, never parsed for
    /// decisions beyond the container/codec checks below.
    mime: String,
    bitrate: u64,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    quality_label: Option<String>,
    size: Option<u64>,
    /// The alternate-language track this format belongs to, when the video has more than
    /// one. Absent on the overwhelming majority of videos, which have a single soundtrack.
    track_name: Option<String>,
    track_id: Option<String>,
    /// A dynamic-range-compressed duplicate of another audio track. YouTube ships these
    /// alongside the originals with the same itag, so they must be told apart or half the
    /// audio picks land on the loudness-squashed copy.
    drc: bool,
}

impl Format {
    /// `None` for an entry with no `url`: that is a SABR-only format, playable by the
    /// server-driven player and by nothing else. Skipping it here is the difference
    /// between offering nine options and offering nine broken ones.
    fn parse(v: &Value) -> Option<Self> {
        let url = v.get("url")?.as_str()?.trim();
        if url.is_empty() {
            return None;
        }
        Some(Self {
            itag: number(v.get("itag")).unwrap_or(0),
            url: url.to_string(),
            mime: v
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            bitrate: number(v.get("bitrate")).unwrap_or(0),
            width: number(v.get("width")).map(|n| n as u32),
            height: number(v.get("height")).map(|n| n as u32),
            fps: number(v.get("fps")).map(|n| n as u32),
            quality_label: v
                .get("qualityLabel")
                .and_then(Value::as_str)
                .map(str::to_string),
            size: number(v.get("contentLength")),
            track_name: nonempty(v.pointer("/audioTrack/displayName")),
            track_id: nonempty(v.pointer("/audioTrack/id")),
            // Two spellings of the same fact. `isDrc` is the modern one; `xtags` carries
            // `drc` on responses that predate it, and either means "the loudness-squashed
            // duplicate", which nobody chose and nobody wants offered twice.
            drc: v.get("isDrc").and_then(Value::as_bool).unwrap_or(false)
                || v.get("xtags")
                    .and_then(Value::as_str)
                    .is_some_and(|x| x.to_ascii_lowercase().contains("drc")),
        })
    }

    fn is_video(&self) -> bool {
        self.mime.starts_with("video/")
    }

    fn is_audio(&self) -> bool {
        self.mime.starts_with("audio/")
    }

    fn is_mp4(&self) -> bool {
        self.mime.starts_with("video/mp4") || self.mime.starts_with("audio/mp4")
    }

    /// AVC beats AV1 at the same height: the merger and every player on the user's
    /// machine handle AVC, while AV1 decoding is still hardware-dependent.
    fn codec_rank(&self) -> u8 {
        if self.mime.contains("avc1") {
            2
        } else if self.mime.contains("av01") {
            1
        } else {
            0
        }
    }

    /// "1080p60" when YouTube says so, otherwise derived from the height.
    fn quality(&self) -> String {
        match (&self.quality_label, self.height) {
            (Some(label), _) if !label.is_empty() => label.clone(),
            (_, Some(h)) => format!("{h}p"),
            _ => "video".to_string(),
        }
    }

    fn kbps(&self) -> u64 {
        self.bitrate / 1000
    }

    fn stream(&self, kind: StreamKind) -> Stream {
        Stream {
            url: self.url.clone(),
            kind,
            mime: (!self.mime.is_empty()).then(|| self.mime.clone()),
            size: self.size,
            // See the module header: these URLs belong to the iOS client and a
            // mismatched User-Agent is grounds for a 403.
            headers: vec![("User-Agent".into(), IOS_USER_AGENT.into())],
            max_chunk: Some(MAX_RANGE_BYTES),
        }
    }

    /// A name for the codec a person can act on: at the same height the codec *is* the
    /// choice being made, and "avc1.640028" does not tell anyone which one is safe to play.
    fn codec(&self) -> Option<String> {
        codecs_of(&self.mime).map(friendly_codec)
    }

    /// The language this soundtrack is in, in YouTube's own words where it gives any.
    ///
    /// `displayName` is written for viewers ("English", "Deutsch (Original)"); the `id` is
    /// machine-shaped (`en.4`) and is only used when the display name is missing.
    fn language(&self) -> Option<String> {
        self.track_name.clone().or_else(|| {
            self.track_id
                .as_deref()
                .and_then(|id| id.split('.').next())
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        })
    }

    fn video_choice(&self, taken: &mut Vec<String>, has_audio: bool) -> VideoChoice {
        let codec = self.codec();
        VideoChoice {
            id: unique_id(taken, format!("v{}", self.itag)),
            label: join_parts(&[
                Some(self.quality()),
                codec.clone(),
                bitrate_text(self.bitrate),
            ]),
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate: (self.bitrate > 0).then_some(self.bitrate),
            codec,
            size: self.size,
            stream: self.stream(if has_audio {
                StreamKind::Muxed
            } else {
                StreamKind::VideoOnly
            }),
            has_audio,
            best: false,
            // Decided centrally by `rank_choices`, from the mime this stream carries.
            container: None,
            mergeable: false,
            // Both are decided centrally by `rank_choices`, from the mime this
            // stream actually carries — an extractor guessing at them is how two
            // sites end up disagreeing about what can be joined.
        }
    }

    fn audio_choice(&self, taken: &mut Vec<String>) -> AudioChoice {
        let codec = self.codec();
        let language = self.language();
        // One itag covers every language of a multi-track video, so the track has to go
        // into the id as well or the English and the Spanish 140 answer to the same name.
        let base = match &self.track_id {
            Some(track) => format!("a{}-{}", self.itag, id_token(track)),
            None => format!("a{}", self.itag),
        };
        AudioChoice {
            id: unique_id(taken, base),
            label: join_parts(&[language.clone(), bitrate_text(self.bitrate), codec.clone()]),
            bitrate: (self.bitrate > 0).then_some(self.bitrate),
            codec,
            language,
            size: self.size,
            stream: self.stream(StreamKind::AudioOnly),
            best: false,
            // Decided centrally by `rank_choices`, from the mime this stream carries.
            container: None,
            mergeable: false,
        }
    }
}

/// A non-empty string at a JSON pointer, or nothing.
fn nonempty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The codecs parameter of a `mimeType`: `video/mp4; codecs="avc1.640028"` → `avc1.640028`.
fn codecs_of(mime: &str) -> Option<&str> {
    let after = mime.split("codecs=").nth(1)?;
    let unquoted = after.trim().trim_start_matches('"');
    let value = unquoted.split(['"', ';']).next().unwrap_or(unquoted).trim();
    (!value.is_empty()).then_some(value)
}

/// Every fetchable video rendition, adaptive and muxed alike.
///
/// Unlike [`MediaOption`], this list is not filtered down to what the merger can pair: a
/// VP9 or AV1 rendition is a legitimate thing to want on its own, and refusing to *show* it
/// because the one-click path cannot use it is the gap these lists exist to close.
fn video_choices(adaptive: &[&Format], muxed: &[Format]) -> Vec<VideoChoice> {
    let mut taken = Vec::new();
    let mut out: Vec<VideoChoice> = adaptive
        .iter()
        .map(|f| f.video_choice(&mut taken, false))
        .collect();
    // A muxed entry already carries its sound, so it belongs here with `has_audio` and
    // never in the audio list — pairing it with a second audio track would double it.
    out.extend(
        muxed
            .iter()
            .filter(|f| f.is_video())
            .map(|f| f.video_choice(&mut taken, true)),
    );
    out
}

/// Every fetchable audio rendition, minus the DRC duplicates.
///
/// YouTube ships a loudness-compressed copy of each track under the same itag. Listing both
/// gives a dropdown with every bitrate in it twice, one of which quietly flattens the mix.
fn audio_choices(audios: &[&Format]) -> Vec<AudioChoice> {
    let mut taken = Vec::new();
    audios
        .iter()
        .filter(|f| !f.drc)
        .map(|f| f.audio_choice(&mut taken))
        .collect()
}

/// InnerTube writes byte counts as strings and pixel counts as numbers, in the same
/// object. Accept both rather than guessing per field.
fn number(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_player_response(root: &Value, id: Option<&str>) -> Result<Extraction, SiteError> {
    check_playability(root)?;

    let details = root.get("videoDetails").ok_or_else(shape)?;
    // `isLive` means *now*; `isLiveContent` merely means it once was, and stays true
    // forever on the finished recording. Keying on the latter would refuse every
    // streamed lecture and conference talk on the site — which is a large part of what
    // people actually want to keep — so only a currently-live stream is refused. A live
    // one has no final byte to download to; a finished one is an ordinary video.
    if details
        .get("isLive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(SiteError::Unavailable(
            "this stream is live right now, so it has no end to download to. \
             It can be saved once it has finished."
                .into(),
        ));
    }

    let title = details
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| id.map(str::to_string))
        .unwrap_or_else(|| "video".to_string());
    let duration_ms = number(details.get("lengthSeconds")).map(|s| s * 1000);

    let streaming = root.get("streamingData").ok_or_else(shape)?;
    let adaptive = formats_at(streaming, "adaptiveFormats");
    let muxed = formats_at(streaming, "formats");

    let videos: Vec<&Format> = adaptive.iter().filter(|f| f.is_video()).collect();
    let audios: Vec<&Format> = adaptive.iter().filter(|f| f.is_audio()).collect();

    let mut options = Vec::new();
    let best_mp4_audio = best_audio(&audios, "audio/mp4");

    // Merged options exist only when there is an MP4 audio track to pair with: the
    // merger writes an MP4, and an Opus track cannot go into one. Offering a WebM audio
    // beside an MP4 video would produce a merge that fails at the last step, after the
    // whole download.
    if let Some(audio) = best_mp4_audio {
        let audio_stream = audio.stream(StreamKind::AudioOnly);
        for height in distinct_heights(&videos) {
            let Some(video) = best_video_at(&videos, height) else {
                // WebM-only height — see above, nothing here can be merged into an MP4.
                continue;
            };
            options.push(MediaOption {
                label: format!("{} · MP4", video.quality()),
                rank: u64::from(height),
                streams: vec![video.stream(StreamKind::VideoOnly), audio_stream.clone()],
                filename: safe_filename(&title, "mp4"),
                width: video.width,
                height: video.height,
                duration_ms,
            });
        }
    }

    // `formats` is the old muxed list. Modern uploads no longer have it — the captured
    // 2026 response carries none — but short and old videos still ship itag 18, and one
    // file that needs no merging is the friendlier download when it exists.
    for f in muxed.iter().filter(|f| f.is_mp4()) {
        let Some(height) = f.height else { continue };
        options.push(MediaOption {
            label: format!("{} · MP4 (single file)", f.quality()),
            rank: u64::from(height),
            streams: vec![f.stream(StreamKind::Muxed)],
            filename: safe_filename(&title, "mp4"),
            width: f.width,
            height: f.height,
            duration_ms,
        });
    }

    // Audio on its own: MP4/AAC when there is any, else the Opus track — still worth
    // offering, just under a label that admits what it is.
    if let Some(audio) = best_mp4_audio {
        options.push(audio_only_option(
            audio,
            &title,
            duration_ms,
            "MP4/AAC",
            "m4a",
        ));
    } else if let Some(audio) = best_audio(&audios, "audio/webm") {
        options.push(audio_only_option(
            audio,
            &title,
            duration_ms,
            "WebM/Opus",
            "webm",
        ));
    }

    if options.is_empty() {
        return Err(SiteError::Unavailable(
            "YouTube offered no downloadable formats for this video — it is served only by \
             its server-side player"
                .into(),
        ));
    }

    // Descending, and stable, so a merged option keeps its place ahead of the muxed
    // option of the same height.
    options.sort_by_key(|o| core::cmp::Reverse(o.rank));

    let mut extraction = Extraction {
        note: None,
        site: "YouTube".into(),
        title,
        options,
        videos: video_choices(&videos, &muxed),
        audios: audio_choices(&audios),
        subtitles: subtitles(root),
    };
    // Ordering and the single `best` flag are decided in one place for every site, so
    // "best" cannot come to mean two things — see [`Extraction::rank_choices`].
    extraction.rank_choices();
    Ok(extraction)
}

fn audio_only_option(
    audio: &Format,
    title: &str,
    duration_ms: Option<u64>,
    codec_label: &str,
    extension: &str,
) -> MediaOption {
    MediaOption {
        label: format!("Audio only · {codec_label} · {} kbps", audio.kbps()),
        rank: audio.kbps(),
        streams: vec![audio.stream(StreamKind::AudioOnly)],
        filename: safe_filename(title, extension),
        width: None,
        height: None,
        duration_ms,
    }
}

fn formats_at(streaming: &Value, key: &str) -> Vec<Format> {
    streaming
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Format::parse).collect())
        .unwrap_or_default()
}

fn distinct_heights(videos: &[&Format]) -> Vec<u32> {
    let mut heights: Vec<u32> = videos.iter().filter_map(|f| f.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    heights
}

fn best_video_at<'a>(videos: &[&'a Format], height: u32) -> Option<&'a Format> {
    videos
        .iter()
        .filter(|f| f.height == Some(height) && f.mime.starts_with("video/mp4"))
        .max_by_key(|f| (f.codec_rank(), f.bitrate))
        .copied()
}

/// Highest bitrate wins, but a non-DRC track always beats a DRC one: the compressed
/// duplicate is the same recording with its dynamics flattened, which nobody asked for.
fn best_audio<'a>(audios: &[&'a Format], prefix: &str) -> Option<&'a Format> {
    audios
        .iter()
        .filter(|f| f.mime.starts_with(prefix))
        .max_by_key(|f| (u8::from(!f.drc), f.bitrate))
        .copied()
}

/// Refuse anything that is not `OK`, in the site's own words where it has any.
///
/// This is the common real-world path — age gates, members-only videos, region locks and
/// removed uploads all land here — so the message matters more than the code around it.
/// It goes straight to the user.
fn check_playability(root: &Value) -> Result<(), SiteError> {
    let status_obj = root.get("playabilityStatus").ok_or_else(shape)?;
    let status = status_obj
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(shape)?;
    if status.eq_ignore_ascii_case("OK") {
        return Ok(());
    }
    let reason = status_obj
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| explain_status(status));
    Err(SiteError::Unavailable(reason))
}

fn explain_status(status: &str) -> String {
    match status {
        "LOGIN_REQUIRED" => "YouTube will not serve this video without a signed-in session".into(),
        "AGE_VERIFICATION_REQUIRED" => {
            "this video is age-restricted and needs a verified account".into()
        }
        "UNPLAYABLE" => "YouTube says this video cannot be played".into(),
        "ERROR" => "YouTube says this video is unavailable — it may have been removed".into(),
        "LIVE_STREAM_OFFLINE" => "this live stream is not on the air".into(),
        other => format!("YouTube refused to play this video ({other})"),
    }
}

/// Caption tracks, asked for as WebVTT.
///
/// The `baseUrl` returns YouTube's own `json3` timedtext format by default; `fmt=vtt`
/// makes it hand back something [`crate::subs`] can already read.
fn subtitles(root: &Value) -> Vec<SubtitleTrack> {
    let tracks = root
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(Value::as_array);
    let Some(tracks) = tracks else {
        return Vec::new();
    };
    tracks
        .iter()
        .filter_map(|t| {
            let base = t.get("baseUrl").and_then(Value::as_str)?.trim();
            if base.is_empty() {
                return None;
            }
            let language = t
                .get("languageCode")
                .and_then(Value::as_str)
                .map(str::to_string);
            let label = t
                .get("name")
                .and_then(track_name)
                .or_else(|| language.clone())
                .unwrap_or_else(|| "Subtitles".to_string());
            // The base URL always carries a query already, but a `?` costs nothing to
            // check and a `&`-prefixed first parameter would silently 404.
            let separator = if base.contains('?') { '&' } else { '?' };
            Some(SubtitleTrack {
                label,
                language,
                url: format!("{base}{separator}fmt=vtt"),
                format: "vtt".into(),
            })
        })
        .collect()
}

/// A caption track's name is `simpleText` on some responses and a `runs` array on others.
fn track_name(name: &Value) -> Option<String> {
    name.get("simpleText")
        .and_then(Value::as_str)
        .or_else(|| name.pointer("/runs/0/text").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of a real iOS player response captured on 2026-09-05: four video
    /// formats (AVC, VP9 and AV1 at 1080p60, AVC at 360p), the AAC track and its DRC
    /// twin, and the Opus track. URLs shortened, structure untouched.
    const IOS_FIXTURE: &str = include_str!("fixtures/youtube_ios_player.json");

    fn extract(body: &str) -> Result<Extraction, SiteError> {
        let mut yt = YouTube::new();
        yt.start("https://www.youtube.com/watch?v=aqz-KE-bpKQ")
            .unwrap();
        match yt.feed(&[body])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("the extractor asked for a second fetch"),
        }
    }

    #[test]
    fn every_shape_of_youtube_link_yields_the_same_video_id() {
        let expected = Some("aqz-KE-bpKQ".to_string());
        assert_eq!(
            video_id("https://www.youtube.com/watch?v=aqz-KE-bpKQ"),
            expected
        );
        assert_eq!(video_id("https://youtu.be/aqz-KE-bpKQ?t=42"), expected);
        assert_eq!(
            video_id("https://www.youtube.com/shorts/aqz-KE-bpKQ"),
            expected
        );
        assert_eq!(
            video_id("https://www.youtube-nocookie.com/embed/aqz-KE-bpKQ?rel=0"),
            expected
        );
        assert_eq!(
            video_id("https://www.youtube.com/live/aqz-KE-bpKQ"),
            expected
        );
        assert_eq!(
            video_id("https://m.youtube.com/watch?list=PL9&v=aqz-KE-bpKQ&index=2"),
            expected
        );
    }

    #[test]
    fn a_link_that_names_no_video_is_refused_with_a_sentence() {
        assert_eq!(video_id("https://www.youtube.com/feed/subscriptions"), None);
        assert_eq!(video_id("https://www.youtube.com/watch?v=PLtooshort"), None);
        let err = YouTube::new()
            .start("https://www.youtube.com/@blender")
            .unwrap_err();
        match err {
            SiteError::Unavailable(msg) => assert!(msg.contains("does not name a video"), "{msg}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_only_request_is_the_ios_player_post() {
        let mut yt = YouTube::new();
        let step = yt.start("https://youtu.be/aqz-KE-bpKQ").unwrap();
        let Step::Need(Need::Fetch(reqs)) = step else {
            panic!("expected a fetch");
        };
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert!(r
            .url
            .starts_with("https://www.youtube.com/youtubei/v1/player?key="));
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "User-Agent" && v == IOS_USER_AGENT));
        let body = r.body.as_deref().unwrap();
        assert!(body.contains(r#""videoId":"aqz-KE-bpKQ""#), "{body}");
        assert!(body.contains(r#""clientName":"IOS""#), "{body}");
        // It must be valid JSON, not merely a plausible-looking string.
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed["context"]["client"]["clientVersion"],
            IOS_CLIENT_VERSION
        );
    }

    #[test]
    fn a_real_response_yields_one_merged_option_per_height_plus_audio() {
        let e = extract(IOS_FIXTURE).unwrap();
        assert_eq!(e.site, "YouTube");
        assert!(e.title.starts_with("Big Buck Bunny"));
        // 1080 and 360 are the heights in the fixture, then the audio-only option.
        assert_eq!(e.options.len(), 3, "{:?}", labels(&e));
        assert_eq!(e.options[0].label, "1080p60 · MP4");
        assert_eq!(e.options[0].height, Some(1080));
        assert_eq!(e.options[0].duration_ms, Some(635_000));
    }

    #[test]
    fn the_best_option_pairs_avc_video_with_aac_audio() {
        let e = extract(IOS_FIXTURE).unwrap();
        let best = &e.options[0];
        assert_eq!(best.streams.len(), 2);
        assert_eq!(best.streams[0].kind, StreamKind::VideoOnly);
        assert!(best.streams[0].mime.as_deref().unwrap().contains("avc1"));
        assert_eq!(best.streams[1].kind, StreamKind::AudioOnly);
        assert!(best.streams[1].mime.as_deref().unwrap().contains("mp4a"));
        // The DRC duplicate of itag 140 must not be what got picked.
        assert!(
            best.streams[1].url.contains("v=std"),
            "{}",
            best.streams[1].url
        );
        assert_eq!(best.streams[0].size, Some(257_619_653));
    }

    #[test]
    fn every_stream_carries_the_ios_user_agent() {
        let e = extract(IOS_FIXTURE).unwrap();
        let mut streams: Vec<(&str, &Stream)> = Vec::new();
        for option in &e.options {
            streams.extend(option.streams.iter().map(|s| (option.label.as_str(), s)));
        }
        // The choice lists are fetched the same way the options are, so the header they
        // cannot do without is asserted over both.
        streams.extend(e.videos.iter().map(|v| (v.label.as_str(), &v.stream)));
        streams.extend(e.audios.iter().map(|a| (a.label.as_str(), &a.stream)));
        for (label, stream) in streams {
            assert!(
                stream
                    .headers
                    .iter()
                    .any(|(k, v)| k == "User-Agent" && v == IOS_USER_AGENT),
                "{label} is missing the iOS User-Agent"
            );
        }
    }

    #[test]
    fn the_choice_lists_hold_every_rendition_ranked_with_exactly_one_best() {
        let e = extract(IOS_FIXTURE).unwrap();
        // Four video formats in the fixture, and three audio ones of which the DRC
        // duplicate is not a choice.
        assert_eq!(video_ids(&e), vec!["v299", "v303", "v399", "v134"]);
        assert_eq!(audio_ids(&e), vec!["a251", "a140"]);

        let heights: Vec<Option<u32>> = e.videos.iter().map(|v| v.height).collect();
        assert_eq!(
            heights,
            vec![Some(1080), Some(1080), Some(1080), Some(360)],
            "videos are not ranked best-first"
        );
        let rates: Vec<Option<u64>> = e.audios.iter().map(|a| a.bitrate).collect();
        assert_eq!(rates, vec![Some(143_452), Some(130_992)]);

        assert_eq!(e.videos.iter().filter(|v| v.best).count(), 1);
        assert_eq!(e.audios.iter().filter(|a| a.best).count(), 1);

        // "Best" means best *deliverable*, not merely largest. The highest-bitrate audio
        // here is Opus in WebM, and this build joins video to audio only inside MP4 — so
        // recommending it would recommend a file that cannot be given a picture. The
        // recommendation is the best MP4 pair; the Opus track stays in the list.
        assert_eq!(e.best_video().map(|v| v.id.as_str()), Some("v299"));
        assert_eq!(
            e.best_audio().map(|a| a.id.as_str()),
            Some("a140"),
            "the recommended audio must be one that can actually be joined"
        );
        assert_eq!(
            e.audios[0].id, "a251",
            "the Opus track is still offered, and still first"
        );
        assert!(!e.audios[0].best);
    }

    #[test]
    fn a_rendition_that_cannot_be_joined_is_offered_but_never_recommended() {
        let e = extract(IOS_FIXTURE).unwrap();
        for choice in &e.videos {
            assert_eq!(
                choice.mergeable,
                choice.has_audio || choice.container.as_deref() == Some("mp4"),
                "{} was misjudged",
                choice.label
            );
        }
        let opus = e
            .audios
            .iter()
            .find(|a| a.id == "a251")
            .expect("no Opus track");
        assert_eq!(opus.container.as_deref(), Some("webm"));
        assert!(!opus.mergeable, "WebM audio cannot be joined by this build");
        let recommended = e.best_audio().unwrap();
        assert!(recommended.mergeable);
        assert_eq!(recommended.container.as_deref(), Some("mp4"));
    }

    #[test]
    fn the_webm_and_av1_renditions_are_offered_even_though_no_merged_option_uses_them() {
        // This is the whole point of the second list. The merger writes MP4/AVC, so those
        // two renditions can never appear in `options` — but a user who wants the AV1 file
        // at a third of the bytes is asking for something real.
        let e = extract(IOS_FIXTURE).unwrap();
        let vp9 = e.videos.iter().find(|v| v.id == "v303").expect("no VP9");
        let av1 = e.videos.iter().find(|v| v.id == "v399").expect("no AV1");
        assert_eq!(vp9.label, "1080p60 · VP9 · 4.7 Mbps");
        assert_eq!(av1.label, "1080p60 · AV1 · 4.5 Mbps");
        for url in [&vp9.stream.url, &av1.stream.url] {
            assert!(
                !e.options
                    .iter()
                    .any(|o| o.streams.iter().any(|s| &s.url == url)),
                "{url} should not be reachable through the merged options"
            );
        }
    }

    #[test]
    fn every_video_choice_names_its_codec_because_that_is_the_choice_being_made() {
        // Three of the four renditions are 1080p60; the codec is the only thing that tells
        // them apart, so a label without it offers the user the same thing three times.
        let e = extract(IOS_FIXTURE).unwrap();
        for v in &e.videos {
            let codec = v.codec.as_deref().expect("no codec");
            assert!(v.label.contains(codec), "{} omits {codec}", v.label);
        }
        assert_eq!(e.videos[0].label, "1080p60 · AVC · 6.9 Mbps");
        assert_eq!(e.videos[3].label, "360p · AVC · 643 kbps");
        assert_eq!(e.audios[1].label, "130 kbps · AAC");
    }

    #[test]
    fn the_drc_duplicate_is_not_offered_as_a_second_audio_choice() {
        // Itag 140 arrives twice: the track, and the same track with its dynamics
        // flattened. Listing both gives a dropdown with 130 kbps in it twice, one of which
        // is quietly the wrong file.
        let e = extract(IOS_FIXTURE).unwrap();
        assert_eq!(e.audios.len(), 2, "{:?}", audio_ids(&e));
        let aac = e.audios.iter().find(|a| a.id == "a140").unwrap();
        assert!(aac.stream.url.contains("v=std"), "{}", aac.stream.url);
        assert!(!e.audios.iter().any(|a| a.stream.url.contains("v=drc")));
    }

    #[test]
    fn an_alternate_soundtrack_is_labelled_with_its_language_and_keeps_its_own_id() {
        // A dubbed upload repeats one itag per language, so the language has to reach both
        // the label — otherwise the dropdown reads "129 kbps · AAC" twice — and the id,
        // which is what a UI hands back to say which one the user picked.
        let e = extract(MULTI_LANGUAGE).unwrap();
        assert_eq!(
            e.audios
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Spanish (Latin America) · 129 kbps · AAC",
                "English original · 129 kbps · AAC"
            ]
        );
        assert_eq!(audio_ids(&e), vec!["a140-es-419.3", "a140-en-US.4"]);
        assert_eq!(
            e.audios[1].language.as_deref(),
            Some("English original"),
            "the language must survive as data, not only inside the label"
        );
    }

    #[test]
    fn a_muxed_format_is_a_video_choice_that_needs_no_audio_picked_for_it() {
        let e = extract(MUXED).unwrap();
        let single = e.videos.iter().find(|v| v.id == "v18").expect("no itag 18");
        assert!(single.has_audio);
        assert_eq!(single.stream.kind, StreamKind::Muxed);
        assert_eq!(single.label, "360p · AVC · 500 kbps");
        // The adaptive rendition beside it is picture only, and the muxed one is not in
        // the audio list — pairing it with a second track would double its sound.
        assert!(!e.videos.iter().find(|v| v.id == "v137").unwrap().has_audio);
        assert_eq!(audio_ids(&e), vec!["a140"]);
    }

    #[test]
    fn a_choice_id_is_unique_within_its_own_list() {
        // A UI pairs a video id with an audio id. Two renditions answering to one name is
        // the user getting a different file from the one they chose.
        for body in [IOS_FIXTURE, MULTI_LANGUAGE, MUXED] {
            let e = extract(body).unwrap();
            for ids in [video_ids(&e), audio_ids(&e)] {
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), ids.len(), "duplicate id in {ids:?}");
            }
        }
    }

    #[test]
    fn an_audio_only_option_is_offered_as_m4a() {
        let e = extract(IOS_FIXTURE).unwrap();
        let audio = e
            .options
            .iter()
            .find(|o| o.label.starts_with("Audio only"))
            .expect("no audio-only option");
        assert!(audio.filename.ends_with(".m4a"), "{}", audio.filename);
        assert_eq!(audio.streams.len(), 1);
        assert_eq!(audio.streams[0].kind, StreamKind::AudioOnly);
        assert_eq!(audio.label, "Audio only · MP4/AAC · 130 kbps");
        assert_eq!(audio.rank, 130);
    }

    #[test]
    fn options_are_sorted_by_rank_descending() {
        let e = extract(IOS_FIXTURE).unwrap();
        let ranks: Vec<u64> = e.options.iter().map(|o| o.rank).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(ranks, sorted, "{:?}", labels(&e));
    }

    #[test]
    fn a_title_with_a_slash_cannot_escape_the_download_directory() {
        let body = fixture_with_title("AC/DC — Back in Black");
        let e = extract(&body).unwrap();
        for option in &e.options {
            assert!(!option.filename.contains('/'), "{}", option.filename);
        }
        assert!(e.options[0].filename.starts_with("AC_DC"));
    }

    #[test]
    fn login_required_is_reported_in_youtubes_own_words() {
        let body = r#"{
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "Sign in to confirm your age",
                "messages": ["This video may be inappropriate for some users."]
            },
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "x", "lengthSeconds": "10"}
        }"#;
        assert_eq!(
            extract(body).unwrap_err(),
            SiteError::Unavailable("Sign in to confirm your age".into())
        );
    }

    #[test]
    fn a_status_with_no_reason_still_reads_as_a_sentence() {
        let body = r#"{"playabilityStatus": {"status": "UNPLAYABLE"}}"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert_eq!(msg, "YouTube says this video cannot be played");
        assert!(!msg.contains("UNPLAYABLE"), "the raw status leaked: {msg}");
    }

    #[test]
    fn every_stream_states_the_request_size_google_tolerates() {
        // Without this the engine plans 8 MiB chunks, which are refused with a 403 that
        // reads like an authorisation failure rather than a throttle.
        let extraction = extract(IOS_FIXTURE).expect("the fixture parses");
        for option in &extraction.options {
            for stream in &option.streams {
                assert_eq!(
                    stream.max_chunk,
                    Some(MAX_RANGE_BYTES),
                    "{} did not state the range limit",
                    option.label
                );
            }
        }
    }

    #[test]
    fn a_stream_that_is_live_right_now_is_refused_because_it_has_no_last_byte() {
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Live now", "isLive": true},
            "streamingData": {"adaptiveFormats": []}
        }"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("live right now"), "{msg}");
    }

    #[test]
    fn a_finished_recording_of_a_live_stream_is_an_ordinary_video() {
        // `isLiveContent` stays true forever on a stream that has ended, so keying on it
        // would refuse every streamed lecture, conference talk and premiere on the site
        // — a large share of exactly what people want to keep. Only `isLive` refuses.
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {
                "videoId": "aqz-KE-bpKQ",
                "title": "The talk, recorded",
                "lengthSeconds": "3600",
                "isLiveContent": true
            },
            "streamingData": {
                "adaptiveFormats": [
                    {"itag": 137, "mimeType": "video/mp4; codecs=\"avc1.640028\"", "bitrate": 4000000,
                     "width": 1920, "height": 1080, "qualityLabel": "1080p", "contentLength": "100",
                     "url": "https://v/1080"},
                    {"itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000,
                     "contentLength": "10", "url": "https://a/128"}
                ]
            }
        }"#;
        let extraction = extract(body).expect("a finished stream downloads like any video");
        assert_eq!(extraction.title, "The talk, recorded");
        assert!(
            extraction.options.iter().any(|o| o.height == Some(1080)),
            "the 1080p option should be offered"
        );
    }

    #[test]
    fn a_format_with_no_url_is_skipped_rather_than_offered_broken() {
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Sabr", "lengthSeconds": "60"},
            "streamingData": {
                "serverAbrStreamingUrl": "https://x/videoplayback?sabr=1",
                "adaptiveFormats": [
                    {"itag": 299, "mimeType": "video/mp4; codecs=\"avc1.64002A\"",
                     "height": 1080, "width": 1920, "bitrate": 100},
                    {"itag": 137, "url": "https://x/720", "mimeType": "video/mp4; codecs=\"avc1.4d401f\"",
                     "height": 720, "width": 1280, "bitrate": 50},
                    {"itag": 140, "url": "https://x/aac", "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                     "bitrate": 128000}
                ]
            }
        }"#;
        let e = extract(body).unwrap();
        assert_eq!(
            labels(&e),
            vec!["720p · MP4", "Audio only · MP4/AAC · 128 kbps"]
        );
    }

    #[test]
    fn a_response_with_only_sabr_formats_says_so_instead_of_offering_nothing() {
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Sabr", "lengthSeconds": "60"},
            "streamingData": {"adaptiveFormats": [
                {"itag": 299, "mimeType": "video/mp4", "height": 1080, "bitrate": 1}
            ]}
        }"#;
        let SiteError::Unavailable(msg) = extract(body).unwrap_err() else {
            panic!("expected Unavailable");
        };
        assert!(msg.contains("server-side player"), "{msg}");
    }

    #[test]
    fn a_muxed_format_becomes_a_single_file_option() {
        let e = extract(MUXED).unwrap();
        assert_eq!(
            labels(&e),
            vec![
                "1080p · MP4",
                "360p · MP4 (single file)",
                "Audio only · MP4/AAC · 128 kbps"
            ]
        );
        let muxed = &e.options[1];
        assert_eq!(muxed.streams.len(), 1);
        assert_eq!(muxed.streams[0].kind, StreamKind::Muxed);
        assert_eq!(muxed.streams[0].size, Some(1_234_567));
    }

    #[test]
    fn webm_audio_is_offered_alone_but_never_merged_into_an_mp4() {
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Opus only", "lengthSeconds": "30"},
            "streamingData": {"adaptiveFormats": [
                {"itag": 137, "url": "https://x/1080", "mimeType": "video/mp4; codecs=\"avc1.640028\"",
                 "width": 1920, "height": 1080, "bitrate": 4000000, "qualityLabel": "1080p"},
                {"itag": 251, "url": "https://x/opus", "mimeType": "audio/webm; codecs=\"opus\"",
                 "bitrate": 143000}
            ]}
        }"#;
        let e = extract(body).unwrap();
        assert_eq!(labels(&e), vec!["Audio only · WebM/Opus · 143 kbps"]);
        assert!(
            e.options[0].filename.ends_with(".webm"),
            "{}",
            e.options[0].filename
        );
    }

    #[test]
    fn caption_tracks_are_asked_for_as_vtt() {
        let body = r#"{
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Subs", "lengthSeconds": "30"},
            "captions": {"playerCaptionsTracklistRenderer": {"captionTracks": [
                {"baseUrl": "https://www.youtube.com/api/timedtext?v=x&lang=en",
                 "languageCode": "en", "name": {"simpleText": "English"}},
                {"baseUrl": "https://www.youtube.com/api/timedtext?v=x&lang=de",
                 "languageCode": "de", "name": {"runs": [{"text": "Deutsch"}]}},
                {"baseUrl": "", "languageCode": "fr"}
            ]}},
            "streamingData": {"adaptiveFormats": [
                {"itag": 137, "url": "https://x/1080", "mimeType": "video/mp4; codecs=\"avc1.640028\"",
                 "height": 1080, "bitrate": 1, "qualityLabel": "1080p"},
                {"itag": 140, "url": "https://x/aac", "mimeType": "audio/mp4", "bitrate": 128000}
            ]}
        }"#;
        let e = extract(body).unwrap();
        assert_eq!(e.subtitles.len(), 2, "the empty baseUrl should be dropped");
        assert_eq!(e.subtitles[0].label, "English");
        assert_eq!(e.subtitles[0].language.as_deref(), Some("en"));
        assert!(
            e.subtitles[0].url.ends_with("&fmt=vtt"),
            "{}",
            e.subtitles[0].url
        );
        assert_eq!(e.subtitles[0].format, "vtt");
        assert_eq!(e.subtitles[1].label, "Deutsch");
    }

    #[test]
    fn a_response_that_is_not_a_player_response_at_all_is_a_shape_error() {
        assert_eq!(extract("not json").unwrap_err(), shape());
        assert_eq!(extract("{}").unwrap_err(), shape());
        assert_eq!(
            YouTube::new().feed(&[]).unwrap_err(),
            SiteError::Shape("YouTube".into())
        );
    }

    #[test]
    fn only_youtubes_own_hosts_match() {
        assert!(matches("www.youtube.com"));
        assert!(matches("m.youtube.com"));
        assert!(matches("music.youtube.com"));
        assert!(matches("youtu.be"));
        assert!(matches("www.youtube-nocookie.com"));
        assert!(!matches("notyoutube.com"));
        assert!(!matches("youtube.com.evil.test"));
    }

    /// A dubbed upload: one itag, one entry per soundtrack.
    const MULTI_LANGUAGE: &str = r#"{
        "playabilityStatus": {"status": "OK"},
        "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Dubbed", "lengthSeconds": "60"},
        "streamingData": {"adaptiveFormats": [
            {"itag": 137, "url": "https://x/1080", "mimeType": "video/mp4; codecs=\"avc1.640028\"",
             "width": 1920, "height": 1080, "bitrate": 4000000, "qualityLabel": "1080p"},
            {"itag": 140, "url": "https://x/en", "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
             "bitrate": 129000,
             "audioTrack": {"displayName": "English original", "id": "en-US.4", "audioIsDefault": true}},
            {"itag": 140, "url": "https://x/es", "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
             "bitrate": 129001,
             "audioTrack": {"displayName": "Spanish (Latin America)", "id": "es-419.3"}}
        ]}
    }"#;

    /// An old upload, which still ships the one-file itag 18 beside the adaptive list.
    const MUXED: &str = r#"{
        "playabilityStatus": {"status": "OK"},
        "videoDetails": {"videoId": "aqz-KE-bpKQ", "title": "Old upload", "lengthSeconds": "30"},
        "streamingData": {
            "formats": [
                {"itag": 18, "url": "https://x/18", "mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"",
                 "width": 640, "height": 360, "bitrate": 500000, "qualityLabel": "360p",
                 "contentLength": "1234567"}
            ],
            "adaptiveFormats": [
                {"itag": 137, "url": "https://x/1080", "mimeType": "video/mp4; codecs=\"avc1.640028\"",
                 "width": 1920, "height": 1080, "bitrate": 4000000, "qualityLabel": "1080p"},
                {"itag": 140, "url": "https://x/aac", "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                 "bitrate": 128000}
            ]
        }
    }"#;

    fn labels(e: &Extraction) -> Vec<String> {
        e.options.iter().map(|o| o.label.clone()).collect()
    }

    fn video_ids(e: &Extraction) -> Vec<String> {
        e.videos.iter().map(|v| v.id.clone()).collect()
    }

    fn audio_ids(e: &Extraction) -> Vec<String> {
        e.audios.iter().map(|a| a.id.clone()).collect()
    }

    /// Rewrite the fixture's title, to test filename handling without a second fixture.
    fn fixture_with_title(title: &str) -> String {
        let mut v: Value = serde_json::from_str(IOS_FIXTURE).unwrap();
        v["videoDetails"]["title"] = Value::String(title.into());
        v.to_string()
    }
}
