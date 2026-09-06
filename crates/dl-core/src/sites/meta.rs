//! Instagram and Facebook — `instagram.com`, `cdninstagram.com`, `facebook.com`,
//! `fb.watch`, `fbcdn.net`.
//!
//! One extractor for both, because the two page shapes are close cousins: the same
//! renderer, the same habit of serialising the media description into JSON embedded in
//! the HTML, and the same CDN family. Splitting them would mean two copies of the
//! escape-decoding, which is the part that actually matters here.
//!
//! # The shapes this was built against
//!
//! Instagram, in order of preference: `<meta property="og:video" content="…">`, then a
//! `"video_versions":[{"url":…,"width":…,"height":…}]` array, then a bare
//! `"video_url":"…"`.
//!
//! Facebook, in order: `"playable_url_quality_hd"`, `"playable_url"`,
//! `"browser_native_hd_url"`, `"browser_native_sd_url"`, then `og:video`.
//!
//! # The escaping, which is the whole trick
//!
//! Every one of those keys lives inside JSON that was serialised into an HTML document,
//! so the value arrives as `https:\/\/scontent.xx.fbcdn.net\/v\/t.mp4?oh=1&oe=2`,
//! and the `og:video` form arrives HTML-escaped as `…?oh=1&amp;oe=2`. Both have to be
//! undone. A parser that skips either produces a URL that looks perfectly fine and 404s,
//! which is the single most common way one of these extractors is quietly wrong — hence
//! [`tests::escaped_urls_are_decoded_or_the_download_would_404`].
//!
//! **Honesty about provenance:** written on 2026-09-05 against those documented shapes
//! and *unverified against a live page* — Facebook redirects an outside fetch to a login
//! and Instagram answers a consent wall, so no real fixture could be captured. The tests
//! assert against hand-written samples.
//!
//! # Known limitation
//!
//! Meta sometimes nests JSON *inside a JSON string*, so the keys arrive doubly escaped
//! (`\"video_versions\":`). That is not unwrapped here: guessing at a second decoding
//! layer risks producing a plausible-looking wrong URL, and reporting
//! [`SiteError::Shape`] is the honest answer. If Meta returns to that form, this is the
//! first thing to fix.

use super::bilibili::{
    decode_html_entities, decode_json_escapes, html_title, json_array_after, json_string_value,
    meta_content,
};
use super::{
    host_is, safe_filename, Extraction, Extractor, MediaOption, Need, SiteError, Step, Stream,
    StreamKind, VideoChoice,
};
use serde_json::Value;

pub fn matches(host: &str) -> bool {
    host_is(host, "instagram.com")
        || host_is(host, "cdninstagram.com")
        || host_is(host, "facebook.com")
        || host_is(host, "fb.watch")
        || host_is(host, "fbcdn.net")
}

const INSTAGRAM: &str = "Instagram";
const FACEBOOK: &str = "Facebook";

/// Whether a host belongs to the Facebook half of the pair.
fn is_facebook(host: &str) -> bool {
    host_is(host, "facebook.com") || host_is(host, "fb.watch") || host_is(host, "fbcdn.net")
}

/// The nominal heights standing in for "HD" and "SD" when Meta names a rendition but not
/// its size. They exist only to sort the menu, and are never reported as a real height.
const HD_RANK: u64 = 1080;
const SD_RANK: u64 = 480;
/// `og:video` says nothing at all about quality, so it sits between the two.
const OG_RANK: u64 = 720;

#[derive(Debug)]
pub struct Meta {
    site: &'static str,
    referer: &'static str,
}

impl Default for Meta {
    /// Instagram, arbitrarily, for a `Meta` built without a URL.
    ///
    /// Nothing in the registry does that — [`build`] is what the registry calls, and it
    /// decides from the URL — but `Default` has to answer something, and both halves
    /// behave identically until `start` runs.
    fn default() -> Self {
        Self::for_url("https://www.instagram.com/")
    }
}

impl Meta {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build already knowing which of the two sites this is.
    ///
    /// One implementation covers Instagram and Facebook because their page shapes are
    /// close cousins, but they are two names to a user — and `sites::site_for` asks for
    /// that name *before* extraction starts, to label a button. Deciding here rather
    /// than in [`Extractor::start`] is what stops a Facebook link being called Instagram.
    pub fn for_url(url: &str) -> Self {
        let host = crate::policy::host_of(url).unwrap_or_default();
        let facebook = host_is(&host, "facebook.com")
            || host_is(&host, "fb.watch")
            || host_is(&host, "fbcdn.net");
        if facebook {
            Self {
                site: FACEBOOK,
                referer: "https://www.facebook.com/",
            }
        } else {
            Self {
                site: INSTAGRAM,
                referer: "https://www.instagram.com/",
            }
        }
    }
}

/// The registry's constructor for this extractor.
pub fn build(url: &str) -> Box<dyn Extractor> {
    Box::new(Meta::for_url(url))
}

impl Extractor for Meta {
    fn site(&self) -> &'static str {
        self.site
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        // The URL is the only thing that says which of the two sites this is, and the
        // answer decides both the reported name and the `Referer` the CDN wants.
        let host = crate::policy::host_of(url).unwrap_or_default();
        if is_facebook(&host) {
            self.site = FACEBOOK;
            self.referer = "https://www.facebook.com/";
        }
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let html = bodies
            .first()
            .copied()
            .ok_or_else(|| SiteError::Shape(self.site.to_string()))?;
        Ok(Step::Done(parse_post_page(html, self.site, self.referer)?))
    }
}

/// One candidate before it becomes a [`MediaOption`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    url: String,
    label: String,
    rank: u64,
    width: Option<u32>,
    height: Option<u32>,
}

/// Turn a post page's HTML into an [`Extraction`]. Pure; the tests call it directly.
pub fn parse_post_page(
    html: &str,
    site: &'static str,
    referer: &str,
) -> Result<Extraction, SiteError> {
    let mut candidates = if site == FACEBOOK {
        facebook_candidates(html)
    } else {
        instagram_candidates(html)
    };

    // Both sides can serve the same URL under two names (`playable_url` and
    // `browser_native_sd_url` are routinely identical); the first one wins, since the
    // candidate lists are already in preference order.
    let mut seen: Vec<String> = Vec::new();
    candidates.retain(|c| {
        let fresh = !seen.contains(&c.url);
        if fresh {
            seen.push(c.url.clone());
        }
        fresh
    });

    if candidates.is_empty() {
        return Err(login_wall(site, html));
    }

    let title = post_title(html, site);
    let mut options: Vec<MediaOption> = Vec::new();
    let mut videos: Vec<VideoChoice> = Vec::new();
    for (index, c) in candidates.into_iter().enumerate() {
        let stream = Stream {
            url: c.url,
            kind: StreamKind::Muxed,
            mime: Some("video/mp4".to_string()),
            size: None,
            headers: vec![("Referer".to_string(), referer.to_string())],
            max_chunk: None,
        };
        // Both sites serve one file carrying both tracks, so every rendition is a whole
        // answer on its own: `has_audio` is true and there is nothing to pair it with.
        videos.push(VideoChoice {
            // Neither site names a rendition. The URLs are signed and expire, so they are
            // no better an id than the position, and the position at least stays stable
            // for as long as the extraction it belongs to.
            id: format!("v{index}"),
            label: choice_label(&c.label, c.height),
            width: c.width,
            height: c.height,
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
            label: c.label,
            rank: c.rank,
            streams: vec![stream],
            filename: safe_filename(&title, "mp4"),
            width: c.width,
            height: c.height,
            duration_ms: None,
        });
    }
    options.sort_by_key(|o| std::cmp::Reverse(o.rank));

    let mut extraction = Extraction {
        site: site.to_string(),
        title,
        options,
        videos,
        // Meta never serves the sound separately, so a UI shows no audio picker here.
        audios: Vec::new(),
        subtitles: Vec::new(),
    };
    // The one place "best" is decided, for every site alike.
    extraction.rank_choices();
    Ok(extraction)
}

/// What a person reads in the picker: what Meta called the rendition, and the height when
/// `video_versions` stated one — "HD" alone says less than "HD · 720p" to someone deciding
/// whether it is worth the bytes.
fn choice_label(label: &str, height: Option<u32>) -> String {
    match height {
        // An Instagram version is already labelled by its height; saying it twice reads
        // like a mistake.
        Some(h) if !label.ends_with('p') => format!("{label} · {h}p"),
        _ => label.to_string(),
    }
}

/// What a user sees when the page held no media.
///
/// On these two sites that is nearly always a login or consent wall rather than a broken
/// extractor, so the sentence says the one thing that actually fixes it. The login markers
/// are checked so the message can be specific when the page admits what it is, but the
/// absence of media is itself the signal: a walled page never carries a media URL.
fn login_wall(site: &str, html: &str) -> SiteError {
    let walled = html.contains("loginForm") || html.contains("LoginForm");
    SiteError::Unavailable(if walled {
        format!(
            "{site} served a sign-in page instead of the post. Open the post in a tab you are \
             already signed into and try again — that is what makes this work."
        )
    } else {
        format!(
            "This {site} post states no video — it is private, deleted, or an image-only post. \
             If it is private, opening it in a tab you are already signed into is what makes \
             this work."
        )
    })
}

/// Instagram: `og:video`, then `video_versions`, then `video_url`.
fn instagram_candidates(html: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(url) = og_video(html) {
        out.push(Candidate {
            url,
            label: "Video".to_string(),
            rank: OG_RANK,
            width: None,
            height: None,
        });
    }

    // `video_versions` is the only Instagram source that states a size, so it is the only
    // one whose options can be ranked by anything real.
    if let Some(text) = json_array_after(html, "\"video_versions\":") {
        if let Ok(Value::Array(versions)) = serde_json::from_str::<Value>(text) {
            for version in &versions {
                let Some(url) = version
                    .get("url")
                    .and_then(Value::as_str)
                    .map(decode_json_escapes)
                    .filter(|u| u.starts_with("http"))
                else {
                    continue;
                };
                let width = dimension(version, "width");
                let height = dimension(version, "height");
                out.push(Candidate {
                    url,
                    label: height.map_or("Video".to_string(), |h| format!("{h}p")),
                    rank: height.map_or(OG_RANK, u64::from),
                    width,
                    height,
                });
            }
        }
    }

    if let Some(url) = json_string_value(html, "video_url").filter(|u| u.starts_with("http")) {
        out.push(Candidate {
            url,
            label: "Video".to_string(),
            rank: OG_RANK,
            width: None,
            height: None,
        });
    }
    out
}

/// Facebook: the two `playable_url` keys, the two `browser_native` keys, then `og:video`.
fn facebook_candidates(html: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (key, label, rank) in [
        ("playable_url_quality_hd", "HD", HD_RANK),
        ("browser_native_hd_url", "HD", HD_RANK),
        ("playable_url", "SD", SD_RANK),
        ("browser_native_sd_url", "SD", SD_RANK),
    ] {
        if let Some(url) = json_string_value(html, key).filter(|u| u.starts_with("http")) {
            out.push(Candidate {
                url,
                label: label.to_string(),
                rank,
                width: None,
                height: None,
            });
        }
    }
    if let Some(url) = og_video(html) {
        out.push(Candidate {
            url,
            label: "Video".to_string(),
            rank: OG_RANK,
            width: None,
            height: None,
        });
    }
    out
}

/// `og:video`, in either of the two spellings both sites emit.
fn og_video(html: &str) -> Option<String> {
    ["og:video:secure_url", "og:video", "og:video:url"]
        .into_iter()
        .find_map(|k| meta_content(html, k))
        // Entity decoding already happened in `meta_content`; this catches the pages that
        // put a JSON-escaped URL inside the attribute, which some Instagram renders do.
        .map(|u| decode_json_escapes(&u))
        .filter(|u| u.starts_with("http"))
}

fn post_title(html: &str, site: &str) -> String {
    for key in ["og:title", "twitter:title", "description", "og:description"] {
        if let Some(t) = meta_content(html, key).map(|t| t.trim().to_string()) {
            if !t.is_empty() {
                // Captions run to paragraphs; a filename does not.
                return t.lines().next().unwrap_or(&t).trim().to_string();
            }
        }
    }
    html_title(html)
        .map(|t| decode_html_entities(&t))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("{site} video"))
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

    const INSTAGRAM_PAGE: &str = r#"<html><head>
<meta property="og:title" content="someone on Instagram: &quot;a caption&quot;">
<meta property="og:video" content="https://scontent.cdninstagram.test/v/og.mp4?efg=1&amp;oe=2">
</head><body><script type="application/json">
{"items":[{"video_versions":[{"type":101,"width":720,"height":1280,"url":"https:\/\/scontent.cdninstagram.test\/v\/hi.mp4?efg=1&oe=2"},
{"type":102,"width":480,"height":854,"url":"https:\/\/scontent.cdninstagram.test\/v\/lo.mp4"}],
"video_url":"https:\/\/scontent.cdninstagram.test\/v\/legacy.mp4"}]}
</script></body></html>"#;

    const FACEBOOK_PAGE: &str = r#"<html><head>
<meta property="og:title" content="Someone&#39;s video">
<meta property="og:video" content="https://video.xx.fbcdn.test/v/og.mp4?_nc=1&amp;oe=2">
</head><body><script>
{"video":{"playable_url_quality_hd":"https:\/\/video.xx.fbcdn.test\/v\/hd.mp4?_nc=1&oe=2",
"playable_url":"https:\/\/video.xx.fbcdn.test\/v\/sd.mp4",
"browser_native_hd_url":"https:\/\/video.xx.fbcdn.test\/v\/hd.mp4?_nc=1&oe=2",
"browser_native_sd_url":"https:\/\/video.xx.fbcdn.test\/v\/native-sd.mp4"}}
</script></body></html>"#;

    const IG_URL: &str = "https://www.instagram.com/reel/Cabcdefghij/";
    const FB_URL: &str = "https://www.facebook.com/watch/?v=123456";

    fn extract(html: &str, url: &str) -> Extraction {
        let mut e = Meta::new();
        assert_eq!(e.start(url).unwrap(), Step::Need(Need::PageState));
        match e.feed(&[html]).unwrap() {
            Step::Done(x) => x,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn meta_claims_both_sites_and_both_cdns_but_no_lookalikes() {
        assert!(matches("www.instagram.com"));
        assert!(matches("scontent-lhr8-1.cdninstagram.com"));
        assert!(matches("www.facebook.com"));
        assert!(matches("fb.watch"));
        assert!(matches("video.xx.fbcdn.net"));
        assert!(!matches("notinstagram.com"));
        assert!(!matches("facebook.com.evil.test"));
    }

    #[test]
    fn the_url_decides_which_of_the_two_sites_this_is() {
        let mut ig = Meta::new();
        ig.start(IG_URL).unwrap();
        assert_eq!(ig.site(), "Instagram");

        let mut fb = Meta::new();
        fb.start(FB_URL).unwrap();
        assert_eq!(fb.site(), "Facebook");

        let mut watch = Meta::new();
        watch.start("https://fb.watch/abcdef/").unwrap();
        assert_eq!(watch.site(), "Facebook");
    }

    #[test]
    fn extraction_begins_by_asking_for_the_page_state() {
        let mut e = Meta::new();
        assert_eq!(e.start(IG_URL).unwrap(), Step::Need(Need::PageState));
    }

    #[test]
    fn instagram_offers_every_video_version_ranked_by_height() {
        let x = extract(INSTAGRAM_PAGE, IG_URL);
        assert_eq!(x.site, "Instagram");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["1280p", "854p", "Video", "Video"]);
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
        assert_eq!(x.options[0].height, Some(1280));
        assert_eq!(x.options[0].width, Some(720));
    }

    #[test]
    fn escaped_urls_are_decoded_or_the_download_would_404() {
        // The JSON-escaped form: backslashed slashes and a & for the ampersand.
        let ig = extract(INSTAGRAM_PAGE, IG_URL);
        assert_eq!(
            ig.options[0].streams[0].url,
            "https://scontent.cdninstagram.test/v/hi.mp4?efg=1&oe=2"
        );
        // The HTML-escaped form: &amp; inside a content attribute.
        let og = ig
            .options
            .iter()
            .find(|o| o.streams[0].url.contains("og.mp4"))
            .expect("the og:video candidate");
        assert_eq!(
            og.streams[0].url,
            "https://scontent.cdninstagram.test/v/og.mp4?efg=1&oe=2"
        );

        for option in ig.options.iter() {
            let url = &option.streams[0].url;
            assert!(!url.contains("\\/"), "backslashes survived: {url}");
            assert!(!url.contains("u0026"), "unicode escape survived: {url}");
            assert!(!url.contains("&amp;"), "html entity survived: {url}");
        }
    }

    #[test]
    fn facebook_ranks_hd_above_sd() {
        let x = extract(FACEBOOK_PAGE, FB_URL);
        assert_eq!(x.site, "Facebook");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["HD", "Video", "SD", "SD"]);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://video.xx.fbcdn.test/v/hd.mp4?_nc=1&oe=2"
        );
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
    }

    #[test]
    fn a_url_stated_under_two_names_is_offered_once() {
        let x = extract(FACEBOOK_PAGE, FB_URL);
        let hd: Vec<&MediaOption> = x
            .options
            .iter()
            .filter(|o| o.streams[0].url.contains("hd.mp4"))
            .collect();
        assert_eq!(
            hd.len(),
            1,
            "playable_url_quality_hd and browser_native_hd_url are the same file"
        );
    }

    #[test]
    fn each_site_gets_its_own_referer() {
        let ig = extract(INSTAGRAM_PAGE, IG_URL);
        assert_eq!(
            ig.options[0].streams[0].headers,
            vec![(
                "Referer".to_string(),
                "https://www.instagram.com/".to_string()
            )]
        );
        let fb = extract(FACEBOOK_PAGE, FB_URL);
        assert_eq!(
            fb.options[0].streams[0].headers,
            vec![(
                "Referer".to_string(),
                "https://www.facebook.com/".to_string()
            )]
        );
    }

    #[test]
    fn the_title_comes_from_open_graph_with_entities_decoded() {
        assert_eq!(
            extract(INSTAGRAM_PAGE, IG_URL).title,
            "someone on Instagram: \"a caption\""
        );
        assert_eq!(extract(FACEBOOK_PAGE, FB_URL).title, "Someone's video");
    }

    #[test]
    fn a_title_with_no_open_graph_falls_back_to_the_document_title() {
        let html = r#"<title>A plain page</title><script>{"playable_url":"https://video.xx.fbcdn.test/v/x.mp4"}</script>"#;
        assert_eq!(extract(html, FB_URL).title, "A plain page");
    }

    #[test]
    fn a_multi_line_caption_is_trimmed_to_its_first_line_for_the_filename() {
        let html = "<meta property=\"og:title\" content=\"first line\nsecond line\">\
                    <meta property=\"og:video\" content=\"https://scontent.cdninstagram.test/v/a.mp4\">";
        let x = extract(html, IG_URL);
        assert_eq!(x.title, "first line");
        assert_eq!(x.options[0].filename, "first line.mp4");
    }

    #[test]
    fn a_bare_video_url_key_is_enough_on_its_own() {
        let html =
            r#"<script>{"video_url":"https:\/\/scontent.cdninstagram.test\/v\/only.mp4"}</script>"#;
        let x = extract(html, IG_URL);
        assert_eq!(x.options.len(), 1);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://scontent.cdninstagram.test/v/only.mp4"
        );
    }

    #[test]
    fn a_sign_in_page_says_what_to_do_about_it() {
        let html = r#"<html><body><form id="loginForm" method="post"></form></body></html>"#;
        let mut e = Meta::new();
        e.start(IG_URL).unwrap();
        let err = e.feed(&[html]).unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(message.contains("Instagram"), "{message}");
        assert!(message.contains("signed into"), "{message}");
        assert_eq!(err.to_string(), *message);
    }

    #[test]
    fn a_page_with_no_media_at_all_is_unavailable_and_names_the_site() {
        let mut e = Meta::new();
        e.start(FB_URL).unwrap();
        let err = e.feed(&["<html>an image post</html>"]).unwrap_err();
        let SiteError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}")
        };
        assert!(message.contains("Facebook"), "{message}");
        assert!(
            message.contains("private, deleted, or an image-only post"),
            "{message}"
        );
    }

    #[test]
    fn a_relative_or_non_http_candidate_is_never_offered() {
        let html = r#"<meta property="og:video" content="/relative.mp4"><script>{"video_url":"blob:whatever"}</script>"#;
        let mut e = Meta::new();
        e.start(IG_URL).unwrap();
        assert!(matches!(e.feed(&[html]), Err(SiteError::Unavailable(_))));
    }

    #[test]
    fn feeding_nothing_reports_shape_instead_of_panicking() {
        let mut e = Meta::new();
        e.start(IG_URL).unwrap();
        assert_eq!(e.feed(&[]), Err(SiteError::Shape("Instagram".into())));
    }

    #[test]
    fn doubly_escaped_json_is_not_guessed_at_it_is_reported() {
        // The known limitation, asserted so the behaviour is deliberate rather than
        // accidental: nothing is extracted, and the user gets a sentence, not a bad URL.
        let html = r#"<script>{"payload":"{\"video_versions\":[{\"url\":\"https:\\/\\/x.test\\/a.mp4\"}]}"}</script>"#;
        let mut e = Meta::new();
        e.start(IG_URL).unwrap();
        assert!(matches!(e.feed(&[html]), Err(SiteError::Unavailable(_))));
    }

    #[test]
    fn every_option_is_also_a_muxed_video_choice_that_needs_no_audio_picked() {
        for (page, url) in [(INSTAGRAM_PAGE, IG_URL), (FACEBOOK_PAGE, FB_URL)] {
            let x = extract(page, url);
            assert!(!x.videos.is_empty(), "{}", x.site);
            assert_eq!(x.videos.len(), x.options.len(), "{}", x.site);
            for option in &x.options {
                assert!(
                    x.videos.iter().any(|v| v.stream == option.streams[0]),
                    "{} on {} is offered but is not a video choice",
                    option.label,
                    x.site
                );
            }
            assert!(
                x.videos.iter().all(|v| v.has_audio),
                "{} serves one file carrying both tracks",
                x.site
            );
            assert!(x.videos.iter().all(|v| v.stream.kind == StreamKind::Muxed));
            assert!(x.audios.is_empty(), "{} muxes its audio in", x.site);
            assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1, "{}", x.site);
            assert!(x.videos[0].best, "{}", x.site);
            let mut ids: Vec<&str> = x.videos.iter().map(|v| v.id.as_str()).collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), count, "two choices on {} share an id", x.site);
        }
    }

    #[test]
    fn instagrams_best_choice_is_the_largest_version_the_page_stated() {
        let x = extract(INSTAGRAM_PAGE, IG_URL);
        let best = x.best_video().expect("a best video");
        assert!(best.best);
        assert_eq!(best.label, "1280p");
        assert_eq!((best.width, best.height), (Some(720), Some(1280)));
        assert_eq!(
            best.stream.url,
            "https://scontent.cdninstagram.test/v/hi.mp4?efg=1&oe=2"
        );
        // `video_versions` is the only Instagram source that states a size, so the two
        // candidates that state nothing sort below both of them.
        let labels: Vec<&str> = x.videos.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["1280p", "854p", "Video", "Video"]);
    }

    #[test]
    fn facebook_names_its_choices_the_way_the_page_named_them() {
        let x = extract(FACEBOOK_PAGE, FB_URL);
        let labels: Vec<&str> = x.videos.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["HD", "SD", "SD", "Video"]);
        assert!(x.videos[0].best);
        assert_eq!(
            x.videos[0].stream.url,
            "https://video.xx.fbcdn.test/v/hd.mp4?_nc=1&oe=2"
        );
    }

    #[test]
    fn a_rendition_gains_its_resolution_only_when_the_name_is_not_already_saying_it() {
        assert_eq!(choice_label("HD", Some(720)), "HD · 720p");
        assert_eq!(choice_label("1280p", Some(1280)), "1280p");
        assert_eq!(choice_label("SD", None), "SD");
    }
}
