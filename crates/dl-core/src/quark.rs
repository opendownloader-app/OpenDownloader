//! Quark netdisk share links: what is in one, and how far a reader can get.
//!
//! # The shape
//!
//! `https://pan.quark.cn/s/<pwd_id>`, optionally with a passcode — either as
//! `?pwd=<code>` or typed by the visitor when the share asks for one. Everything after
//! the id (`#/list/share`, tracking parameters) is the web client's own routing and
//! carries nothing a reader needs.
//!
//! # How much of a share is readable, and by whom
//!
//! Two of Quark's three steps answer with no account at all:
//!
//! | step | endpoint | anonymous? |
//! |---|---|---|
//! | exchange the id for a session token | `share/sharepage/token` | yes |
//! | list a directory inside the share | `share/sharepage/detail` | yes |
//! | turn a file id into a download URL | `file/download` | **no** |
//!
//! The third answers `code 23018, "download file size limit"` to a caller with no
//! session — measured against files of 155 MB, 605 MB and 61 GB in the same share, so it
//! is the absence of an account rather than the size of any particular file. Signing in
//! is what lifts it, and the cap that remains after that is the one the account is
//! entitled to.
//!
//! So this module exists to read a share, not to defeat its gate. Listing is genuinely
//! useful on its own — names, sizes and structure, before committing to anything — and it
//! is exactly what Quark hands any visitor who opens the page.
//!
//! # Why none of this can happen in the page
//!
//! Quark's API allows one origin, its own. A request carrying any other `Origin` is
//! answered `403` outright, and so is the preflight, so no header a page can set makes
//! the call succeed. Every request here goes through the relay, which is a program on the
//! user's own machine and sends no `Origin` at all.

use serde::Serialize;

/// A parsed Quark share link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarkShare {
    /// The share id — Quark calls it `pwd_id`, and it is what the token call takes.
    pub pwd_id: String,
    /// The visitor's passcode, or empty when the share has none.
    pub passcode: String,
}

/// Whether this URL is a Quark share link.
pub fn is_share_link(url: &str) -> bool {
    parse(url).is_some()
}

/// Read a Quark share link, or `None` if it is not one.
pub fn parse(url: &str) -> Option<QuarkShare> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    // `pan.quark.cn` is the share host. Quark's other hosts (`drive-pc`, `drive-h`) are
    // API endpoints and never carry a share link.
    let rest = rest.strip_prefix("pan.quark.cn/")?;
    let rest = rest.strip_prefix("s/")?;

    // The id ends at whatever the web client appended: a query, a fragment, or a path
    // segment of its own.
    let id_end = rest.find(['?', '#', '/']).unwrap_or(rest.len());
    let pwd_id = &rest[..id_end];
    // Quark's ids are short lowercase hex. Checking the shape keeps `pan.quark.cn/s/`
    // followed by something else from being reported as a share.
    if pwd_id.is_empty() || !pwd_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    Some(QuarkShare {
        pwd_id: pwd_id.to_string(),
        passcode: passcode_in(&rest[id_end..]).unwrap_or_default(),
    })
}

/// Pull `pwd=` out of the part of the link after the id, query or fragment alike.
fn passcode_in(tail: &str) -> Option<String> {
    for part in tail.split(['?', '#', '&']) {
        if let Some(value) = part.strip_prefix("pwd=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(url: &str) -> QuarkShare {
        parse(url).unwrap_or_else(|| panic!("expected a share link: {url}"))
    }

    #[test]
    fn reads_a_plain_share() {
        let s = share("https://pan.quark.cn/s/6af352275c72");
        assert_eq!(s.pwd_id, "6af352275c72");
        assert_eq!(s.passcode, "");
    }

    #[test]
    fn ignores_the_web_client_s_own_routing() {
        // The fragment is the Quark page's router, not part of the share id.
        assert_eq!(
            share("https://pan.quark.cn/s/6af352275c72#/list/share").pwd_id,
            "6af352275c72"
        );
        assert_eq!(share("http://www.pan.quark.cn/s/abc123/").pwd_id, "abc123");
    }

    #[test]
    fn finds_a_passcode_in_either_place() {
        assert_eq!(
            share("https://pan.quark.cn/s/6af352275c72?pwd=1a2b").passcode,
            "1a2b"
        );
        assert_eq!(
            share("https://pan.quark.cn/s/6af352275c72#pwd=1a2b").passcode,
            "1a2b"
        );
        // Not the first parameter, and not the only one.
        assert_eq!(
            share("https://pan.quark.cn/s/abc123?from=x&pwd=zz9").passcode,
            "zz9"
        );
    }

    #[test]
    fn refuses_what_is_not_a_share() {
        for url in [
            "https://pan.quark.cn/",
            "https://pan.quark.cn/s/",
            // A different Quark surface, not a share.
            "https://pan.quark.cn/list#/list/all",
            "https://pan.baidu.com/s/6af352275c72",
            "https://mega.nz/file/abc#key",
            "magnet:?xt=urn:btih:0000",
        ] {
            assert!(parse(url).is_none(), "should not parse: {url}");
        }
    }

    #[test]
    fn an_empty_passcode_is_no_passcode() {
        assert_eq!(share("https://pan.quark.cn/s/abc123?pwd=").passcode, "");
    }
}
