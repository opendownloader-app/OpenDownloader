//! The last-resort extractor, for pages that simply state their media.
//!
//! Vimeo, Twitch, Reddit, X, Weibo, Bluesky and the rest do not share an API, but they
//! do share a habit: the media URL is written into the page, in an Open Graph header, a
//! JSON-LD block, or a plain `<video>` tag, because that is how link previews and
//! browsers without their player work. Reading what is already in the loaded document
//! costs one message to the tab and no request the site can refuse, which is why this
//! asks for [`Need::PageState`] rather than fetching anything.
//!
//! It is deliberately dumb. It claims no knowledge of any site's internals, so it cannot
//! rot the way a hand-written API parser does; when a site changes its player entirely,
//! this either still finds the `<video>` tag or returns [`SiteError::Shape`] and the
//! passive sniffer takes over. Registered last for exactly that reason.

use serde_json::Value;

use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, SiteError, Step, Stream,
    StreamKind, VideoChoice,
};
use crate::hls::resolve_url;

/// Sites whose pages state their media plainly enough for this to work.
/// Hosts whose pages simply state their media.
///
/// The bar for being on this list is low on purpose: a site belongs here if its page
/// carries an `og:video` or a `<video src>` that is a real file, which is true of a
/// surprising number of large sites and of nearly every small one. Where a site needs an
/// API call instead — Vimeo, Dailymotion, Twitch, X — it gets its own extractor and is
/// claimed there first, because the registry tries the dedicated ones before this.
///
/// A host missing from this list is not unsupported, only unclaimed: the passive sniffer
/// still sees whatever the page fetches. Being here adds the title, the rendition list
/// and a filename worth reading.
const CLAIMED: &[&str] = &[
    // Social and short video.
    "reddit.com",
    "redd.it",
    "weibo.com",
    "kuaishou.com",
    "xiaohongshu.com",
    "bsky.app",
    "threads.net",
    "threads.com",
    "tumblr.com",
    "pinterest.com",
    "pin.it",
    "linkedin.com",
    "snapchat.com",
    "imgur.com",
    "9gag.com",
    "coub.com",
    // Video hosts and communities.
    "streamable.com",
    "rumble.com",
    "odysee.com",
    "bitchute.com",
    "nicovideo.jp",
    "vk.com",
    "vkvideo.ru",
    "ok.ru",
    "ted.com",
    "archive.org",
    "veoh.com",
    // Chinese platforms whose free content states itself in the page.
    "ixigua.com",
    "huya.com",
    "douyu.com",
    "acfun.cn",
    "miaopai.com",
    // Audio, which the same page reading handles unchanged.
    "soundcloud.com",
    "mixcloud.com",
    "bandcamp.com",
    // News and broadcast, where an article's video is usually stated outright.
    "bbc.co.uk",
    "bbc.com",
    "cnn.com",
    "espn.com",
    "nbcnews.com",
    "theguardian.com",
];

pub fn matches(host: &str) -> bool {
    CLAIMED.iter().any(|d| host_is(host, d))
}

#[derive(Debug, Default)]
pub struct Generic {
    /// The page's own URL, kept because half of what turns up in these documents is a
    /// relative or scheme-relative reference that means nothing without it.
    page_url: String,
}

impl Generic {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Generic {
    fn site(&self) -> &'static str {
        "video page"
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        self.page_url = url.to_string();
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let html = bodies.first().copied().ok_or_else(shape)?;
        let title = page_title(html, &self.page_url);
        let urls = media_urls(html, &self.page_url);
        if urls.is_empty() {
            return Err(shape());
        }

        let count = urls.len() as u64;
        let mut options: Vec<MediaOption> = Vec::new();
        let mut videos: Vec<VideoChoice> = Vec::new();
        for (i, url) in urls.into_iter().enumerate() {
            let ext = extension_of(&url);
            let ext = ext.as_deref();
            let stream = Stream {
                mime: mime_for(ext).map(str::to_string),
                // Several of these hosts serve media only to their own pages.
                // The Referer costs nothing when it is not needed and is the
                // whole difference between 200 and 403 when it is.
                headers: vec![("Referer".into(), self.page_url.clone())],
                max_chunk: None,
                url,
                kind: StreamKind::Muxed,
                size: None,
            };
            // A page that states its media states a whole file, sound included, so every
            // choice here answers the question on its own — which is what `has_audio`
            // says, and why the audio list below stays empty.
            videos.push(VideoChoice {
                // Nothing on these pages names a rendition and nothing states a size, so
                // the position — which is the confidence order the URLs were found in —
                // is the only id there is to give.
                id: format!("v{i}"),
                label: label_for(ext),
                width: None,
                height: None,
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
                label: label_for(ext),
                // Preference order is the only ranking available: nothing here states
                // a resolution, so "the source we trust most" has to stand in for
                // "the best picture".
                rank: count - i as u64,
                streams: vec![stream],
                filename: safe_filename(&title, file_extension(ext)),
                width: None,
                height: None,
                duration_ms: None,
            });
        }

        let mut extraction = Extraction {
            note: None,
            site: "video page".into(),
            title,
            options,
            videos,
            // Nothing here separates picture from sound, so a UI shows no audio picker.
            audios: Vec::new(),
            subtitles: Vec::new(),
        };
        // `rank_choices` states nothing about these beyond flagging the first, since none
        // of them declares a resolution or a bitrate to be sorted by — and its sort is
        // stable, so the page's own confidence order survives it.
        extraction.rank_choices();
        Ok(Step::Done(extraction))
    }
}

fn shape() -> SiteError {
    SiteError::Shape("generic".into())
}

/// Everything worth downloading this page mentions, best source first, deduped.
///
/// The order is a confidence ranking, not an accident: `og:video:secure_url` is what the
/// site tells other services to play, a `<video>` tag is what happened to be in the DOM
/// when the page loaded, and a page often contains both — the same file, plus an advert
/// or a preview clip.
fn media_urls(html: &str, page_url: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("data:") || raw.starts_with("blob:") {
            return;
        }
        let absolute = resolve_url(page_url, raw);
        if !found.contains(&absolute) {
            found.push(absolute);
        }
    };

    // `og:video` is a promise that *something* is playable there, not that it is a media
    // file. Vimeo, YouTube and most embed-friendly sites point it at their own player
    // page and say so in `og:video:type: text/html`. Offering that as a download hands
    // the user an HTML page named `.mp4`, so the declared type is honoured: when the page
    // says its og:video is a document, the og: family is skipped and the `<video>` tag —
    // which does name a real file — is used instead.
    let og_is_embed = meta_content(html, "og:video:type")
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/html"));
    if !og_is_embed {
        for key in ["og:video:secure_url", "og:video:url", "og:video"] {
            if let Some(v) = meta_content(html, key) {
                push(&v);
            }
        }
    }
    if let Some(v) = meta_content(html, "twitter:player:stream") {
        push(&v);
    }
    for v in json_ld_content_urls(html) {
        push(&v);
    }
    for v in video_tag_sources(html) {
        push(&v);
    }
    found
}

/// og:title, then the document title, then whatever the URL's last segment is.
///
/// The last fallback is poor but it is never empty, and an option with no filename is
/// worse than one named after a slug.
fn page_title(html: &str, page_url: &str) -> String {
    if let Some(t) = meta_content(html, "og:title") {
        return t;
    }
    if let Some(t) = element_text(html, "title") {
        let t = decode_entities(t);
        if !t.trim().is_empty() {
            return t.trim().to_string();
        }
    }
    let path = page_url.split(['?', '#']).next().unwrap_or(page_url);
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("video")
        .to_string()
}

/// A `<meta>` whose `property` — or `name`, which plenty of pages use for Open Graph
/// even though the spec says otherwise — is `key`.
fn meta_content(html: &str, key: &str) -> Option<String> {
    start_tags(html, "meta").into_iter().find_map(|tag| {
        let attrs = attributes(tag);
        let get = |n: &str| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
        let named = ["property", "name", "itemprop"]
            .iter()
            .filter_map(|a| get(a))
            .any(|v| v.eq_ignore_ascii_case(key));
        named
            .then(|| get("content"))
            .flatten()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

/// `contentUrl` from every JSON-LD `VideoObject` in the document.
///
/// The walk is recursive because these blocks routinely wrap the interesting object in
/// an `@graph`, an array, or a `mainEntity`, and the nesting differs per site.
fn json_ld_content_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in script_bodies(html, "ld+json") {
        let Ok(value) = serde_json::from_str::<Value>(block.trim()) else {
            continue;
        };
        collect_video_objects(&value, &mut out);
    }
    out
}

fn collect_video_objects(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| collect_video_objects(v, out)),
        Value::Object(map) => {
            if is_video_object(map.get("@type")) {
                if let Some(url) = map.get("contentUrl").and_then(Value::as_str) {
                    out.push(url.to_string());
                }
            }
            map.values().for_each(|v| collect_video_objects(v, out));
        }
        _ => {}
    }
}

/// `@type` is a string on most pages and an array on a few.
fn is_video_object(ty: Option<&Value>) -> bool {
    match ty {
        Some(Value::String(s)) => s.eq_ignore_ascii_case("VideoObject"),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .any(|s| s.eq_ignore_ascii_case("VideoObject")),
        _ => false,
    }
}

/// `<video src>` and every `<source src>` inside a `<video>` element.
fn video_tag_sources(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<video") {
        let start = cursor + offset;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        if let Some(src) = attribute(&html[start..open_end - 1], "src") {
            out.push(src);
        }
        let inner_len = lower[open_end..]
            .find("</video")
            .unwrap_or(lower.len() - open_end);
        for tag in start_tags(&html[open_end..open_end + inner_len], "source") {
            if let Some(src) = attribute(tag, "src") {
                out.push(src);
            }
        }
        cursor = open_end + inner_len;
    }
    out
}

/// The text of the first `<name>…</name>` element.
fn element_text<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find(&format!("<{name}"))?;
    let gt = lower[open..].find('>')? + open + 1;
    let close = lower[gt..].find(&format!("</{name}"))? + gt;
    Some(&html[gt..close])
}

/// The bodies of `<script>` elements whose `type` mentions `kind`.
fn script_bodies<'a>(html: &'a str, kind: &str) -> Vec<&'a str> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<script") {
        let start = cursor + offset;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        let body_len = lower[open_end..]
            .find("</script")
            .unwrap_or(lower.len() - open_end);
        let is_wanted = attribute(&html[start..open_end - 1], "type")
            .is_some_and(|t| t.to_ascii_lowercase().contains(kind));
        if is_wanted {
            out.push(&html[open_end..open_end + body_len]);
        }
        cursor = open_end + body_len;
    }
    out
}

/// Every `<name …>` start tag in the document, as raw text without the angle brackets.
///
/// Hand-rolled because pulling an HTML parser into a wasm bundle to read four attributes
/// is not a trade worth making; the shapes this has to survive are meta tags and video
/// tags, both of which are flat.
fn start_tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("<{name}");
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find(&needle) {
        let start = cursor + offset;
        let after_name = start + needle.len();
        // `<sourceset` is not `<source`: the name has to end where the needle does.
        let is_tag = lower
            .as_bytes()
            .get(after_name)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/' || *b == b'\n');
        let Some(gt) = lower[after_name..].find('>') else {
            break;
        };
        let end = after_name + gt;
        if is_tag {
            out.push(&html[start..end]);
        }
        cursor = end + 1;
    }
    out
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    attributes(tag)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.trim().is_empty())
}

/// Split a start tag into lowercase attribute names and entity-decoded values.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let bytes = tag.as_bytes();
    let mut i = 0;
    // Skip `<name`.
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut out = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            match bytes.get(i) {
                Some(&quote @ (b'"' | b'\'')) => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    value = decode_entities(&tag[start..i]);
                    i += 1;
                }
                Some(_) => {
                    let start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    value = decode_entities(&tag[start..i]);
                }
                None => {}
            }
        }
        out.push((name, value));
    }
    out
}

/// Decode the handful of entities that actually appear in these attributes.
///
/// `&amp;` is the one that matters: an og:video URL is entity-escaped as a matter of
/// course, and a query string fetched with a literal `&amp;` in it comes back a 403 or a
/// 404 rather than a video. The rest are here because they cost one match arm each.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = [
            ("&amp;", "&"),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&#x27;", "'"),
            ("&apos;", "'"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&#x2F;", "/"),
            ("&#47;", "/"),
        ]
        .into_iter()
        .find(|(entity, _)| tail.starts_with(entity));
        match decoded {
            Some((entity, replacement)) => {
                out.push_str(replacement);
                rest = &tail[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The lowercase extension of a URL's path, ignoring query and fragment.
fn extension_of(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| ext.to_ascii_lowercase())
}

fn label_for(ext: Option<&str>) -> String {
    match ext {
        Some("mp4") | Some("m4v") => "MP4",
        Some("webm") => "WebM",
        Some("m3u8") => "HLS stream",
        Some("mpd") => "DASH stream",
        Some("mov") => "MOV",
        Some("mkv") => "MKV",
        Some("ogv") | Some("ogg") => "Ogg",
        Some("mp3") => "MP3",
        Some("m4a") => "M4A",
        _ => "Video",
    }
    .to_string()
}

/// What lands on disk. A playlist is not a file the user wants, the fMP4 it describes is,
/// which is the same rule [`crate::classify`] applies to sniffed playlists.
fn file_extension(ext: Option<&str>) -> &'static str {
    match ext {
        Some("webm") => "webm",
        Some("mov") => "mov",
        Some("mkv") => "mkv",
        Some("ogv") | Some("ogg") => "ogg",
        Some("mp3") => "mp3",
        Some("m4a") => "m4a",
        _ => "mp4",
    }
}

fn mime_for(ext: Option<&str>) -> Option<&'static str> {
    Some(match ext? {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "m3u8" => "application/vnd.apple.mpegurl",
        "mpd" => "application/dash+xml",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "ogv" | "ogg" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_URL: &str = "https://vimeo.com/channels/staffpicks/76979871";

    fn extract(html: &str) -> Result<Extraction, SiteError> {
        extract_from(PAGE_URL, html)
    }

    fn extract_from(url: &str, html: &str) -> Result<Extraction, SiteError> {
        let mut g = Generic::new();
        assert_eq!(g.start(url).unwrap(), Step::Need(Need::PageState));
        match g.feed(&[html])? {
            Step::Done(e) => Ok(e),
            Step::Need(_) => panic!("the generic extractor asked for a second input"),
        }
    }

    fn urls(e: &Extraction) -> Vec<&str> {
        e.options
            .iter()
            .map(|o| o.streams[0].url.as_str())
            .collect()
    }

    #[test]
    fn an_og_video_declared_as_html_is_an_embed_and_is_not_offered() {
        // Vimeo's real page shape, checked live: og:video points at the player rather
        // than at a file, and the page says so in og:video:type. Offering that hands the
        // user an HTML document named `.mp4`.
        let html = r#"<html><head>
            <meta property="og:title" content="A film">
            <meta property="og:video:secure_url" content="https://player.example/video/1">
            <meta property="og:video:type" content="text/html">
        </head><body>
            <video src="https://cdn.example/real.mp4"></video>
        </body></html>"#;
        let e = extract(html).expect("the video tag still names a real file");
        assert_eq!(urls(&e), vec!["https://cdn.example/real.mp4"]);
    }

    #[test]
    fn an_embed_only_page_reports_that_nothing_was_found_rather_than_offering_the_player() {
        let html = r#"<html><head>
            <meta property="og:video" content="https://player.example/video/1">
            <meta property="og:video:type" content="text/html">
        </head><body></body></html>"#;
        assert!(matches!(extract(html), Err(SiteError::Shape(_))));
    }

    #[test]
    fn an_og_video_with_no_declared_type_is_still_trusted() {
        // Most sites omit og:video:type entirely, and for those the URL is usually the
        // file. Only an explicit `text/html` is treated as a refusal.
        let html = r#"<html><head>
            <meta property="og:video" content="https://cdn.example/clip.mp4">
        </head><body></body></html>"#;
        let e = extract(html).expect("an untyped og:video is a media URL");
        assert_eq!(urls(&e), vec!["https://cdn.example/clip.mp4"]);
    }

    #[test]
    fn it_claims_the_long_tail_of_video_pages_and_nothing_else() {
        for host in [
            "old.reddit.com",
            "v.redd.it",
            "streamable.com",
            "bsky.app",
            "www.weibo.com",
            "www.tumblr.com",
            "rumble.com",
            "www.nicovideo.jp",
            "soundcloud.com",
            "www.bbc.co.uk",
            "vk.com",
            "www.pinterest.com",
        ] {
            assert!(matches(host), "{host} should be claimed");
        }

        // Vimeo, Dailymotion, Twitch and X are deliberately *not* here: each needs an API
        // call rather than a page read, each has its own extractor, and the registry
        // reaches those first. Claiming them here too would be a second, worse answer to
        // a question already answered.
        for host in ["vimeo.com", "www.dailymotion.com", "www.twitch.tv", "x.com"] {
            assert!(!matches(host), "{host} has a dedicated extractor");
        }

        assert!(!matches("notreddit.com"));
        assert!(!matches("reddit.com.evil.test"));
        assert!(!matches("example.com"));
    }

    #[test]
    fn open_graph_video_is_the_first_thing_looked_at() {
        let html = r#"<html><head>
            <meta property="og:title" content="A Staff Pick">
            <meta property="og:video" content="https://cdn.example/low.mp4">
            <meta property="og:video:url" content="https://cdn.example/mid.mp4">
            <meta property="og:video:secure_url" content="https://cdn.example/best.mp4">
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(e.site, "video page");
        assert_eq!(e.title, "A Staff Pick");
        assert_eq!(
            urls(&e),
            vec![
                "https://cdn.example/best.mp4",
                "https://cdn.example/mid.mp4",
                "https://cdn.example/low.mp4"
            ]
        );
        assert_eq!(e.options[0].label, "MP4");
        assert_eq!(e.options[0].rank, 3);
        assert!(e.options[0].rank > e.options[1].rank);
        assert_eq!(e.options[0].filename, "A Staff Pick.mp4");
        assert_eq!(e.options[0].streams[0].kind, StreamKind::Muxed);
    }

    #[test]
    fn the_twitter_player_stream_is_read_when_open_graph_is_absent() {
        let html = r#"<html><head>
            <meta name="twitter:player:stream" content="https://video.twimg.com/x.mp4">
            <meta name="twitter:card" content="player">
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(urls(&e), vec!["https://video.twimg.com/x.mp4"]);
    }

    #[test]
    fn a_json_ld_video_object_is_found_however_deeply_it_is_wrapped() {
        let html = r#"<html><head>
            <script type="application/ld+json">
            {"@context":"https://schema.org","@graph":[
              {"@type":"WebPage","name":"wrapper"},
              {"@type":["VideoObject","CreativeWork"],
               "name":"Clip","contentUrl":"https://cdn.example/ld.mp4"}
            ]}
            </script>
            <script type="application/json">{"contentUrl":"https://cdn.example/ignored.mp4"}</script>
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(urls(&e), vec!["https://cdn.example/ld.mp4"]);
    }

    #[test]
    fn video_and_source_tags_are_both_collected() {
        let html = r#"<html><body>
            <video src="https://cdn.example/direct.webm" controls poster="p.jpg"></video>
            <video controls>
              <source src="https://cdn.example/a.mp4" type="video/mp4">
              <source src='https://cdn.example/b.webm' type='video/webm'>
            </video>
        </body></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(
            urls(&e),
            vec![
                "https://cdn.example/direct.webm",
                "https://cdn.example/a.mp4",
                "https://cdn.example/b.webm"
            ]
        );
        assert_eq!(e.options[0].label, "WebM");
        assert!(e.options[0].filename.ends_with(".webm"));
    }

    #[test]
    fn an_entity_escaped_url_is_decoded_before_it_is_offered() {
        let html = r#"<html><head>
            <meta property="og:video:secure_url"
                  content="https://cdn.example/v.mp4?sig=abc&amp;exp=123&amp;q=&quot;hd&quot;">
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(
            urls(&e),
            vec!["https://cdn.example/v.mp4?sig=abc&exp=123&q=\"hd\""]
        );
        assert!(!urls(&e)[0].contains("&amp;"), "a literal &amp; survived");
    }

    #[test]
    fn relative_and_scheme_relative_urls_are_resolved_against_the_page() {
        let html = r#"<html><body>
            <video src="/media/root.mp4"></video>
            <video src="clip.mp4"></video>
            <video src="//cdn.example/scheme.mp4"></video>
        </body></html>"#;
        let e = extract_from("https://streamable.com/e/abc123?t=4", html).unwrap();
        assert_eq!(
            urls(&e),
            vec![
                "https://streamable.com/media/root.mp4",
                "https://streamable.com/e/clip.mp4",
                "https://cdn.example/scheme.mp4"
            ]
        );
    }

    #[test]
    fn the_same_file_named_twice_is_offered_once() {
        let html = r#"<html><head>
            <meta property="og:video:secure_url" content="https://cdn.example/v.mp4">
            <meta property="og:video" content="https://cdn.example/v.mp4">
            </head><body>
            <video src="https://cdn.example/v.mp4"></video>
            <video src="/v.mp4"></video>
        </body></html>"#;
        let e = extract_from("https://cdn.example/watch", html).unwrap();
        assert_eq!(urls(&e), vec!["https://cdn.example/v.mp4"]);
        assert_eq!(e.options.len(), 1);
    }

    #[test]
    fn an_hls_playlist_is_labelled_as_a_stream_and_named_for_what_lands_on_disk() {
        let html = r#"<html><head>
            <meta property="og:title" content="Live replay">
            <meta property="og:video" content="https://cdn.example/master.m3u8">
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(e.options[0].label, "HLS stream");
        assert_eq!(e.options[0].filename, "Live replay.mp4");
        assert_eq!(
            e.options[0].streams[0].mime.as_deref(),
            Some("application/vnd.apple.mpegurl")
        );
    }

    #[test]
    fn every_stream_carries_the_pages_own_referer() {
        let html = r#"<video src="https://cdn.example/v.mp4"></video>"#;
        let e = extract(html).unwrap();
        assert_eq!(
            e.options[0].streams[0].headers,
            vec![("Referer".to_string(), PAGE_URL.to_string())]
        );
    }

    #[test]
    fn the_title_falls_back_to_the_document_and_then_to_the_url() {
        let with_title = r#"<html><head><title>Caf&#233; &amp; Bar</title></head>
            <body><video src="https://cdn.example/v.mp4"></video></body></html>"#;
        // `&#233;` is not one of the entities worth decoding; it survives intact rather
        // than being mangled, which is what matters.
        assert_eq!(extract(with_title).unwrap().title, "Caf&#233; & Bar");

        let untitled = r#"<video src="https://cdn.example/v.mp4"></video>"#;
        let e = extract_from("https://streamable.com/moo7l3?t=1", untitled).unwrap();
        assert_eq!(e.title, "moo7l3");
        assert_eq!(e.options[0].filename, "moo7l3.mp4");
    }

    #[test]
    fn a_page_stating_no_media_is_a_shape_error_not_an_empty_success() {
        let html = "<html><head><title>Nothing here</title></head><body><p>hi</p></body></html>";
        assert_eq!(
            extract(html).unwrap_err(),
            SiteError::Shape("generic".into())
        );
        assert_eq!(extract("").unwrap_err(), SiteError::Shape("generic".into()));
        assert_eq!(
            Generic::new().feed(&[]).unwrap_err(),
            SiteError::Shape("generic".into())
        );
    }

    #[test]
    fn blob_and_data_sources_are_ignored_because_they_cannot_be_fetched() {
        let html = r#"<html><body>
            <video src="blob:https://vimeo.com/9f8c-1234"></video>
            <video src="data:video/mp4;base64,AAAA"></video>
        </body></html>"#;
        assert_eq!(
            extract(html).unwrap_err(),
            SiteError::Shape("generic".into())
        );
    }

    #[test]
    fn a_lookalike_tag_name_is_not_mistaken_for_a_meta_tag() {
        let html = r#"<html><head>
            <metadata property="og:video" content="https://cdn.example/wrong.mp4"></metadata>
            <meta property="og:video" content="https://cdn.example/right.mp4">
        </head></html>"#;
        let e = extract(html).unwrap();
        assert_eq!(urls(&e), vec!["https://cdn.example/right.mp4"]);
    }

    #[test]
    fn every_discovered_url_is_also_a_muxed_video_choice_needing_no_audio_picked() {
        let html = r#"<html><head>
            <meta property="og:video:secure_url" content="https://cdn.example/best.mp4">
        </head><body>
            <video src="https://cdn.example/alt.webm"></video>
        </body></html>"#;
        let e = extract(html).unwrap();
        assert!(!e.videos.is_empty());
        assert_eq!(e.videos.len(), e.options.len());
        for option in &e.options {
            assert!(
                e.videos.iter().any(|v| v.stream == option.streams[0]),
                "{} is offered but is not a video choice",
                option.label
            );
        }
        assert!(
            e.videos.iter().all(|v| v.has_audio),
            "a page that states a file states a whole file"
        );
        assert!(e.videos.iter().all(|v| v.stream.kind == StreamKind::Muxed));
        assert!(
            e.audios.is_empty(),
            "nothing here separates picture from sound"
        );
        // Labelled from the extension, exactly as the options are.
        let labels: Vec<&str> = e.videos.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["MP4", "WebM"]);
        let ids: Vec<&str> = e.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["v0", "v1"], "ids are unique and stable");
        assert_eq!(e.videos.iter().filter(|v| v.best).count(), 1);
        assert!(e.videos[0].best);
        // Nothing states a resolution or a bitrate, so the page's own confidence order is
        // what survives the shared ranking rule.
        assert_eq!(e.videos[0].stream.url, "https://cdn.example/best.mp4");
    }
}
