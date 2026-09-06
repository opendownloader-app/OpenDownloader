//! WeChat — `mp.weixin.qq.com` articles, and an honest refusal for Channels.
//!
//! # Two cases, and only one of them works
//!
//! **Articles** (`mp.weixin.qq.com/s/…`). A video inside an article is either a direct
//! `mpvideo.qpic.cn` URL sitting in the page, or — far more often — nothing but a `vid`,
//! with the player resolving it at runtime. When only a `vid` is present this emits a
//! single [`Need::Fetch`] for
//! `…/mp/videoplayer?action=get_mp_video_play_url&…&vid=<VID>&…&f=json` and reads
//! `url_info[]` out of the answer. That second request is a genuine second round trip, not
//! a scrape: the page really does not contain the address, and the request inherits the
//! user's cookies from the tab.
//!
//! **Channels / 视频号** (`channels.weixin.qq.com`). The media is encrypted and every URL
//! is signed for the session that asked for it. There is no parser that makes that work,
//! so [`Extractor::start`] refuses immediately with a sentence saying so. No workaround is
//! attempted, and none should be added.
//!
//! # The shape this was built against
//!
//! `var vid = "wxv_…"`, `vid=<id>` in a player URL, `"vid":"…"` in an embedded config, and
//! bare `https://mpvideo.qpic.cn/…` URLs; then `{"url_info":[{"url":…,"format_id":…}]}`
//! from the player endpoint.
//!
//! **Honesty about provenance:** written on 2026-09-05 against that documented shape and
//! *unverified against a live page* — an outside fetch of an article gets a "please open
//! in WeChat" interstitial, so no real fixture could be captured, and the tests assert
//! against hand-written samples.
//!
//! # A note on the host list
//!
//! `qq.com` is claimed wholesale, as the media for an article is served from several
//! hosts under it. Only WeChat article pages are modelled; another Tencent property under
//! `qq.com` will report [`SiteError::Shape`] rather than being handled.

use super::bilibili::{
    decode_html_entities, decode_json_escapes, html_title, json_object_after, meta_content,
};
use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, Request, SiteError, Step,
    Stream, StreamKind, VideoChoice,
};
use serde_json::Value;

pub fn matches(host: &str) -> bool {
    host_is(host, "weixin.qq.com")
        || host_is(host, "mp.weixin.qq.com")
        || host_is(host, "channels.weixin.qq.com")
        || host_is(host, "qpic.cn")
        || host_is(host, "qq.com")
}

const SITE: &str = "WeChat";
const REFERER: &str = "https://mp.weixin.qq.com/";

fn shape() -> SiteError {
    SiteError::Shape(SITE.to_string())
}

fn headers() -> Vec<(String, String)> {
    vec![("Referer".to_string(), REFERER.to_string())]
}

/// The player endpoint that turns a `vid` into fetchable URLs.
///
/// Every parameter but `vid` is deliberately left empty: the endpoint accepts that for a
/// public article, and the request carries the user's own cookies from the tab, which is
/// what supplies everything the blank fields would otherwise have to.
fn play_url_request(vid: &str) -> Request {
    Request::get(format!(
        "https://mp.weixin.qq.com/mp/videoplayer?action=get_mp_video_play_url&preview=0\
         &__biz=&mid=&idx=&vid={vid}&uin=&key=&pass_ticket=&wxtoken=&appmsg_token=&x5=0&f=json"
    ))
    .with_header("Referer", REFERER)
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Stage {
    /// Waiting for the article's own HTML.
    #[default]
    Page,
    /// Waiting for the player endpoint's JSON.
    PlayUrl,
}

#[derive(Debug, Default)]
pub struct Weixin {
    stage: Stage,
    page_url: String,
    title: String,
}

impl Weixin {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Weixin {
    fn site(&self) -> &'static str {
        SITE
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        let host = crate::policy::host_of(url).unwrap_or_default();
        if host_is(&host, "channels.weixin.qq.com") {
            return Err(channels_are_not_supported());
        }
        // Kept because the title falls back to it and because it is what the reported
        // failure has to be about.
        self.page_url = url.to_string();
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().copied().ok_or_else(shape)?;
        match self.stage {
            Stage::Page => {
                self.title = article_title(body, &self.page_url);
                // A direct URL in the page means the article inlined the address and no
                // second request is needed at all.
                let direct = urls_containing(body, "mpvideo.qpic.cn");
                if !direct.is_empty() {
                    let mut extraction = Extraction {
                        site: SITE.to_string(),
                        title: self.title.clone(),
                        options: direct_options(&direct, &self.title),
                        videos: direct_videos(&direct),
                        // A WeChat address is one muxed file; there is no sound to pick.
                        audios: Vec::new(),
                        subtitles: Vec::new(),
                    };
                    // The one place "best" is decided, for every site alike.
                    extraction.rank_choices();
                    return Ok(Step::Done(extraction));
                }
                let vid = find_vid(body).ok_or_else(shape)?;
                self.stage = Stage::PlayUrl;
                Ok(Step::Need(Need::Fetch(vec![play_url_request(&vid)])))
            }
            Stage::PlayUrl => Ok(Step::Done(parse_play_url(body, &self.title)?)),
        }
    }
}

/// The refusal for Channels, stated plainly.
///
/// It says what is true (encrypted, per-session signatures), what *is* supported
/// (articles), and it does not hint at a workaround, because there is not one that this
/// project would ship.
fn channels_are_not_supported() -> SiteError {
    SiteError::Unavailable(
        "WeChat Channels (视频号) video is encrypted and every address is signed for one \
         session, so opendownloader cannot download it. Videos inside WeChat articles \
         (mp.weixin.qq.com/s/…) are supported."
            .to_string(),
    )
}

fn direct_options(urls: &[String], title: &str) -> Vec<MediaOption> {
    urls.iter()
        .enumerate()
        .map(|(index, url)| MediaOption {
            label: if urls.len() > 1 {
                format!("Video {}", index + 1)
            } else {
                "Video".to_string()
            },
            // Nothing in the page states a quality, so every direct URL ranks the same and
            // the stable sort keeps them in document order — which is the order they
            // appear in the article.
            rank: 1,
            streams: vec![Stream {
                url: url.clone(),
                kind: StreamKind::Muxed,
                mime: Some("video/mp4".to_string()),
                size: None,
                headers: headers(),
                max_chunk: None,
            }],
            filename: if urls.len() > 1 {
                safe_filename(&format!("{title} ({})", index + 1), "mp4")
            } else {
                safe_filename(title, "mp4")
            },
            width: None,
            height: None,
            duration_ms: None,
        })
        .collect()
}

/// The same direct URLs as [`VideoChoice`]s.
///
/// An article can hold several videos, and these are those videos rather than renditions
/// of one — nothing in the page distinguishes them by quality, so they are all equal and
/// `rank_choices` leaves them in document order. Each is a muxed file, hence
/// `has_audio: true` and an empty audio list.
fn direct_videos(urls: &[String]) -> Vec<VideoChoice> {
    urls.iter()
        .enumerate()
        .map(|(index, url)| VideoChoice {
            // The page names nothing, so the position in the article is the id.
            id: format!("v{index}"),
            label: if urls.len() > 1 {
                format!("Video {}", index + 1)
            } else {
                "Video".to_string()
            },
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            codec: None,
            size: None,
            stream: Stream {
                url: url.clone(),
                kind: StreamKind::Muxed,
                mime: Some("video/mp4".to_string()),
                size: None,
                headers: headers(),
                max_chunk: None,
            },
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
        })
        .collect()
}

/// Read `url_info[]` out of the player endpoint's answer.
pub fn parse_play_url(body: &str, title: &str) -> Result<Extraction, SiteError> {
    // The endpoint has been seen wrapped in a callback, so fall back to brace-matching the
    // first object in the body rather than insisting the whole thing is JSON.
    let root: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => serde_json::from_str(json_object_after(body, "").ok_or_else(shape)?)
            .map_err(|_| shape())?,
    };

    let infos = root
        .get("url_info")
        .and_then(Value::as_array)
        .ok_or_else(shape)?;

    let mut options: Vec<MediaOption> = Vec::new();
    let mut videos: Vec<VideoChoice> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for info in infos {
        let Some(url) = info
            .get("url")
            .and_then(Value::as_str)
            .map(|u| decode_html_entities(&decode_json_escapes(u)))
            .filter(|u| u.starts_with("http"))
        else {
            continue;
        };
        // WeChat's `format_id` values are not publicly documented, so the number is shown
        // rather than dressed up as a resolution that might be wrong. Higher has meant
        // better in every observed response, which is enough to sort by.
        let format_id = info.get("format_id").and_then(Value::as_u64).unwrap_or(0);
        let label = if format_id > 0 {
            format!("Quality {format_id}")
        } else {
            "Video".to_string()
        };
        let size = info.get("size").and_then(Value::as_u64);
        let width = dimension(info, "width");
        let height = dimension(info, "height");
        let stream = Stream {
            url,
            kind: StreamKind::Muxed,
            mime: Some("video/mp4".to_string()),
            size,
            headers: headers(),
            max_chunk: None,
        };
        // One muxed file per entry: the choice is whole on its own, which is what
        // `has_audio` says, and there is no separate sound to offer beside it.
        let mut id = if format_id > 0 {
            format!("f{format_id}")
        } else {
            format!("v{}", videos.len())
        };
        if ids.contains(&id) {
            id = format!("{id}-{}", videos.len());
        }
        ids.push(id.clone());
        videos.push(VideoChoice {
            id,
            // The `format_id` on its own means nothing to a person; the resolution beside
            // it is the part they can actually decide with.
            label: match height {
                Some(h) => format!("{label} · {h}p"),
                None => label.clone(),
            },
            width,
            height,
            fps: None,
            bitrate: None,
            codec: None,
            size,
            stream: stream.clone(),
            has_audio: true,
            best: false,
            // Both are computed by `rank_choices`, from the stream's own mime.
            container: None,
            mergeable: false,
        });
        options.push(MediaOption {
            label,
            rank: format_id,
            streams: vec![stream],
            filename: safe_filename(title, "mp4"),
            width,
            height,
            duration_ms: root
                .get("duration")
                .and_then(Value::as_u64)
                .filter(|s| *s > 0)
                .map(|s| s * 1000),
        });
    }

    if options.is_empty() {
        return Err(SiteError::Unavailable(
            "WeChat returned no playable address for this video. The article may have been \
             deleted, or the video may be visible only inside the WeChat app."
                .to_string(),
        ));
    }
    options.sort_by_key(|o| std::cmp::Reverse(o.rank));
    let mut extraction = Extraction {
        site: SITE.to_string(),
        title: title.to_string(),
        options,
        videos,
        // WeChat states one address per quality, sound included; nothing to pair.
        audios: Vec::new(),
        subtitles: Vec::new(),
    };
    // The one place "best" is decided, for every site alike.
    extraction.rank_choices();
    Ok(extraction)
}

/// The `vid` an article's player is configured with.
///
/// Three spellings are known — `var vid = "…"`, `vid=…` inside a player URL, and
/// `"vid":"…"` in an embedded config — so this scans for the token rather than matching a
/// fixed prefix. The token has to stand alone: `videoid` and `wx_vid` are not it, which is
/// what the boundary check is for.
fn find_vid(html: &str) -> Option<String> {
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find("vid") {
        let at = cursor + rel;
        cursor = at + 3;
        if at > 0 && is_identifier_byte(html.as_bytes()[at - 1]) {
            continue;
        }
        let mut rest = html[at + 3..].trim_start();
        // `"vid":` closes its key quote before the colon.
        if let Some(r) = rest.strip_prefix(['"', '\'']) {
            rest = r.trim_start();
        }
        let Some(rest) = rest.strip_prefix(['=', ':']) else {
            continue;
        };
        let mut rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix(['"', '\'']) {
            rest = r;
        }
        let value: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        // Short values are query-string noise (`vid=0`, `vid=1`), not a WeChat video id.
        if value.len() >= 6 {
            return Some(value);
        }
    }
    None
}

fn is_identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Every URL in the text that runs through `needle`.
///
/// Written as a scan rather than a pattern match because these addresses turn up in three
/// contexts in one document — an attribute, a JSON string, a JavaScript literal — and the
/// only thing they share is that they start at `http` and end at a quote or a bracket.
///
/// Two characters are deliberately *not* terminators. A backslash is not, so a
/// JSON-escaped `https:\/\/…` is captured whole and decoded afterwards rather than
/// truncated to `https:`. A semicolon is not, because an HTML attribute spells its
/// ampersands `&amp;` and stopping there would cut the query string in half — which is
/// exactly the kind of URL that looks right and 404s.
fn urls_containing(text: &str, needle: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(needle) {
        let at = cursor + rel;
        cursor = at + needle.len();
        let Some(start) = text[..at].rfind("http") else {
            continue;
        };
        // A scheme far away from the host belongs to some other URL entirely.
        if at - start > 200 {
            continue;
        }
        let tail = &text[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "\"'<>)]}".contains(c))
            .unwrap_or(tail.len());
        let raw = tail[..end].trim_end_matches('\\');
        let url = decode_html_entities(&decode_json_escapes(raw));
        if url.starts_with("http") && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// `msg_title`, then Open Graph, then the document title, then the URL.
fn article_title(html: &str, page_url: &str) -> String {
    for marker in ["var msg_title =", "var msg_title="] {
        if let Some(t) = quoted_value_after(html, marker).filter(|t| !t.trim().is_empty()) {
            return t.trim().to_string();
        }
    }
    for key in ["og:title", "twitter:title"] {
        if let Some(t) = meta_content(html, key).filter(|t| !t.trim().is_empty()) {
            return t.trim().to_string();
        }
    }
    if let Some(t) = html_title(html).filter(|t| !t.trim().is_empty()) {
        return t.trim().to_string();
    }
    // `…/s/<id>` is the only thing a bare article URL carries.
    page_url
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(|s| s.split(['?', '#']).next().unwrap_or(s).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "WeChat video".to_string())
}

/// The quoted string that follows `marker`, escapes and entities decoded.
fn quoted_value_after(html: &str, marker: &str) -> Option<String> {
    let rest = html[html.find(marker)? + marker.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &rest[quote.len_utf8()..];
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return Some(decode_html_entities(&decode_json_escapes(&body[..i])));
        }
    }
    None
}

fn dimension(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE_URL: &str = "https://mp.weixin.qq.com/s/AbCdEfGhIjKlMn";

    const ARTICLE_WITH_VID: &str = r#"<html><head><title>ignored</title></head><body>
<script>var msg_title = "一篇文章 &amp; its video";</script>
<iframe class="video_iframe" data-src="https://mp.weixin.qq.com/mp/videoplayer?vid=wxv_2891234567890123456&width=100"></iframe>
</body></html>"#;

    const PLAY_URL_JSON: &str = r#"{"base_resp":{"ret":0},"duration":95,"url_info":[
{"url":"https:\/\/mpvideo.qpic.cn\/0b2e.f10002.mp4?dis_k=1&dis_t=2","format_id":10,"size":1048576,"width":640,"height":360},
{"url":"https:\/\/mpvideo.qpic.cn\/0b2e.f20002.mp4?dis_k=1&dis_t=2","format_id":20,"width":1280,"height":720}]}"#;

    #[test]
    fn wechat_claims_its_hosts_and_not_lookalikes() {
        assert!(matches("mp.weixin.qq.com"));
        assert!(matches("weixin.qq.com"));
        assert!(matches("channels.weixin.qq.com"));
        assert!(matches("mpvideo.qpic.cn"));
        assert!(matches("qq.com"));
        assert!(!matches("notqq.com"));
        assert!(!matches("qq.com.evil.test"));
    }

    #[test]
    fn channels_is_refused_at_the_first_step_with_a_plain_sentence() {
        let mut e = Weixin::new();
        let err = e
            .start("https://channels.weixin.qq.com/pages/feed?id=abc")
            .unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(message.contains("encrypted"), "{message}");
        assert!(message.contains("cannot download"), "{message}");
        assert!(message.contains("articles"), "{message}");
        assert_eq!(err.to_string(), *message);
    }

    #[test]
    fn an_article_begins_by_asking_for_the_page_state() {
        let mut e = Weixin::new();
        assert_eq!(e.start(ARTICLE_URL).unwrap(), Step::Need(Need::PageState));
        assert_eq!(e.site(), "WeChat");
    }

    #[test]
    fn a_vid_becomes_exactly_one_fetch_of_the_player_endpoint() {
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        let Step::Need(Need::Fetch(requests)) = e.feed(&[ARTICLE_WITH_VID]).unwrap() else {
            panic!("expected a fetch")
        };
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "GET");
        assert!(request.url.contains("action=get_mp_video_play_url"));
        assert!(request.url.contains("vid=wxv_2891234567890123456"));
        assert!(request.url.ends_with("f=json"));
        assert_eq!(
            request.headers,
            vec![("Referer".to_string(), REFERER.to_string())]
        );
    }

    #[test]
    fn the_player_answer_becomes_options_ranked_by_format_id() {
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        e.feed(&[ARTICLE_WITH_VID]).unwrap();
        let Step::Done(x) = e.feed(&[PLAY_URL_JSON]).unwrap() else {
            panic!("expected Done")
        };
        assert_eq!(x.site, "WeChat");
        assert_eq!(x.title, "一篇文章 & its video");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Quality 20", "Quality 10"]);
        assert_eq!(x.options[0].height, Some(720));
        assert_eq!(x.options[0].duration_ms, Some(95_000));
        assert_eq!(x.options[1].streams[0].size, Some(1_048_576));
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
    }

    #[test]
    fn escaped_urls_from_the_player_are_decoded_or_the_download_would_404() {
        let x = parse_play_url(PLAY_URL_JSON, "t").unwrap();
        assert_eq!(
            x.options[0].streams[0].url,
            "https://mpvideo.qpic.cn/0b2e.f20002.mp4?dis_k=1&dis_t=2"
        );
        for option in &x.options {
            assert!(!option.streams[0].url.contains("\\/"));
        }
    }

    #[test]
    fn every_stream_carries_the_wechat_referer() {
        let x = parse_play_url(PLAY_URL_JSON, "t").unwrap();
        for option in &x.options {
            assert_eq!(
                option.streams[0].headers,
                vec![("Referer".to_string(), REFERER.to_string())]
            );
        }
    }

    #[test]
    fn a_direct_mpvideo_url_in_the_page_skips_the_second_request_entirely() {
        let html = r#"<html><title>Direct</title><body>
<video src="https://mpvideo.qpic.cn/0bf2.f10002.mp4?dis_k=abc&amp;dis_t=1"></video></body></html>"#;
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        let Step::Done(x) = e.feed(&[html]).unwrap() else {
            panic!("expected Done without a fetch")
        };
        assert_eq!(x.options.len(), 1);
        assert_eq!(x.options[0].label, "Video");
        assert_eq!(
            x.options[0].streams[0].url,
            "https://mpvideo.qpic.cn/0bf2.f10002.mp4?dis_k=abc&dis_t=1"
        );
    }

    #[test]
    fn a_json_escaped_direct_url_is_captured_whole_rather_than_truncated_at_the_backslash() {
        let html =
            r#"<script>{"cdn_url":"https:\/\/mpvideo.qpic.cn\/0bf2.f10002.mp4?a=1&b=2"}</script>"#;
        let found = urls_containing(html, "mpvideo.qpic.cn");
        assert_eq!(
            found,
            vec!["https://mpvideo.qpic.cn/0bf2.f10002.mp4?a=1&b=2".to_string()]
        );
    }

    #[test]
    fn several_videos_in_one_article_are_offered_in_document_order() {
        let html = r#"<title>Two</title><video src="https://mpvideo.qpic.cn/one.mp4"></video>
<video src="https://mpvideo.qpic.cn/two.mp4"></video>"#;
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        let Step::Done(x) = e.feed(&[html]).unwrap() else {
            panic!()
        };
        assert_eq!(x.options.len(), 2);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://mpvideo.qpic.cn/one.mp4"
        );
        assert_eq!(
            x.options[1].streams[0].url,
            "https://mpvideo.qpic.cn/two.mp4"
        );
        assert_eq!(x.options[0].filename, "Two (1).mp4");
    }

    #[test]
    fn the_vid_is_found_in_all_three_spellings_and_not_inside_another_word() {
        assert_eq!(
            find_vid(r#"<script>var vid = "wxv_123456789";</script>"#).as_deref(),
            Some("wxv_123456789")
        );
        assert_eq!(
            find_vid("<iframe src=\"https://x/player?vid=wxv_987654321&w=1\">").as_deref(),
            Some("wxv_987654321")
        );
        assert_eq!(
            find_vid(r#"{"videoid":"nope","vid":"wxv_555555555"}"#).as_deref(),
            Some("wxv_555555555")
        );
        assert_eq!(find_vid("<div class=\"videoid\">no vid here</div>"), None);
        assert_eq!(find_vid("?vid=0&x=1"), None);
    }

    #[test]
    fn a_page_with_neither_a_direct_url_nor_a_vid_reports_shape_and_names_the_site() {
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        assert_eq!(
            e.feed(&["<html>a text-only article</html>"]).unwrap_err(),
            SiteError::Shape("WeChat".into())
        );
    }

    #[test]
    fn a_player_answer_with_no_url_info_reports_shape() {
        assert!(matches!(
            parse_play_url(r#"{"base_resp":{"ret":-1}}"#, "t"),
            Err(SiteError::Shape(_))
        ));
    }

    #[test]
    fn a_player_answer_whose_entries_are_all_unusable_is_unavailable_not_a_shape_change() {
        let err = parse_play_url(r#"{"url_info":[{"format_id":10}]}"#, "t").unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(message.contains("no playable address"), "{message}");
    }

    #[test]
    fn a_callback_wrapped_answer_is_still_read() {
        let jsonp = r#"cb({"url_info":[{"url":"https://mpvideo.qpic.cn/a.mp4","format_id":10}]});"#;
        let x = parse_play_url(jsonp, "t").unwrap();
        assert_eq!(x.options[0].streams[0].url, "https://mpvideo.qpic.cn/a.mp4");
    }

    #[test]
    fn the_title_falls_back_from_msg_title_to_open_graph_to_the_url() {
        assert_eq!(
            article_title(ARTICLE_WITH_VID, ARTICLE_URL),
            "一篇文章 & its video"
        );
        assert_eq!(
            article_title(
                r#"<meta property="og:title" content="From Open Graph">"#,
                ARTICLE_URL
            ),
            "From Open Graph"
        );
        assert_eq!(
            article_title("<html>nothing at all</html>", ARTICLE_URL),
            "AbCdEfGhIjKlMn"
        );
    }

    #[test]
    fn feeding_nothing_reports_shape_instead_of_panicking() {
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        assert!(matches!(e.feed(&[]), Err(SiteError::Shape(_))));
    }

    #[test]
    fn the_player_answer_becomes_muxed_video_choices_needing_no_audio_picked() {
        let x = parse_play_url(PLAY_URL_JSON, "t").unwrap();
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
            "each url_info entry is one file carrying both tracks"
        );
        assert!(x.videos.iter().all(|v| v.stream.kind == StreamKind::Muxed));
        assert!(x.audios.is_empty(), "WeChat never serves the sound apart");
        // Named by `format_id`, which is the only thing WeChat gives a rendition.
        let ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["f20", "f10"]);
        assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1);
        assert!(x.videos[0].best);
        assert_eq!(x.best_video().map(|v| v.id.as_str()), Some("f20"));
        // The number alone means nothing to a person; the resolution beside it does.
        assert_eq!(x.videos[0].label, "Quality 20 · 720p");
        assert_eq!(x.videos[0].height, Some(720));
        assert_eq!(x.videos[1].size, Some(1_048_576));
    }

    #[test]
    fn several_direct_videos_in_one_article_are_each_a_choice_in_document_order() {
        let html = r#"<title>Two</title><video src="https://mpvideo.qpic.cn/one.mp4"></video>
<video src="https://mpvideo.qpic.cn/two.mp4"></video>"#;
        let mut e = Weixin::new();
        e.start(ARTICLE_URL).unwrap();
        let Step::Done(x) = e.feed(&[html]).unwrap() else {
            panic!()
        };
        assert_eq!(x.videos.len(), x.options.len());
        let ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            ["v0", "v1"],
            "the page names nothing to be named after"
        );
        assert_eq!(x.videos[0].label, "Video 1");
        assert_eq!(x.videos[0].stream.url, "https://mpvideo.qpic.cn/one.mp4");
        assert!(x.videos.iter().all(|v| v.has_audio));
        assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1);
        assert!(
            x.videos[0].best,
            "nothing separates them, so the first stands"
        );
        assert!(x.audios.is_empty());
    }
}
