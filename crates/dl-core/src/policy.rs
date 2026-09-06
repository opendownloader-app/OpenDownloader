//! Compliance policy.
//!
//! There is exactly one principle here, and every rule below is an expression of it:
//!
//! > **We do not break encryption.**
//!
//! That is a narrower line than this module once drew, and the narrowing was deliberate.
//! An earlier version refused a list of hosts on terms-of-service grounds — YouTube,
//! Bilibili and the rest — which is a *positioning* decision, not a legal one, and the
//! owner of this project has decided differently. Downloading a plain, unencrypted file
//! that a server hands your browser is a different act from circumventing an access
//! control, and the two do not belong under one rule.
//!
//! So what remains is the act that genuinely is different in kind:
//!
//! 1. [`is_restricted`] — services whose catalogue is DRM-protected
//!    (Widevine/FairPlay/PlayReady). Downloading from them necessarily means defeating
//!    that protection, which is a distinct legal category under the DMCA's §1201 and its
//!    equivalents elsewhere. Refused on both the page origin and the media origin.
//! 2. [`refuse_encrypted`] — any stream that declares encryption, on **any** host,
//!    including hosts not on the list above. No key is ever fetched and nothing is ever
//!    decrypted.
//!
//! Rule 2 is what makes rule 1 a short list rather than an endless one. A site with a
//! free catalogue and a paid, protected one — Bilibili and iQiyi both work this way —
//! needs no special handling: its open content downloads, and its protected content
//! refuses itself the moment the manifest declares a key.
//!
//! Both are pure functions over their inputs, so they can be audited by reading them,
//! and neither has an override, an "advanced mode", or a setting that turns it off.

/// Services whose catalogue is DRM-protected.
///
/// The test for inclusion is not "does this site dislike downloaders" — nearly all of
/// them do — but "is the media itself under an access control we would have to defeat".
/// Every entry here ships Widevine, FairPlay or PlayReady on its main catalogue.
///
/// Matching is boundary-aware (see [`host_matches`]): an entry matches the host itself
/// and any subdomain of it, but never a lookalike domain that merely ends with the same
/// characters.
const RESTRICTED_HOSTS: &[&str] = &[
    // Subscription video.
    "netflix.com",
    "nflxvideo.net",
    "disneyplus.com",
    "disney-plus.net",
    "hotstar.com",
    "primevideo.com",
    "amazonvideo.com",
    "aiv-cdn.net",
    "max.com",
    "hbomax.com",
    "hulu.com",
    "peacocktv.com",
    "paramountplus.com",
    "appletv.com",
    "tv.apple.com",
    "crunchyroll.com",
    // Music and audiobooks. Every one of these is DRM-protected end to end.
    "spotify.com",
    "scdn.co",
    "music.apple.com",
    "audible.com",
    "tidal.com",
    "deezer.com",
];

/// True when either the page origin or the media URL lives on a restricted host.
///
/// Both sides are checked because neither alone is sufficient: a restricted site can
/// serve media from a neutral CDN, and a neutral site can embed a restricted player.
pub fn is_restricted(page_origin: &str, media_url: &str) -> bool {
    [page_origin, media_url]
        .iter()
        .filter_map(|u| host_of(u))
        .any(|host| RESTRICTED_HOSTS.iter().any(|b| host_matches(&host, b)))
}

/// True when a manifest declares any form of encryption, HLS or DASH.
///
/// This is the check that does the real work, and the reason [`RESTRICTED_HOSTS`] can be
/// a short list of whole services rather than an endless list of domains: a site with
/// both free and protected content refuses its protected content by itself, here, on the
/// evidence of its own manifest.
///
/// For HLS, `METHOD=NONE` is the spec's way of *ending* an encrypted stretch, so it is
/// not a refusal. Anything else — `AES-128`, `SAMPLE-AES`, `SAMPLE-AES-CTR` — is. For
/// DASH, any `ContentProtection` element is.
pub fn refuse_encrypted(manifest_text: &str) -> bool {
    // DASH states protection as an element rather than a tag, and a `cenc` scheme or a
    // Widevine/PlayReady/FairPlay system id all mean the same thing here: the samples
    // are encrypted and we would have to obtain a key to read them.
    if manifest_text.contains("<ContentProtection") || manifest_text.contains("ContentProtection ")
    {
        return true;
    }

    manifest_text.lines().any(|line| {
        let line = line.trim();
        let is_key_tag = line.starts_with("#EXT-X-KEY:") || line.starts_with("#EXT-X-SESSION-KEY:");
        is_key_tag && !method_is_none(line)
    })
}

fn method_is_none(line: &str) -> bool {
    let Some(rest) = line.split_once("METHOD=").map(|(_, r)| r) else {
        return false;
    };
    let value = rest
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    value.eq_ignore_ascii_case("NONE")
}

/// Extract the lowercase host from a URL.
///
/// Hand-rolled rather than using the `url` crate: that pulls in `idna` and its Unicode
/// tables, which is a large amount of wasm for a job this small.
pub fn host_of(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        // A bare host with no scheme is still usable (page origins are always absolute,
        // but callers pass through unvalidated strings).
        None => url,
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo, then the port.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = match authority.strip_prefix('[') {
        // IPv6 literal — keep the brackets off and ignore any port after them.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.split(':').next().unwrap_or(authority),
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Boundary-aware suffix match.
///
/// `m.youtube.com` matches `youtube.com`; `notyoutube.com` must not. A naive
/// `ends_with` gets the second case wrong, which would block innocent domains.
fn host_matches(host: &str, blocked: &str) -> bool {
    host == blocked
        || (host.len() > blocked.len()
            && host.ends_with(blocked)
            && host.as_bytes()[host.len() - blocked.len() - 1] == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_drm_services_on_either_origin() {
        // A restricted site can serve media from a neutral CDN, and a neutral site can
        // embed a restricted player, so neither side alone is sufficient.
        assert!(is_restricted(
            "https://www.netflix.com",
            "https://cdn.example.com/a.mp4"
        ));
        assert!(is_restricted(
            "https://blog.example.com",
            "https://x.nflxvideo.net/range/0-100"
        ));
        assert!(is_restricted(
            "https://open.spotify.com",
            "https://x/y.m3u8"
        ));
    }

    #[test]
    fn the_major_video_platforms_are_no_longer_refused_by_host() {
        // These were refused on terms-of-service grounds, which is a positioning
        // decision rather than a legal one, and the project has decided differently.
        // What still protects their paid catalogues is `refuse_encrypted`, which acts
        // on the manifest rather than on the domain name.
        for origin in [
            "https://www.youtube.com",
            "https://www.bilibili.com",
            "https://www.tiktok.com",
            "https://www.douyin.com",
            "https://www.instagram.com",
            "https://www.facebook.com",
            "https://mp.weixin.qq.com",
            "https://x.com",
            "https://www.iqiyi.com",
        ] {
            assert!(
                !is_restricted(origin, "https://cdn.example.com/a.mp4"),
                "{origin} should no longer be refused by host"
            );
        }
        // …including their media CDNs.
        assert!(!is_restricted(
            "https://www.youtube.com",
            "https://r1---sn-x.googlevideo.com/videoplayback"
        ));
    }

    #[test]
    fn allows_ordinary_sites() {
        assert!(!is_restricted(
            "https://blog.example.com",
            "https://cdn.example.com/a.mp4"
        ));
    }

    #[test]
    fn subdomain_match_is_boundary_aware() {
        assert!(is_restricted("https://www.netflix.com", "https://x/y.mp4"));
        assert!(!is_restricted("https://notnetflix.com", "https://x/y.mp4"));
        assert!(!is_restricted(
            "https://netflix.com.evil.io",
            "https://x/y.mp4"
        ));
    }

    #[test]
    fn host_extraction_handles_ports_userinfo_and_ipv6() {
        assert_eq!(
            host_of("https://user:pw@Example.COM:8443/a"),
            Some("example.com".into())
        );
        assert_eq!(host_of("http://[::1]:9000/x"), Some("::1".into()));
        assert_eq!(host_of("https://cdn.x/a?b=c#d"), Some("cdn.x".into()));
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn refuses_encrypted_playlists() {
        assert!(refuse_encrypted(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n"
        ));
        assert!(refuse_encrypted(
            "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES\n"
        ));
        assert!(!refuse_encrypted("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n"));
        assert!(!refuse_encrypted("#EXTM3U\n#EXTINF:4,\nseg.ts\n"));
    }

    #[test]
    fn a_dash_manifest_declaring_content_protection_is_refused() {
        // The host list is short precisely because this check does the work: a site with
        // a free catalogue and a protected one is handled by its own manifests.
        assert!(refuse_encrypted(
            "<MPD><Period><AdaptationSet><ContentProtection schemeIdUri=\"urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED\"/></AdaptationSet></Period></MPD>"
        ));
        assert!(refuse_encrypted(
            "<MPD><ContentProtection schemeIdUri=\"urn:mpeg:dash:mp4protection:2011\" value=\"cenc\"/></MPD>"
        ));
        assert!(!refuse_encrypted(
            "<MPD><Period><AdaptationSet/></Period></MPD>"
        ));
    }
}
