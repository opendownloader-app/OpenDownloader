//! TikTok — `tiktok.com` and the `vt.`/`vm.` short-link hosts.
//!
//! # The shape this was built against
//!
//! A video page carries its whole rehydration payload in
//! `<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">`, and the
//! item lives at `__DEFAULT_SCOPE__["webapp.video-detail"].itemInfo.itemStruct`. The
//! previous generation of the page used `<script id="SIGI_STATE">` with the same item
//! under `ItemModule[<video id>]`; both are parsed, because TikTok has served the older
//! one to older user agents and to some regions long after replacing it.
//!
//! **Honesty about provenance:** written on 2026-09-05 against those two documented
//! shapes and *unverified against a live page* — TikTok answers a bot wall to any fetch
//! that does not come from a logged-in browser, so no real fixture could be captured. The
//! tests assert against hand-written samples. A change on TikTok's side surfaces as
//! [`SiteError::Shape`] naming the site.
//!
//! # Why the page and not the API
//!
//! Every TikTok media URL is signed for the session that requested it, so a URL fetched
//! outside the user's tab is not one the CDN will serve. Reading the page the user is
//! already on is not a convenience here; it is the only thing that works.

use super::bilibili::{decode_json_escapes, script_tag_body};
use super::{
    host_is, safe_filename, AudioChoice, Extraction, Extractor, MediaOption, Need, SiteError, Step,
    Stream, StreamKind, VideoChoice,
};
use serde_json::Value;

pub fn matches(host: &str) -> bool {
    // `vt.` and `vm.` are subdomains of tiktok.com, so the one suffix rule covers them;
    // they are named in the doc comment rather than duplicated here.
    host_is(host, "tiktok.com")
}

const SITE: &str = "TikTok";
const REFERER: &str = "https://www.tiktok.com/";

fn shape() -> SiteError {
    SiteError::Shape(SITE.to_string())
}

/// The one sentence a user sees when the post is gone.
///
/// It names the two things they can do about it, because "unavailable" on its own reads
/// as a bug in the downloader when it is usually a private account.
fn unavailable(detail: Option<&str>) -> SiteError {
    let base = "TikTok will not serve this video — it is private, deleted, or restricted. \
                Opening it in a tab you are signed into is what usually fixes it.";
    SiteError::Unavailable(match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{base} TikTok said: {d}"),
        None => base.to_string(),
    })
}

fn headers() -> Vec<(String, String)> {
    // TikTok's CDN checks `Referer` on the media host the same way bilibili's does.
    vec![("Referer".to_string(), REFERER.to_string())]
}

#[derive(Debug, Default)]
pub struct TikTok {
    page_url: String,
}

impl TikTok {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for TikTok {
    fn site(&self) -> &'static str {
        SITE
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        // Kept because the `SIGI_STATE` shape keys its items by video id, and the id is
        // only ever stated in the URL.
        self.page_url = url.to_string();
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let html = bodies.first().copied().ok_or_else(shape)?;
        Ok(Step::Done(parse_video_page(html, &self.page_url)?))
    }
}

/// Turn a video page's HTML into an [`Extraction`]. Pure; the tests call it directly.
pub fn parse_video_page(html: &str, page_url: &str) -> Result<Extraction, SiteError> {
    let item = item_struct(html, page_url)?;
    let title = item
        .get("desc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("TikTok video")
        .to_string();
    let author = item
        .pointer("/author/uniqueId")
        .or_else(|| item.pointer("/author/unique_id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let media = media_for(&item, &title, author);
    if media.options.is_empty() {
        // The item parsed but stated no playable URL, which is what a takedown looks like
        // from here — the metadata survives the media.
        return Err(unavailable(None));
    }
    let mut extraction = Extraction {
        note: None,
        site: SITE.to_string(),
        title,
        options: media.options,
        videos: media.videos,
        audios: media.audios,
        subtitles: Vec::new(),
    };
    // The one place "best" is decided, for every site alike.
    extraction.rank_choices();
    Ok(extraction)
}

/// Find the item, in whichever of the two shapes this page uses.
fn item_struct(html: &str, page_url: &str) -> Result<Value, SiteError> {
    if let Some(body) = script_tag_body(html, "__UNIVERSAL_DATA_FOR_REHYDRATION__") {
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        let detail = root
            .pointer("/__DEFAULT_SCOPE__/webapp.video-detail")
            .ok_or_else(shape)?;
        // TikTok reports a removed or private post *inside* a perfectly well-formed
        // payload, so the status has to be read before the item is looked for; otherwise
        // a takedown is misreported as the site having changed.
        let status_code = detail
            .get("statusCode")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let status_msg = detail.get("statusMsg").and_then(Value::as_str);
        if status_code != 0 || status_msg.is_some_and(|m| !m.trim().is_empty()) {
            return Err(unavailable(status_msg));
        }
        return detail
            .pointer("/itemInfo/itemStruct")
            .cloned()
            .ok_or_else(|| unavailable(None));
    }

    if let Some(body) = script_tag_body(html, "SIGI_STATE") {
        let root: Value = serde_json::from_str(body).map_err(|_| shape())?;
        let modules = root
            .get("ItemModule")
            .and_then(Value::as_object)
            .ok_or_else(shape)?;
        // Keyed by video id. A page can hold several items (the recommendation rail), so
        // the id from the URL picks the one the user is actually looking at; falling back
        // to the first entry only matters for short links that carry no id.
        let by_id = video_id(page_url).and_then(|id| modules.get(&id));
        return by_id
            .or_else(|| modules.values().next())
            .cloned()
            .ok_or_else(|| unavailable(None));
    }

    Err(shape())
}

/// The numeric id out of `…/@user/video/1234567890`.
fn video_id(page_url: &str) -> Option<String> {
    let after = page_url.split("/video/").nth(1)?;
    let id = after.split(['/', '?', '#']).next()?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| id.to_string())
}

/// The ready-made options and the per-track choices, built from one walk of the item.
///
/// The two describe the same renditions from different angles — an option is "give me
/// this file", a [`VideoChoice`] is "give me this picture" — so they are built together
/// rather than one being re-derived from the other, which would mean parsing TikTok's
/// shape twice and getting it wrong in two places instead of one.
struct Media {
    options: Vec<MediaOption>,
    videos: Vec<VideoChoice>,
    audios: Vec<AudioChoice>,
}

fn media_for(item: &Value, title: &str, author: &str) -> Media {
    let video = item.get("video").unwrap_or(&Value::Null).clone();
    let width = dimension(&video, "width");
    let height = dimension(&video, "height");
    let duration_ms = duration_ms(&video);
    let extension = video
        .get("format")
        .and_then(Value::as_str)
        .filter(|f| f.chars().all(|c| c.is_ascii_alphanumeric()) && !f.is_empty())
        .unwrap_or("mp4")
        .to_string();
    let filename = filename_for(title, author, &extension);

    let play_addr = url_of(&video, "playAddr").or_else(|| url_of(&video, "play_addr"));
    let download_addr = url_of(&video, "downloadAddr").or_else(|| url_of(&video, "download_addr"));

    let mut options: Vec<MediaOption> = Vec::new();
    // Every TikTok rendition is muxed, so each one is a whole answer on its own and
    // carries `has_audio: true`; nothing here needs an audio partner picked for it.
    let mut videos: Vec<VideoChoice> = Vec::new();
    let mut ids: Vec<String> = Vec::new();

    // `bitrateInfo` is the real menu: the same clip at several encodes, every one of them
    // muxed, so there is nothing to merge and the bitrate is the only thing separating
    // them.
    let mut rendition_ranks: Vec<u64> = Vec::new();
    for entry in video
        .get("bitrateInfo")
        .or_else(|| video.get("bitrate_info"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let Some(url) = first_in_url_list(entry) else {
            continue;
        };
        let bitrate = entry
            .get("Bitrate")
            .or_else(|| entry.get("bit_rate"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let kbps = bitrate / 1000;
        rendition_ranks.push(kbps);
        let label = bitrate_label(entry, kbps, height);
        let stream = muxed(url, &extension);
        let id = choice_id(
            &mut ids,
            gear_name(entry)
                .map(str::to_string)
                .or_else(|| (kbps > 0).then(|| format!("{kbps}kbps"))),
            videos.len(),
        );
        videos.push(VideoChoice {
            id,
            label: label.clone(),
            width,
            height,
            fps: None,
            bitrate: (bitrate > 0).then_some(bitrate),
            codec: None,
            size: None,
            stream: stream.clone(),
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
        });
        options.push(MediaOption {
            label,
            rank: kbps,
            streams: vec![stream],
            filename: filename.clone(),
            width,
            height,
            duration_ms,
        });
    }

    // No `bitrateInfo` at all (older payloads, and some photo-mode posts): `playAddr` is
    // then the only rendition there is.
    if options.is_empty() {
        if let Some(url) = play_addr.clone() {
            let label = height.map_or("Video".to_string(), |h| format!("{h}p"));
            let stream = muxed(url, &extension);
            videos.push(VideoChoice {
                id: choice_id(&mut ids, None, videos.len()),
                label: label.clone(),
                width,
                height,
                fps: None,
                bitrate: None,
                codec: None,
                size: None,
                stream: stream.clone(),
                has_audio: true,
                best: false,
                // Both are computed by `rank_choices`, from the stream's own mime.
                container: None,
                mergeable: false,
            });
            options.push(MediaOption {
                label,
                rank: height.map_or(1, u64::from),
                streams: vec![stream],
                filename: filename.clone(),
                width,
                height,
                duration_ms,
            });
        }
    }

    // `downloadAddr` is the copy TikTok's own "save video" button uses. When it differs
    // from `playAddr` it is the un-watermarked encode, which is the one people want, so it
    // is ranked above every bitrate rendition rather than listed among them.
    //
    // `videos` does not repeat that opinion. TikTok states no bitrate for this copy, so
    // `Extraction::rank_choices` sorts it below an equal-resolution rendition that does
    // state one — and inventing a number here purely to win that comparison would be a
    // lie about what the site said. The ready-made list keeps the editorial ranking; the
    // choice list keeps the one ordering rule the whole product shares.
    if let Some(url) = download_addr.filter(|d| Some(d) != play_addr.as_ref()) {
        let top = rendition_ranks.iter().copied().max().unwrap_or(0);
        let stream = muxed(url, &extension);
        videos.push(VideoChoice {
            id: choice_id(&mut ids, Some("no-watermark".to_string()), videos.len()),
            label: match height {
                Some(h) => format!("Original (no watermark) · {h}p"),
                None => "Original (no watermark)".to_string(),
            },
            width,
            height,
            fps: None,
            bitrate: None,
            codec: None,
            size: None,
            stream: stream.clone(),
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
        });
        options.insert(
            0,
            MediaOption {
                label: "No watermark".to_string(),
                rank: top + 1,
                streams: vec![stream],
                filename: filename.clone(),
                width,
                height,
                duration_ms,
            },
        );
    }

    // The original sound, as a separate file. Rank 0 keeps it at the bottom: TikTok states
    // no bitrate for it, and guessing one would sort it into the middle of the video list.
    let mut audios: Vec<AudioChoice> = Vec::new();
    if let Some(url) = item
        .pointer("/music/playUrl")
        .or_else(|| item.pointer("/music/play_url"))
        .and_then(Value::as_str)
        .map(decode_json_escapes)
        .filter(|u| !u.is_empty())
    {
        let sound = item
            .pointer("/music/title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let label = match sound {
            Some(t) => format!("Original sound · {t}"),
            None => "Original sound".to_string(),
        };
        let stream = Stream {
            url,
            kind: StreamKind::AudioOnly,
            mime: Some("audio/mpeg".to_string()),
            size: None,
            headers: headers(),
            max_chunk: None,
        };
        // Deliberate, and the one place these five sites differ: the original sound is a
        // genuinely separate track TikTok publishes on its own, so it is listed as an
        // audio choice. It is *not* a partner for a video choice — every video above
        // already carries this same audio, which is what `has_audio: true` says — so
        // pairing the two is not a meaningful thing to ask for. A UI should read a TikTok
        // audio choice as an alternative to a video (download the sound by itself),
        // never as the other half of one.
        audios.push(AudioChoice {
            id: "original-sound".to_string(),
            label: label.clone(),
            bitrate: None,
            codec: None,
            language: None,
            size: None,
            stream: stream.clone(),
            best: false,
            // Both are computed by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
        });
        options.push(MediaOption {
            label,
            rank: 0,
            streams: vec![stream],
            filename: filename_for(title, author, "mp3"),
            width: None,
            height: None,
            duration_ms: None,
        });
    }

    options.sort_by_key(|o| std::cmp::Reverse(o.rank));
    Media {
        options,
        videos,
        audios,
    }
}

/// An id that is unique within one extraction and stable from one run to the next.
///
/// What TikTok called the encode is preferred — a saved choice should still mean the same
/// thing tomorrow, and `GearName` is the only durable name on offer. The position is the
/// fallback for a payload that names nothing, and the suffix is for the payload that
/// names two encodes the same.
fn choice_id(used: &mut Vec<String>, preferred: Option<String>, index: usize) -> String {
    let mut id = match preferred {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => format!("v{index}"),
    };
    if used.contains(&id) {
        id = format!("{id}-{index}");
    }
    used.push(id.clone());
    id
}

fn muxed(url: String, extension: &str) -> Stream {
    Stream {
        url,
        kind: StreamKind::Muxed,
        mime: Some(format!("video/{extension}")),
        size: None,
        headers: headers(),
        max_chunk: None,
    }
}

/// `GearName` is TikTok's own name for the encode ("normal_720_0", "adapt_lowest_1080_1").
/// It is shown as-is when present because it is the only thing distinguishing two entries
/// that share a resolution, with the bitrate spelled out beside it.
fn bitrate_label(entry: &Value, kbps: u64, height: Option<u32>) -> String {
    let gear = gear_name(entry);
    let quality = entry
        .get("QualityType")
        .or_else(|| entry.get("quality_type"))
        .and_then(Value::as_i64);
    let head = match (gear, height) {
        (Some(g), _) => g.to_string(),
        (None, Some(h)) => format!("{h}p"),
        (None, None) => match quality {
            Some(q) => format!("quality {q}"),
            None => "Video".to_string(),
        },
    };
    if kbps > 0 {
        format!("{head} · {kbps} kbps")
    } else {
        head
    }
}

/// `GearName`, in either spelling — the encode's own name, and the only durable thing
/// TikTok gives a rendition to be identified by.
fn gear_name(entry: &Value) -> Option<&str> {
    entry
        .get("GearName")
        .or_else(|| entry.get("gear_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|g| !g.is_empty())
}

/// `PlayAddr.UrlList[0]`, escapes decoded.
///
/// The list is CDN edges for one encode, not alternative qualities, so only the first is
/// taken; a [`Stream`] holds one URL.
fn first_in_url_list(entry: &Value) -> Option<String> {
    for path in ["/PlayAddr/UrlList", "/play_addr/url_list"] {
        if let Some(list) = entry.pointer(path).and_then(Value::as_array) {
            for u in list {
                if let Some(u) = u.as_str().filter(|u| !u.is_empty()) {
                    return Some(decode_json_escapes(u));
                }
            }
        }
    }
    None
}

fn url_of(video: &Value, key: &str) -> Option<String> {
    match video.get(key)? {
        Value::String(s) if !s.is_empty() => Some(decode_json_escapes(s)),
        // Some payloads state these as `{"url_list":[…]}` rather than a bare string.
        other => other
            .get("url_list")
            .and_then(Value::as_array)
            .and_then(|l| l.first())
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(decode_json_escapes),
    }
}

fn dimension(video: &Value, key: &str) -> Option<u32> {
    video
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

/// `duration` is documented as seconds and is stated as milliseconds by some payloads.
/// A TikTok longer than an hour does not exist in either shape, so a value that large is
/// unambiguously milliseconds — worth handling, because a duration a thousand times wrong
/// makes the UI's progress estimate nonsense.
fn duration_ms(video: &Value) -> Option<u64> {
    let raw = video.get("duration").and_then(Value::as_f64)?;
    if raw <= 0.0 {
        return None;
    }
    Some(if raw > 3600.0 {
        raw.round() as u64
    } else {
        (raw * 1000.0).round() as u64
    })
}

/// `@author - caption.mp4`, so a folder of downloads is sortable by who made them.
fn filename_for(title: &str, author: &str, extension: &str) -> String {
    if author.is_empty() {
        safe_filename(title, extension)
    } else {
        safe_filename(&format!("{author} - {title}"), extension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIVERSAL: &str = r#"<html><body><script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">
{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"statusCode":0,"itemInfo":{"itemStruct":{
"id":"7300000000000000001","desc":"a caption with a } brace","author":{"uniqueId":"someone"},
"music":{"playUrl":"https:\/\/sf.tiktokcdn.test\/music.mp3?a=1&b=2","title":"original sound - someone"},
"video":{"duration":15,"width":1080,"height":1920,"format":"mp4",
"cover":"https:\/\/p16.tiktokcdn.test\/cover.jpg",
"playAddr":"https:\/\/v16.tiktokcdn.test\/play.mp4?wm=1",
"downloadAddr":"https:\/\/v16.tiktokcdn.test\/download.mp4?wm=0",
"bitrateInfo":[
{"Bitrate":2000000,"QualityType":10,"GearName":"normal_1080_0","PlayAddr":{"UrlList":["https:\/\/v16.tiktokcdn.test\/hi.mp4?a=1&b=2"]}},
{"Bitrate":800000,"QualityType":20,"GearName":"normal_540_0","PlayAddr":{"UrlList":["https:\/\/v16.tiktokcdn.test\/lo.mp4"]}}]}}}}}}
</script></body></html>"#;

    const SIGI: &str = r#"<html><script id="SIGI_STATE" type="application/json">
{"ItemModule":{"7300000000000000001":{"id":"7300000000000000001","desc":"older shape","author":{"uniqueId":"legacy"},
"music":{"playUrl":"https:\/\/sf.tiktokcdn.test\/legacy.mp3"},
"video":{"duration":9,"width":576,"height":1024,"format":"mp4",
"playAddr":"https:\/\/v16.tiktokcdn.test\/legacy-play.mp4",
"downloadAddr":"https:\/\/v16.tiktokcdn.test\/legacy-play.mp4",
"bitrateInfo":[{"Bitrate":500000,"GearName":"normal_540_0","PlayAddr":{"UrlList":["https:\/\/v16.tiktokcdn.test\/legacy.mp4"]}}]}},
"7300000000000000002":{"desc":"a different video in the rail","video":{"playAddr":"https:\/\/v16.tiktokcdn.test\/other.mp4"}}}}
</script></html>"#;

    const URL: &str = "https://www.tiktok.com/@someone/video/7300000000000000001";

    fn extract(html: &str, url: &str) -> Extraction {
        let mut e = TikTok::new();
        assert_eq!(e.start(url).unwrap(), Step::Need(Need::PageState));
        match e.feed(&[html]).unwrap() {
            Step::Done(x) => x,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn tiktok_claims_its_own_hosts_and_the_short_links() {
        assert!(matches("www.tiktok.com"));
        assert!(matches("vt.tiktok.com"));
        assert!(matches("vm.tiktok.com"));
        assert!(!matches("nottiktok.com"));
        assert!(!matches("tiktok.com.evil.test"));
    }

    #[test]
    fn extraction_begins_by_asking_for_the_page_state() {
        let mut e = TikTok::new();
        assert_eq!(e.start(URL).unwrap(), Step::Need(Need::PageState));
        assert_eq!(e.site(), "TikTok");
    }

    #[test]
    fn the_rehydration_payload_yields_the_caption_and_every_bitrate() {
        let x = extract(UNIVERSAL, URL);
        assert_eq!(x.site, "TikTok");
        assert_eq!(x.title, "a caption with a } brace");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "No watermark",
                "normal_1080_0 · 2000 kbps",
                "normal_540_0 · 800 kbps",
                "Original sound · original sound - someone"
            ]
        );
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
    }

    #[test]
    fn the_download_address_is_offered_as_the_watermark_free_copy_and_ranked_first() {
        let x = extract(UNIVERSAL, URL);
        assert_eq!(x.options[0].label, "No watermark");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://v16.tiktokcdn.test/download.mp4?wm=0"
        );
        assert_eq!(x.options[0].streams[0].kind, StreamKind::Muxed);
    }

    #[test]
    fn escaped_urls_are_decoded_or_the_download_would_404() {
        let x = extract(UNIVERSAL, URL);
        let hi = &x.options[1].streams[0].url;
        assert_eq!(hi, "https://v16.tiktokcdn.test/hi.mp4?a=1&b=2");
        assert!(!hi.contains("\\/"), "backslashes survived: {hi}");
        assert!(!hi.contains("\\u0026"), "unicode escape survived: {hi}");
    }

    #[test]
    fn the_original_sound_is_offered_as_an_audio_only_file() {
        let x = extract(UNIVERSAL, URL);
        let sound = x.options.last().unwrap();
        assert_eq!(sound.streams[0].kind, StreamKind::AudioOnly);
        assert_eq!(
            sound.streams[0].url,
            "https://sf.tiktokcdn.test/music.mp3?a=1&b=2"
        );
        assert_eq!(sound.filename, "someone - a caption with a } brace.mp3");
    }

    #[test]
    fn every_stream_carries_the_tiktok_referer() {
        let x = extract(UNIVERSAL, URL);
        for option in &x.options {
            for stream in &option.streams {
                assert!(
                    stream
                        .headers
                        .contains(&("Referer".to_string(), REFERER.to_string())),
                    "{} is missing the Referer",
                    option.label
                );
            }
        }
    }

    #[test]
    fn dimensions_and_duration_come_through_for_the_ui() {
        let x = extract(UNIVERSAL, URL);
        assert_eq!(x.options[0].width, Some(1080));
        assert_eq!(x.options[0].height, Some(1920));
        assert_eq!(x.options[0].duration_ms, Some(15_000));
    }

    #[test]
    fn the_older_sigi_shape_is_parsed_the_same_way() {
        let x = extract(SIGI, URL);
        assert_eq!(x.title, "older shape");
        // `downloadAddr` equals `playAddr` here, so no "no watermark" option is invented.
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["normal_540_0 · 500 kbps", "Original sound"]);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://v16.tiktokcdn.test/legacy.mp4"
        );
        assert_eq!(x.options[0].filename, "legacy - older shape.mp4");
    }

    #[test]
    fn the_sigi_shape_picks_the_item_the_url_names_not_whatever_is_first() {
        let x = extract(SIGI, "https://www.tiktok.com/@x/video/7300000000000000002");
        assert_eq!(x.title, "a different video in the rail");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://v16.tiktokcdn.test/other.mp4"
        );
    }

    #[test]
    fn a_short_link_with_no_id_falls_back_to_the_first_item() {
        let x = extract(SIGI, "https://vm.tiktok.com/ZSabcdef/");
        assert_eq!(x.title, "older shape");
    }

    #[test]
    fn a_payload_with_no_bitrate_list_still_offers_the_play_address() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"desc":"bare","video":{"height":1024,"playAddr":"https:\/\/v16.tiktokcdn.test\/only.mp4"}}}}}}</script>"#;
        let x = extract(html, URL);
        assert_eq!(x.options.len(), 1);
        assert_eq!(x.options[0].label, "1024p");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://v16.tiktokcdn.test/only.mp4"
        );
    }

    #[test]
    fn a_url_list_shaped_play_address_is_read_too() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"desc":"listy","video":{"play_addr":{"url_list":["https:\/\/v16.tiktokcdn.test\/listed.mp4"]}}}}}}}</script>"#;
        let x = extract(html, URL);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://v16.tiktokcdn.test/listed.mp4"
        );
    }

    #[test]
    fn a_non_zero_status_code_is_reported_as_unavailable_in_a_readable_sentence() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"statusCode":10204,"statusMsg":"item doesn't exist"}}}</script>"#;
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        let err = e.feed(&[html]).unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(
            message.contains("private, deleted, or restricted"),
            "{message}"
        );
        assert!(message.contains("signed into"), "{message}");
        assert!(message.contains("item doesn't exist"), "{message}");
        assert_eq!(err.to_string(), *message);
    }

    #[test]
    fn a_missing_item_struct_is_unavailable_rather_than_a_shape_change() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"statusCode":0,"itemInfo":{}}}}</script>"#;
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        assert!(matches!(e.feed(&[html]), Err(SiteError::Unavailable(_))));
    }

    #[test]
    fn an_item_with_metadata_but_no_media_is_unavailable() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"desc":"taken down","video":{}}}}}}</script>"#;
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        assert!(matches!(e.feed(&[html]), Err(SiteError::Unavailable(_))));
    }

    #[test]
    fn a_page_with_neither_payload_reports_shape_and_names_the_site() {
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        assert_eq!(
            e.feed(&["<html>a bot wall</html>"]).unwrap_err(),
            SiteError::Shape("TikTok".into())
        );
    }

    #[test]
    fn an_unparseable_payload_reports_shape() {
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        assert!(matches!(
            e.feed(&[r#"<script id="SIGI_STATE">not json at all {</script>"#]),
            Err(SiteError::Shape(_))
        ));
    }

    #[test]
    fn feeding_nothing_reports_shape_instead_of_panicking() {
        let mut e = TikTok::new();
        e.start(URL).unwrap();
        assert!(matches!(e.feed(&[]), Err(SiteError::Shape(_))));
    }

    #[test]
    fn a_duration_already_in_milliseconds_is_not_multiplied_again() {
        let seconds = serde_json::json!({"duration": 15});
        assert_eq!(duration_ms(&seconds), Some(15_000));
        let millis = serde_json::json!({"duration": 15000});
        assert_eq!(duration_ms(&millis), Some(15_000));
        assert_eq!(duration_ms(&serde_json::json!({"duration": 0})), None);
        assert_eq!(duration_ms(&serde_json::json!({})), None);
    }

    #[test]
    fn every_muxed_rendition_is_also_a_video_choice_that_needs_no_audio_picked() {
        let x = extract(UNIVERSAL, URL);
        let muxed: Vec<&MediaOption> = x
            .options
            .iter()
            .filter(|o| o.streams[0].kind == StreamKind::Muxed)
            .collect();
        assert!(!x.videos.is_empty());
        assert_eq!(x.videos.len(), muxed.len());
        for option in &muxed {
            assert!(
                x.videos.iter().any(|v| v.stream == option.streams[0]),
                "{} is offered but is not a video choice",
                option.label
            );
        }
        assert!(
            x.videos.iter().all(|v| v.has_audio),
            "every TikTok rendition carries its own sound"
        );
        assert!(x.videos.iter().all(|v| v.stream.kind == StreamKind::Muxed));
        let mut ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two choices share an id");
    }

    #[test]
    fn exactly_one_video_choice_is_best_and_it_is_the_top_of_the_ranked_list() {
        let x = extract(UNIVERSAL, URL);
        assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1);
        assert!(x.videos[0].best);
        // Named by the encode rather than by position, so a saved choice still means the
        // same rendition after the page is read again.
        let ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["normal_1080_0", "normal_540_0", "no-watermark"]);
        assert_eq!(x.best_video().map(|v| v.id.as_str()), Some("normal_1080_0"));
        assert_eq!(x.videos[0].bitrate, Some(2_000_000));
        assert_eq!(x.videos[0].height, Some(1920));
        // The un-watermarked copy states no bitrate, so the product-wide ranking rule puts
        // it last rather than this file inventing a number to win with. `options` still
        // offers it first, which is where that opinion belongs.
        assert_eq!(x.options[0].label, "No watermark");
        let last = x.videos.last().unwrap();
        assert_eq!(last.label, "Original (no watermark) · 1920p");
        assert_eq!(last.bitrate, None);
        assert!(!last.best);
    }

    #[test]
    fn the_original_sound_is_an_audio_choice_and_an_alternative_rather_than_a_partner() {
        let x = extract(UNIVERSAL, URL);
        assert_eq!(x.audios.len(), 1);
        let sound = &x.audios[0];
        assert_eq!(sound.id, "original-sound");
        assert_eq!(sound.label, "Original sound · original sound - someone");
        assert_eq!(sound.stream.kind, StreamKind::AudioOnly);
        assert!(sound.best);
        assert_eq!(
            x.best_audio().map(|a| a.id.as_str()),
            Some("original-sound")
        );
        // Every video choice already carries this same sound — which is exactly what
        // `has_audio` says — so the audio choice is something to download instead of a
        // video, never the other half of one.
        assert!(x.videos.iter().all(|v| v.has_audio));
    }

    #[test]
    fn the_older_shape_yields_choices_too_and_invents_no_watermark_free_copy() {
        let x = extract(SIGI, URL);
        assert_eq!(x.videos.len(), 1);
        assert_eq!(x.videos[0].id, "normal_540_0");
        assert_eq!(x.videos[0].bitrate, Some(500_000));
        assert!(x.videos[0].has_audio);
        assert!(x.videos[0].best);
        assert_eq!(x.audios.len(), 1);
        assert_eq!(x.audios[0].label, "Original sound");
        assert!(x.audios[0].best);
    }

    #[test]
    fn a_payload_that_names_nothing_falls_back_to_the_position_for_an_id() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"desc":"bare","video":{"height":1024,"playAddr":"https:\/\/v16.tiktokcdn.test\/only.mp4"}}}}}}</script>"#;
        let x = extract(html, URL);
        assert_eq!(x.videos.len(), 1);
        assert_eq!(x.videos[0].id, "v0");
        assert_eq!(x.videos[0].label, "1024p");
        assert!(x.videos[0].has_audio);
        assert!(x.videos[0].best);
        assert!(x.audios.is_empty(), "this payload states no music");
    }
}
