//! Turning an observed network request into a download candidate.
//!
//! The service worker sees every response header on a page it has been granted access
//! to. Almost all of it is noise. [`classify`] is the pure function that decides what is
//! worth offering the user, and it is the only place that decision is made.

use serde::{Deserialize, Serialize};

use crate::policy;

/// What kind of download a candidate implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// A single HTTP resource fetched with byte ranges.
    Progressive,
    /// An HLS playlist that must be parsed and expanded into segments.
    HlsPlaylist,
}

/// Everything the sniffer knows about one observed response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMeta {
    pub url: String,
    pub page_origin: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub content_disposition: Option<String>,
}

/// A downloadable thing we are willing to offer the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaCandidate {
    pub url: String,
    pub kind: MediaKind,
    pub filename: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

/// MIME types we treat as directly downloadable media.
/// What counts as media worth offering.
///
/// Images are deliberately absent. They were here, and on a page like a TikTok or Douyin
/// feed that meant sixty-odd thumbnails crowding out the one video someone came for —
/// the popup listed avatars and preview frames beside the file they wanted. A browser
/// already saves an image in two clicks; this is a video and audio downloader, and the
/// list is only useful if everything in it is something you would plausibly download.
const PROGRESSIVE_MIME_PREFIXES: &[&str] = &["video/", "audio/"];

/// Non-media MIME types worth offering anyway — the "downloader", not "video grabber",
/// half of the product.
const PROGRESSIVE_MIME_EXACT: &[&str] = &[
    "application/pdf",
    "application/zip",
    "application/x-zip-compressed",
    "application/gzip",
    "application/x-tar",
    "application/x-7z-compressed",
    "application/x-rar-compressed",
    "application/epub+zip",
    "application/octet-stream",
];

/// MIME types that identify an HLS playlist.
const HLS_MIMES: &[&str] = &[
    "application/vnd.apple.mpegurl",
    "application/x-mpegurl",
    "audio/mpegurl",
    "audio/x-mpegurl",
    "vnd.apple.mpegurl",
];

/// Extensions belonging to *segments* of a stream rather than a whole file. These are
/// fetched as part of a playlist job and must never be offered individually — otherwise
/// a single HLS video floods the UI with hundreds of useless two-second candidates.
const SEGMENT_EXTENSIONS: &[&str] = &["ts", "m4s", "cmfv", "cmfa"];

/// Segment MIME types, same reasoning as [`SEGMENT_EXTENSIONS`].
const SEGMENT_MIMES: &[&str] = &["video/mp2t", "audio/mp2t"];

/// Whether a stream is an HLS playlist rather than a file.
///
/// Public because the front ends need the same answer and were guessing at it. Both
/// labelled every non-merged option "progressive", so a Vimeo master playlist was
/// downloaded as though it were the video: 2.6 KB of `.m3u8` written to a `.mp4`,
/// reported complete, and given a SHA-256 — a wrong answer wearing the badge of a
/// verified one. Deciding it here means one rule, and it is the rule the sniffer already
/// classifies by.
pub fn is_hls_playlist(mime: Option<&str>, url: &str) -> bool {
    let mime = mime.map(normalize_mime);
    if mime.as_deref().is_some_and(|m| HLS_MIMES.contains(&m)) {
        return true;
    }
    url_extension(url).as_deref() == Some("m3u8")
}

/// Decide whether an observed response is worth offering, and as what.
///
/// Returns `None` for anything that is not media, is a stream segment, or is barred by
/// [`policy`].
pub fn classify(meta: &RequestMeta) -> Option<MediaCandidate> {
    if policy::is_restricted(&meta.page_origin, &meta.url) {
        return None;
    }

    let mime = meta
        .content_type
        .as_deref()
        .map(normalize_mime)
        .filter(|m| !m.is_empty());
    let ext = url_extension(&meta.url);

    // Segments first: they would otherwise match the progressive rules below.
    if mime.as_deref().is_some_and(|m| SEGMENT_MIMES.contains(&m))
        || ext
            .as_deref()
            .is_some_and(|e| SEGMENT_EXTENSIONS.contains(&e))
    {
        return None;
    }

    let kind = if is_hls_playlist(mime.as_deref(), &meta.url) {
        MediaKind::HlsPlaylist
    } else if mime.as_deref().is_some_and(is_progressive_mime) {
        MediaKind::Progressive
    } else if mime.is_none() && ext.as_deref().is_some_and(is_progressive_extension) {
        // No Content-Type at all (some CDNs omit it) — fall back to the URL.
        MediaKind::Progressive
    } else {
        return None;
    };

    // `application/octet-stream` is the CDN default for "I don't know"; without a
    // recognisable extension it is as likely to be a font or a WASM blob as a video.
    if mime.as_deref() == Some("application/octet-stream")
        && !ext.as_deref().is_some_and(is_progressive_extension)
    {
        return None;
    }

    Some(MediaCandidate {
        filename: derive_filename(meta, kind, mime.as_deref(), ext.as_deref()),
        url: meta.url.clone(),
        kind,
        mime,
        size: meta.content_length,
    })
}

fn is_progressive_mime(mime: &str) -> bool {
    PROGRESSIVE_MIME_PREFIXES
        .iter()
        .any(|p| mime.starts_with(p))
        || PROGRESSIVE_MIME_EXACT.contains(&mime)
}

fn is_progressive_extension(ext: &str) -> bool {
    // Image extensions are absent for the same reason as the image MIME prefix above:
    // a page loads dozens of them by itself, and every one became a download candidate.
    //
    // Documents and archives stay. The distinction is not arbitrary — a page does not
    // quietly fetch forty PDFs while you watch a video, so they never crowd anything out,
    // and a direct link to one is a thing somebody deliberately opened.
    const EXTS: &[&str] = &[
        "mp4", "m4v", "mov", "webm", "mkv", "avi", "flv", "ogv", "mp3", "m4a", "aac", "flac",
        "wav", "ogg", "opus", "pdf", "zip", "gz", "tar", "7z", "rar", "epub",
    ];
    EXTS.contains(&ext)
}

/// Lowercase the type and drop any `; charset=…` parameters.
fn normalize_mime(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The lowercase extension from a URL's path, ignoring query and fragment.
fn url_extension(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Pick a filename: Content-Disposition wins, then the URL path, then a generic name.
/// The extension is corrected from the MIME type when the source had none.
fn derive_filename(
    meta: &RequestMeta,
    kind: MediaKind,
    mime: Option<&str>,
    ext: Option<&str>,
) -> String {
    if let Some(name) = meta
        .content_disposition
        .as_deref()
        .and_then(filename_from_disposition)
    {
        return sanitize_filename(&name);
    }

    let stem = url_stem(&meta.url).unwrap_or_else(|| "download".to_string());
    let wanted_ext = match kind {
        // A playlist is not what lands on disk — an fMP4 is.
        MediaKind::HlsPlaylist => "mp4".to_string(),
        MediaKind::Progressive => ext
            .map(str::to_string)
            .or_else(|| mime.and_then(extension_for_mime).map(str::to_string))
            .unwrap_or_else(|| "bin".to_string()),
    };
    sanitize_filename(&format!("{stem}.{wanted_ext}"))
}

/// The last path segment with any extension removed.
fn url_stem(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|s| !s.is_empty())?;
    let stem = match last.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => last,
    };
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" | "video/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/gzip" => "gz",
        "application/x-tar" => "tar",
        "application/epub+zip" => "epub",
        _ => return None,
    })
}

/// Pull a filename out of a Content-Disposition header.
///
/// `filename*=UTF-8''…` (RFC 5987) takes precedence over plain `filename=`, matching how
/// browsers themselves resolve the two.
fn filename_from_disposition(header: &str) -> Option<String> {
    if let Some(idx) = header.find("filename*=") {
        let value = header[idx + "filename*=".len()..]
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        // Strip the charset'language' prefix, then percent-decode.
        let encoded = value.rsplit('\'').next().unwrap_or(value);
        let decoded = percent_decode(encoded);
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    let idx = header.find("filename=")?;
    let value = header[idx + "filename=".len()..]
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = core::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip anything that could escape the download directory or confuse a filesystem.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    if trimmed.is_empty() {
        return "download.bin".to_string();
    }
    // Leave room for the manager's own de-duplication suffixes.
    trimmed.chars().take(180).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(url: &str, ct: Option<&str>) -> RequestMeta {
        RequestMeta {
            url: url.into(),
            page_origin: "https://ok.example".into(),
            content_type: ct.map(Into::into),
            content_length: None,
            content_disposition: None,
        }
    }

    #[test]
    fn detects_progressive_by_mime() {
        let c = classify(&meta("https://cdn.x/a?q=1", Some("video/mp4"))).unwrap();
        assert_eq!(c.kind, MediaKind::Progressive);
        assert_eq!(c.filename, "a.mp4");
    }

    #[test]
    fn detects_hls_by_extension_and_mime() {
        assert_eq!(
            classify(&meta("https://cdn.x/master.m3u8", None))
                .unwrap()
                .kind,
            MediaKind::HlsPlaylist
        );
        assert_eq!(
            classify(&meta(
                "https://cdn.x/p",
                Some("application/vnd.apple.mpegurl")
            ))
            .unwrap()
            .kind,
            MediaKind::HlsPlaylist
        );
    }

    #[test]
    fn hls_candidates_are_named_for_what_lands_on_disk() {
        let c = classify(&meta("https://cdn.x/hls/master.m3u8", None)).unwrap();
        assert_eq!(c.filename, "master.mp4");
    }

    #[test]
    fn ignores_non_media() {
        assert!(classify(&meta(
            "https://cdn.x/app.js",
            Some("application/javascript")
        ))
        .is_none());
        assert!(classify(&meta("https://cdn.x/page", Some("text/html"))).is_none());
        assert!(classify(&meta("https://cdn.x/s.css", Some("text/css"))).is_none());
    }

    #[test]
    fn octet_stream_needs_a_recognisable_extension() {
        assert!(classify(&meta(
            "https://cdn.x/blob",
            Some("application/octet-stream")
        ))
        .is_none());
        assert!(classify(&meta(
            "https://cdn.x/f.zip",
            Some("application/octet-stream")
        ))
        .is_some());
    }

    #[test]
    fn content_disposition_wins_over_url_path() {
        let mut m = meta("https://cdn.x/blob?id=9", Some("video/mp4"));
        m.content_disposition = Some("attachment; filename=\"My Clip.mp4\"".into());
        assert_eq!(classify(&m).unwrap().filename, "My Clip.mp4");
    }

    #[test]
    fn rfc5987_filenames_are_decoded_and_preferred() {
        let mut m = meta("https://cdn.x/blob", Some("video/mp4"));
        m.content_disposition =
            Some("attachment; filename=\"fallback.mp4\"; filename*=UTF-8''caf%C3%A9.mp4".into());
        assert_eq!(classify(&m).unwrap().filename, "café.mp4");
    }

    #[test]
    fn filenames_cannot_escape_the_download_directory() {
        let mut m = meta("https://cdn.x/blob", Some("video/mp4"));
        m.content_disposition = Some("attachment; filename=\"../../etc/passwd\"".into());
        let name = classify(&m).unwrap().filename;
        assert!(!name.contains('/'), "got {name}");
        assert!(!name.starts_with('.'), "got {name}");
    }

    #[test]
    fn drm_hosts_never_surface() {
        let mut m = meta("https://cdn.x/a.mp4", Some("video/mp4"));
        m.page_origin = "https://www.netflix.com".into();
        assert!(classify(&m).is_none());
    }

    #[test]
    fn the_major_video_platforms_do_surface() {
        // They were refused on terms-of-service grounds and no longer are; what still
        // stops a protected stream is `policy::refuse_encrypted`, which reads the
        // manifest rather than the domain.
        let mut m = meta("https://cdn.x/a.mp4", Some("video/mp4"));
        m.page_origin = "https://www.youtube.com".into();
        assert!(classify(&m).is_some());
        m.page_origin = "https://www.bilibili.com".into();
        assert!(classify(&m).is_some());
    }

    #[test]
    fn hls_segments_are_not_candidates() {
        assert!(classify(&meta("https://cdn.x/seg00001.ts", Some("video/mp2t"))).is_none());
        assert!(classify(&meta("https://cdn.x/seg1.m4s", None)).is_none());
    }

    #[test]
    fn missing_content_type_falls_back_to_the_extension() {
        let c = classify(&meta("https://cdn.x/movie.mkv", None)).unwrap();
        assert_eq!(c.kind, MediaKind::Progressive);
        assert_eq!(c.filename, "movie.mkv");
    }

    #[test]
    fn extension_is_derived_from_mime_when_the_url_has_none() {
        let c = classify(&meta("https://cdn.x/stream", Some("audio/mpeg"))).unwrap();
        assert_eq!(c.filename, "stream.mp3");
    }

    #[test]
    fn size_is_carried_through_from_content_length() {
        let mut m = meta("https://cdn.x/a.mp4", Some("video/mp4"));
        m.content_length = Some(1234);
        assert_eq!(classify(&m).unwrap().size, Some(1234));
    }
}

#[cfg(test)]
mod images_are_not_media {
    use super::*;

    fn meta(url: &str, content_type: Option<&str>) -> RequestMeta {
        RequestMeta {
            url: url.to_string(),
            page_origin: "https://example.com".to_string(),
            content_type: content_type.map(str::to_string),
            content_length: Some(4096),
            content_disposition: None,
        }
    }

    /// A feed page loads dozens of thumbnails and avatars. Every one of them used to be
    /// offered, which buried the single video someone actually came for — sixty-one
    /// candidates on a Douyin page, almost all of them pictures.
    #[test]
    fn an_image_is_never_offered_as_a_download() {
        for (url, mime) in [
            ("https://cdn.example.com/thumb.jpg", Some("image/jpeg")),
            ("https://cdn.example.com/avatar.png", Some("image/png")),
            ("https://cdn.example.com/cover.webp", Some("image/webp")),
            ("https://cdn.example.com/icon.svg", Some("image/svg+xml")),
            // No Content-Type at all: the URL must not rescue it either.
            ("https://cdn.example.com/preview.jpg", None),
        ] {
            assert!(
                classify(&meta(url, mime)).is_none(),
                "should not be offered: {url} ({mime:?})"
            );
        }
    }

    #[test]
    fn video_and_audio_are_still_offered() {
        for (url, mime) in [
            ("https://cdn.example.com/clip.mp4", Some("video/mp4")),
            ("https://cdn.example.com/song.m4a", Some("audio/mp4")),
            ("https://cdn.example.com/clip.mp4", None),
        ] {
            assert!(
                classify(&meta(url, mime)).is_some(),
                "should still be offered: {url} ({mime:?})"
            );
        }
    }
}
