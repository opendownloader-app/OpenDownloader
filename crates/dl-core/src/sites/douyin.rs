//! Douyin — `douyin.com`, `iesdouyin.com`, `ixigua.com`.
//!
//! # The shape this was built against
//!
//! A Douyin watch page assigns its router payload inline as `window._ROUTER_DATA = {…}`.
//! The item sits under `loaderData["video_(<id>)/page"].videoInfoRes`, as either
//! `item_list[0]` or `aweme_detail` depending on which renderer served the page. Share
//! links on `iesdouyin.com` use the same payload with the same keys.
//!
//! `window.__INITIAL_STATE__` is parsed as a fallback. Its layout differs between page
//! generations far more than the router payload's does, so rather than pinning a path
//! this walks the tree for the first object holding a `video.play_addr` — the one thing
//! every generation of the shape agrees on.
//!
//! **Honesty about provenance:** written on 2026-09-05 against the documented shape and
//! *unverified against a live page*; Douyin serves outside fetches a verification page,
//! so no real fixture could be captured, and the tests assert against hand-written
//! samples. `ixigua.com` is claimed here because it is Douyin's sibling and its media
//! host is shared, but its own page payload was **not** modelled — an ixigua page whose
//! layout does not contain a `video.play_addr` object reports [`SiteError::Shape`] rather
//! than pretending.

use super::bilibili::{decode_html_entities, html_title, json_object_after};
use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, SiteError, Step, Stream,
    StreamKind, VideoChoice,
};
use serde_json::Value;

pub fn matches(host: &str) -> bool {
    host_is(host, "douyin.com") || host_is(host, "iesdouyin.com") || host_is(host, "ixigua.com")
}

const SITE: &str = "Douyin";
const REFERER: &str = "https://www.douyin.com/";

fn shape() -> SiteError {
    SiteError::Shape(SITE.to_string())
}

fn headers() -> Vec<(String, String)> {
    // Douyin's media hosts reject a request with no `Referer` the same way bilibili's do.
    vec![("Referer".to_string(), REFERER.to_string())]
}

#[derive(Debug, Default)]
pub struct Douyin {
    page_url: String,
}

impl Douyin {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Douyin {
    fn site(&self) -> &'static str {
        SITE
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        // Kept because `loaderData` is keyed by the video id, which only the URL states.
        self.page_url = url.to_string();
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let html = bodies.first().copied().ok_or_else(shape)?;
        Ok(Step::Done(parse_video_page(html, &self.page_url)?))
    }
}

/// Turn a watch page's HTML into an [`Extraction`]. Pure; the tests call it directly.
pub fn parse_video_page(html: &str, page_url: &str) -> Result<Extraction, SiteError> {
    let id = video_id(page_url);
    let aweme = aweme_from_router(html, id.as_deref())
        .or_else(|| aweme_from_initial_state(html))
        .ok_or_else(shape)?;

    let title = aweme
        .get("desc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .or_else(|| html_title(html).map(|t| decode_html_entities(&t)))
        .unwrap_or_else(|| "Douyin video".to_string());

    let media = media_for(&aweme, &title);
    if media.options.is_empty() {
        return Err(SiteError::Unavailable(
            "Douyin stated no playable address for this video — it is private, deleted, or \
             friends-only. Opening it in a tab you are signed into is what usually fixes it."
                .to_string(),
        ));
    }
    let mut extraction = Extraction {
        site: SITE.to_string(),
        title,
        options: media.options,
        videos: media.videos,
        // Nothing to pair: every Douyin address is one file carrying both tracks.
        audios: Vec::new(),
        subtitles: Vec::new(),
    };
    // The one place "best" is decided, for every site alike.
    extraction.rank_choices();
    Ok(extraction)
}

/// `window._ROUTER_DATA` → `loaderData[…].videoInfoRes` → the item.
fn aweme_from_router(html: &str, id: Option<&str>) -> Option<Value> {
    let text = json_object_after(html, "window._ROUTER_DATA")?;
    let root: Value = serde_json::from_str(text).ok()?;
    let loader = root.get("loaderData")?.as_object()?;

    // The documented key is `video_(<id>)/page`, parentheses included. Falling back to
    // "any route that looks like a video page" keeps share links working, where the id in
    // the URL is not the id in the key.
    let route = id
        .and_then(|id| loader.get(&format!("video_({id})/page")))
        .or_else(|| {
            loader
                .iter()
                .find(|(k, _)| k.starts_with("video_") && k.ends_with("/page"))
                .map(|(_, v)| v)
        })
        .or_else(|| loader.values().find(|v| v.get("videoInfoRes").is_some()))?;

    let info = route.get("videoInfoRes").unwrap_or(route);
    info.pointer("/item_list/0")
        .or_else(|| info.get("aweme_detail"))
        .cloned()
        // Some renders drop `videoInfoRes` and put the item straight on the route.
        .or_else(|| find_aweme(route).cloned())
}

/// `window.__INITIAL_STATE__`, searched rather than indexed.
fn aweme_from_initial_state(html: &str) -> Option<Value> {
    let text = json_object_after(html, "window.__INITIAL_STATE__")?;
    let root: Value = serde_json::from_str(text).ok()?;
    find_aweme(&root).cloned()
}

/// The first object in the tree that carries a `video.play_addr`.
///
/// A structural search rather than a path because that pairing is the only thing every
/// version of every Douyin payload has in common; a path would be a fifth thing to keep
/// in sync with a site nobody here can observe.
fn find_aweme(v: &Value) -> Option<&Value> {
    if v.pointer("/video/play_addr").is_some() || v.pointer("/video/play_addr_265").is_some() {
        return Some(v);
    }
    match v {
        Value::Object(map) => map.values().find_map(find_aweme),
        Value::Array(items) => items.iter().find_map(find_aweme),
        _ => None,
    }
}

/// Strip Douyin's watermark by asking for the same encode on the unstamped route.
///
/// A `play_addr` normally points at `…/aweme/v1/playwm/?video_id=…`. The `playwm` route
/// burns the Douyin logo and the author's ID into the picture; the byte-identical request
/// against `play` returns the same encode without them. That single substring substitution
/// is the whole of the "no watermark" trick — no signing, no second request — which is why
/// the clean URL is offered first and the original is kept as a fallback for the day the
/// route stops answering.
fn without_watermark(url: &str) -> Option<String> {
    url.contains("playwm")
        .then(|| url.replacen("playwm", "play", 1))
}

/// The ready-made options and the per-track choices, built from one walk of the item.
///
/// Douyin serves nothing but muxed files, so every choice here carries `has_audio: true`
/// and the audio list stays empty — there is no second half to pick, and a UI reading an
/// empty `audios` correctly shows no audio picker at all.
struct Media {
    options: Vec<MediaOption>,
    videos: Vec<VideoChoice>,
}

fn media_for(aweme: &Value, title: &str) -> Media {
    let video = aweme.get("video").cloned().unwrap_or(Value::Null);
    let width = dimension(&video, "width");
    let height = dimension(&video, "height");
    let duration_ms = duration_ms(aweme, &video);
    let filename = safe_filename(title, "mp4");

    let mut options: Vec<MediaOption> = Vec::new();
    let mut videos: Vec<VideoChoice> = Vec::new();
    let mut ids: Vec<String> = Vec::new();

    let mut gears: Vec<&Value> = video
        .get("bit_rate")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    gears.sort_by_key(|g| std::cmp::Reverse(bitrate_of(g)));

    for gear in gears {
        let Some(url) = url_list_first(gear.get("play_addr")) else {
            continue;
        };
        let bitrate = bitrate_of(gear);
        let kbps = bitrate / 1000;
        let gear_width = dimension(gear, "width").or(width);
        let gear_height = dimension(gear, "height").or(height);
        let label = gear_label(gear, kbps, gear_height);
        // The clean route when the substitution applies, the original when it does
        // not — the user never has to choose the watermarked copy by mistake.
        let stream = muxed(without_watermark(&url).unwrap_or(url));
        let id = choice_id(
            &mut ids,
            gear_name(gear)
                .map(str::to_string)
                .or_else(|| (kbps > 0).then(|| format!("{kbps}kbps"))),
            videos.len(),
        );
        videos.push(VideoChoice {
            id,
            label: label.clone(),
            width: gear_width,
            height: gear_height,
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
            rank: if kbps > 0 {
                kbps
            } else {
                height.map_or(1, u64::from)
            },
            streams: vec![stream],
            filename: filename.clone(),
            width: gear_width,
            height: gear_height,
            duration_ms,
        });
    }

    let play_addr = url_list_first(video.get("play_addr"));
    if options.is_empty() {
        if let Some(url) = play_addr.clone() {
            let label = height.map_or("Video".to_string(), |h| format!("{h}p"));
            let stream = muxed(without_watermark(&url).unwrap_or(url));
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

    // The HEVC encode of the same clip: the same picture in roughly half the bytes. It
    // shares the top rank so it sits beside the best option rather than at the bottom, and
    // the stable sort keeps it just below, because not every player takes H.265.
    let best_rank = options.iter().map(|o| o.rank).max().unwrap_or(1);
    if let Some(url) = url_list_first(video.get("play_addr_265")) {
        let clean = without_watermark(&url).unwrap_or(url);
        if !options
            .iter()
            .any(|o| o.streams.iter().any(|s| s.url == clean))
        {
            let stream = muxed(clean);
            videos.push(VideoChoice {
                id: choice_id(&mut ids, Some("h265".to_string()), videos.len()),
                label: match height {
                    Some(h) => format!("H.265 · {h}p"),
                    None => "H.265".to_string(),
                },
                width,
                height,
                fps: None,
                // Douyin states no bitrate for this address, and `rank_choices` will place
                // it accordingly rather than have this file invent a number to outrank a
                // gear with.
                bitrate: None,
                codec: Some("hevc".to_string()),
                size: None,
                stream: stream.clone(),
                has_audio: true,
                best: false,
                // Both are computed by `rank_choices`, from the stream's own mime.
                container: None,
                mergeable: false,
            });
            options.push(MediaOption {
                label: "H.265".to_string(),
                rank: best_rank,
                streams: vec![stream],
                filename: filename.clone(),
                width,
                height,
                duration_ms,
            });
        }
    }

    // The watermarked original, kept as a rescue. Rank 0 puts it last, which is what
    // "fallback" has to mean in a list the UI sorts for the user. It is a choice like any
    // other — a whole muxed file — so it is listed as one, named for what is wrong with
    // it rather than hidden.
    if let Some(url) = play_addr.filter(|u| u.contains("playwm")) {
        let stream = muxed(url);
        videos.push(VideoChoice {
            id: choice_id(&mut ids, Some("watermarked".to_string()), videos.len()),
            label: "Original (watermarked)".to_string(),
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
            label: "Original (watermarked)".to_string(),
            rank: 0,
            streams: vec![stream],
            filename: filename.clone(),
            width,
            height,
            duration_ms,
        });
    }

    options.sort_by_key(|o| std::cmp::Reverse(o.rank));
    Media { options, videos }
}

/// An id that is unique within one extraction and stable from one run to the next.
///
/// `gear_name` is Douyin's own name for an encode and is what a saved choice should still
/// mean tomorrow; the position is the fallback for a payload that names nothing, and the
/// suffix is for the payload that names two gears the same.
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

fn muxed(url: String) -> Stream {
    Stream {
        url,
        kind: StreamKind::Muxed,
        mime: Some("video/mp4".to_string()),
        size: None,
        headers: headers(),
        max_chunk: None,
    }
}

fn gear_label(gear: &Value, kbps: u64, height: Option<u32>) -> String {
    let name = gear_name(gear);
    let head = match (name, height) {
        (Some(n), _) => n.to_string(),
        (None, Some(h)) => format!("{h}p"),
        (None, None) => "Video".to_string(),
    };
    if kbps > 0 {
        format!("{head} · {kbps} kbps")
    } else {
        head
    }
}

/// Douyin's own name for an encode ("normal_1080_0"), the one durable thing a rendition
/// can be identified by from one extraction to the next.
fn gear_name(gear: &Value) -> Option<&str> {
    gear.get("gear_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

fn bitrate_of(gear: &Value) -> u64 {
    gear.get("bit_rate").and_then(Value::as_u64).unwrap_or(0)
}

/// The first entry of a `{"url_list":[…]}` address.
///
/// The list is CDN edges for one encode, not alternative qualities, so only the first is
/// taken. `uri` is skipped deliberately: it is an internal id, not something fetchable.
fn url_list_first(addr: Option<&Value>) -> Option<String> {
    let addr = addr?;
    if let Some(list) = addr.get("url_list").and_then(Value::as_array) {
        for u in list {
            if let Some(u) = u.as_str().filter(|u| u.starts_with("http")) {
                return Some(u.to_string());
            }
        }
    }
    // Some payloads state the address as a bare string.
    addr.as_str()
        .filter(|u| u.starts_with("http"))
        .map(str::to_string)
}

fn dimension(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

/// Douyin states `duration` in milliseconds, on the item or on the video.
fn duration_ms(aweme: &Value, video: &Value) -> Option<u64> {
    video
        .get("duration")
        .or_else(|| aweme.get("duration"))
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
}

/// The numeric id of the video a URL names.
///
/// Two shapes, because Douyin has two. A permalink puts the id in the path —
/// `…/video/<id>`, or `…/share/video/<id>/`. Everywhere else the video opens as an
/// overlay on top of whatever page you were on, and the id is a query parameter instead:
/// `douyin.com/jingxuan?modal_id=<id>`, and the same on `/discover`, a user's page and
/// the home feed.
///
/// Reading only the path form is why a `modal_id` link failed. The page it loads
/// describes several videos — the feed underneath the overlay — so with no id to pick
/// with, the extractor either took the wrong one or found nothing it could tie to the
/// URL. The id is what makes the choice unambiguous, and it was there all along.
fn video_id(page_url: &str) -> Option<String> {
    if let Some(after) = page_url.split("/video/").nth(1) {
        let id = after.split(['/', '?', '#']).next().unwrap_or("");
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    modal_id(page_url)
}

/// `modal_id=<id>` from the query string, whichever parameter position it is in.
fn modal_id(page_url: &str) -> Option<String> {
    let query = page_url.split(['?', '#']).nth(1)?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("modal_id=") {
            let id = value.split('#').next().unwrap_or("");
            // Douyin's ids are decimal. Checking keeps a stray `modal_id=login` or an
            // empty value from being handed on as though it named a video.
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_read_from_a_permalink_or_from_modal_id() {
        // The path form, which always worked.
        assert_eq!(
            video_id("https://www.douyin.com/video/7300000000000000001").as_deref(),
            Some("7300000000000000001")
        );
        assert_eq!(
            video_id("https://www.iesdouyin.com/share/video/999/?region=SG").as_deref(),
            Some("999")
        );
        // The overlay form, which did not. This is the link a person copies from the
        // feed, and it is the common one.
        assert_eq!(
            video_id("https://www.douyin.com/jingxuan?modal_id=7681584405361560878").as_deref(),
            Some("7681584405361560878")
        );
        assert_eq!(
            video_id("https://www.douyin.com/discover?a=1&modal_id=42&b=2").as_deref(),
            Some("42")
        );
        assert_eq!(
            video_id("https://www.douyin.com/user/MS4wLjAB?modal_id=7#play").as_deref(),
            Some("7")
        );
        // A page with no video named at all, and a non-numeric value that is not an id.
        assert_eq!(video_id("https://www.douyin.com/jingxuan"), None);
        assert_eq!(video_id("https://www.douyin.com/jingxuan?modal_id="), None);
        assert_eq!(video_id("https://www.douyin.com/jingxuan?modal_id=login"), None);
    }

    const ROUTER_PAGE: &str = r#"<html><head><title>ignored</title></head><body>
<script>window._ROUTER_DATA = {"loaderData":{"video_(7300000000000000001)/page":{"videoInfoRes":{"item_list":[{
"aweme_id":"7300000000000000001","desc":"一个测试 } video","duration":15000,
"video":{"width":1080,"height":1920,"duration":15000,
"play_addr":{"uri":"v0300","url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=v0300&ratio=720p"]},
"play_addr_265":{"url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=v0300&is_h265=1"]},
"bit_rate":[
{"bit_rate":600000,"gear_name":"normal_540_0","quality_type":20,"play_addr":{"url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=lo"]}},
{"bit_rate":1800000,"gear_name":"normal_1080_0","quality_type":10,"play_addr":{"width":1080,"height":1920,"url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=hi"]}}]}}]}}},"errors":{}};</script>
</body></html>"#;

    const URL: &str = "https://www.douyin.com/video/7300000000000000001";

    fn extract(html: &str, url: &str) -> Extraction {
        let mut e = Douyin::new();
        assert_eq!(e.start(url).unwrap(), Step::Need(Need::PageState));
        match e.feed(&[html]).unwrap() {
            Step::Done(x) => x,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn douyin_claims_its_own_hosts_and_not_lookalikes() {
        assert!(matches("www.douyin.com"));
        assert!(matches("www.iesdouyin.com"));
        assert!(matches("ixigua.com"));
        assert!(!matches("notdouyin.com"));
        assert!(!matches("douyin.com.evil.test"));
    }

    #[test]
    fn extraction_begins_by_asking_for_the_page_state() {
        let mut e = Douyin::new();
        assert_eq!(e.start(URL).unwrap(), Step::Need(Need::PageState));
        assert_eq!(e.site(), "Douyin");
    }

    #[test]
    fn the_router_payload_yields_every_gear_best_first() {
        let x = extract(ROUTER_PAGE, URL);
        assert_eq!(x.site, "Douyin");
        assert_eq!(x.title, "一个测试 } video");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "normal_1080_0 · 1800 kbps",
                "H.265",
                "normal_540_0 · 600 kbps",
                "Original (watermarked)"
            ]
        );
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
    }

    #[test]
    fn the_playwm_route_is_swapped_for_the_clean_one() {
        let x = extract(ROUTER_PAGE, URL);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://aweme.snssdk.test/aweme/v1/play/?video_id=hi"
        );
        for option in &x.options {
            if option.label != "Original (watermarked)" {
                assert!(
                    !option.streams[0].url.contains("playwm"),
                    "{} still points at the watermarked route",
                    option.label
                );
            }
        }
    }

    #[test]
    fn the_watermarked_original_is_kept_as_the_last_resort() {
        let x = extract(ROUTER_PAGE, URL);
        let fallback = x.options.last().unwrap();
        assert_eq!(fallback.label, "Original (watermarked)");
        assert_eq!(fallback.rank, 0);
        assert!(fallback.streams[0].url.contains("playwm"));
    }

    #[test]
    fn a_url_that_was_never_watermarked_is_left_exactly_as_it_came() {
        assert_eq!(without_watermark("https://x.test/play/?v=1"), None);
        assert_eq!(
            without_watermark("https://x.test/playwm/?v=1").as_deref(),
            Some("https://x.test/play/?v=1")
        );
    }

    #[test]
    fn a_page_whose_addresses_are_already_clean_offers_no_watermarked_fallback() {
        let html = r#"<script>window._ROUTER_DATA={"loaderData":{"video_(1)/page":{"videoInfoRes":{"aweme_detail":{"desc":"clean","video":{"height":720,"play_addr":{"url_list":["https://aweme.snssdk.test/aweme/v1/play/?video_id=x"]}}}}}}}</script>"#;
        let x = extract(html, "https://www.douyin.com/video/1");
        assert_eq!(x.options.len(), 1);
        assert_eq!(x.options[0].label, "720p");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://aweme.snssdk.test/aweme/v1/play/?video_id=x"
        );
    }

    #[test]
    fn the_aweme_detail_spelling_is_read_as_well_as_item_list() {
        let html = r#"<script>window._ROUTER_DATA={"loaderData":{"video_(9)/page":{"videoInfoRes":{"aweme_detail":{"desc":"detail shape","duration":8000,"video":{"play_addr":{"url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=d"]}}}}}}}</script>"#;
        let x = extract(html, "https://www.douyin.com/video/9");
        assert_eq!(x.title, "detail shape");
        assert_eq!(x.options[0].duration_ms, Some(8000));
    }

    #[test]
    fn a_share_link_whose_route_key_does_not_match_the_url_still_resolves() {
        let x = extract(ROUTER_PAGE, "https://www.iesdouyin.com/share/video/999/");
        assert_eq!(x.title, "一个测试 } video");
    }

    #[test]
    fn the_initial_state_shape_is_found_by_searching_for_a_play_address() {
        let html = r#"<script>window.__INITIAL_STATE__={"anything":{"nested":{"deeper":{"desc":"from initial state","video":{"height":1024,"play_addr":{"url_list":["https://aweme.snssdk.test/aweme/v1/playwm/?video_id=is"]}}}}}}</script>"#;
        let x = extract(html, URL);
        assert_eq!(x.title, "from initial state");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://aweme.snssdk.test/aweme/v1/play/?video_id=is"
        );
    }

    #[test]
    fn every_stream_carries_the_douyin_referer() {
        let x = extract(ROUTER_PAGE, URL);
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
    fn dimensions_come_from_the_gear_when_it_states_them_and_the_video_otherwise() {
        let x = extract(ROUTER_PAGE, URL);
        assert_eq!(x.options[0].width, Some(1080));
        assert_eq!(x.options[0].height, Some(1920));
        assert_eq!(x.options[0].duration_ms, Some(15_000));
    }

    #[test]
    fn a_caption_free_video_falls_back_to_the_document_title() {
        let html = r#"<title>Titled by the document</title><script>window._ROUTER_DATA={"loaderData":{"video_(1)/page":{"videoInfoRes":{"aweme_detail":{"desc":"   ","video":{"play_addr":{"url_list":["https://x.test/play/?v=1"]}}}}}}}</script>"#;
        assert_eq!(extract(html, URL).title, "Titled by the document");
    }

    #[test]
    fn a_page_with_no_router_data_reports_shape_and_names_the_site() {
        let mut e = Douyin::new();
        e.start(URL).unwrap();
        assert_eq!(
            e.feed(&["<html>a verification page</html>"]).unwrap_err(),
            SiteError::Shape("Douyin".into())
        );
    }

    #[test]
    fn an_item_with_no_fetchable_address_is_unavailable_not_a_shape_change() {
        let html = r#"<script>window._ROUTER_DATA={"loaderData":{"video_(1)/page":{"videoInfoRes":{"aweme_detail":{"desc":"gone","video":{"play_addr":{"uri":"v0300","url_list":[]}}}}}}}</script>"#;
        let mut e = Douyin::new();
        e.start(URL).unwrap();
        let err = e.feed(&[html]).unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(message.contains("signed into"), "{message}");
    }

    #[test]
    fn an_internal_uri_is_never_mistaken_for_a_fetchable_url() {
        let addr = serde_json::json!({"uri": "v0300abc", "url_list": ["v0300abc"]});
        assert_eq!(url_list_first(Some(&addr)), None);
    }

    #[test]
    fn feeding_nothing_reports_shape_instead_of_panicking() {
        let mut e = Douyin::new();
        e.start(URL).unwrap();
        assert!(matches!(e.feed(&[]), Err(SiteError::Shape(_))));
    }

    #[test]
    fn every_option_is_also_a_muxed_video_choice_that_needs_no_audio_picked() {
        let x = extract(ROUTER_PAGE, URL);
        assert!(!x.videos.is_empty());
        assert_eq!(x.videos.len(), x.options.len());
        for option in &x.options {
            assert!(
                x.videos.iter().any(|v| v.stream == option.streams[0]),
                "{} is offered but is not a video choice",
                option.label
            );
        }
        assert!(
            x.videos.iter().all(|v| v.has_audio),
            "every Douyin address is one file carrying both tracks"
        );
        assert!(x.videos.iter().all(|v| v.stream.kind == StreamKind::Muxed));
        assert!(x.audios.is_empty(), "Douyin never serves the sound apart");
        let mut ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two choices share an id");
    }

    #[test]
    fn exactly_one_video_choice_is_best_and_it_is_the_top_gear() {
        let x = extract(ROUTER_PAGE, URL);
        assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1);
        assert!(x.videos[0].best);
        // The gear name is the id, so a saved pick still means the same encode after the
        // page is read again; the two addresses Douyin does not name fall back to a name
        // for what they are.
        let ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            ["normal_1080_0", "normal_540_0", "h265", "watermarked"]
        );
        assert_eq!(x.best_video().map(|v| v.id.as_str()), Some("normal_1080_0"));
        assert_eq!(x.videos[0].label, "normal_1080_0 · 1800 kbps");
        assert_eq!(x.videos[0].bitrate, Some(1_800_000));
        assert_eq!(x.videos[0].height, Some(1920));
        assert!(!x.videos[0].stream.url.contains("playwm"));
    }

    #[test]
    fn the_hevc_choice_names_its_codec_because_not_every_player_takes_it() {
        let x = extract(ROUTER_PAGE, URL);
        let hevc = x
            .videos
            .iter()
            .find(|v| v.id == "h265")
            .expect("the H.265 encode is a choice too");
        assert_eq!(hevc.codec.as_deref(), Some("hevc"));
        assert_eq!(hevc.label, "H.265 · 1920p");
        assert!(hevc.has_audio);
        assert!(!hevc.stream.url.contains("playwm"));
    }

    #[test]
    fn a_page_with_one_address_and_no_gears_still_produces_one_flagged_choice() {
        let html = r#"<script>window._ROUTER_DATA={"loaderData":{"video_(1)/page":{"videoInfoRes":{"aweme_detail":{"desc":"clean","video":{"height":720,"play_addr":{"url_list":["https://aweme.snssdk.test/aweme/v1/play/?video_id=x"]}}}}}}}</script>"#;
        let x = extract(html, "https://www.douyin.com/video/1");
        assert_eq!(x.videos.len(), 1);
        assert_eq!(x.videos[0].id, "v0");
        assert_eq!(x.videos[0].label, "720p");
        assert!(x.videos[0].has_audio);
        assert!(x.videos[0].best);
        assert!(x.audios.is_empty());
    }
}
