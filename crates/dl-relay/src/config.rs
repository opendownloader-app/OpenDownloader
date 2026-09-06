//! Operator-supplied settings, and the defaults that apply when there are none.
//!
//! Every field is optional. A relay with no config file at all is a useful relay —
//! bound to loopback, refusing private hosts, capped — so that the first run of a
//! freshly cloned checkout is safe rather than merely convenient.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 20 GiB. Large enough for any single file a browser can realistically write,
/// small enough that a runaway response cannot fill a small server's pipe forever.
const DEFAULT_MAX_BYTES: u64 = 21_474_836_480;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Loopback by default. Exposing the relay to a network is a decision the
    /// operator makes deliberately, not one they make by forgetting to set this.
    pub bind: String,
    /// CORS origins. `["*"]` is the useful default because a self-hosted relay
    /// usually has exactly one client — the operator — and no session to steal.
    pub allowed_origins: Vec<String>,
    /// When non-empty, the only upstream hosts the relay will talk to. Empty means
    /// "anything the compliance policy already permits", which is not the same as
    /// "anything".
    pub allow_hosts: Vec<String>,
    /// Opt-in to relaying into private address space. Off by default because a relay
    /// with this on is a hole punched through the operator's network perimeter: every
    /// client of the relay can reach every host the relay can reach.
    pub allow_private_hosts: bool,
    /// Ceiling on a single relayed response, enforced both from `content-length` and
    /// again while streaming, because a chunked response declares no length.
    pub max_bytes: u64,
    /// Simultaneous upstream requests. The relay is a pipe, not a cache, so this is
    /// the only thing standing between it and its own bandwidth bill.
    pub max_concurrent: usize,
    /// Connect plus response-headers timeout. Deliberately **not** a whole-body
    /// timeout: a 20 GiB file over a slow link is the normal case here, not an abuse.
    pub timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8088".to_string(),
            allowed_origins: vec!["*".to_string()],
            allow_hosts: Vec::new(),
            allow_private_hosts: false,
            max_bytes: DEFAULT_MAX_BYTES,
            max_concurrent: 8,
            timeout_seconds: 30,
        }
    }
}

impl Config {
    /// Load from an explicit path, or from `dl-relay.toml` beside the binary.
    ///
    /// A path the operator named and that does not exist is an error; the implicit
    /// path simply not being there is not. Silently ignoring a typo'd `--config`
    /// would start a relay configured differently from the one that was asked for.
    pub fn load(explicit: Option<&str>) -> Result<Self, String> {
        match explicit {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read config {path}: {e}"))?;
                toml::from_str(&text).map_err(|e| format!("cannot parse config {path}: {e}"))
            }
            None => match default_path() {
                Some(path) if path.exists() => {
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
                    toml::from_str(&text)
                        .map_err(|e| format!("cannot parse config {}: {e}", path.display()))
                }
                _ => Ok(Self::default()),
            },
        }
    }

    /// True when `origin` may be answered by the CORS layer.
    pub fn allows_any_origin(&self) -> bool {
        self.allowed_origins.iter().any(|o| o == "*")
    }

    pub fn parse_bind(&self) -> Result<SocketAddr, String> {
        self.bind
            .parse()
            .map_err(|e| format!("bind is not a socket address ({}): {e}", self.bind))
    }
}

fn default_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(Path::new(exe.parent()?).join("dl-relay.toml"))
}

/// Printed verbatim at startup so an operator debugging a refusal can see the
/// settings that produced it rather than the ones they believe are in effect.
impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  bind                = {}", self.bind)?;
        writeln!(f, "  allowed_origins     = {:?}", self.allowed_origins)?;
        writeln!(
            f,
            "  allow_hosts         = {}",
            if self.allow_hosts.is_empty() {
                "[] (any host the policy permits)".to_string()
            } else {
                format!("{:?}", self.allow_hosts)
            }
        )?;
        writeln!(f, "  allow_private_hosts = {}", self.allow_private_hosts)?;
        writeln!(f, "  max_bytes           = {}", self.max_bytes)?;
        writeln!(f, "  max_concurrent      = {}", self.max_concurrent)?;
        write!(f, "  timeout_seconds     = {}", self.timeout_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_file_yields_the_documented_defaults() {
        let cfg: Config = toml::from_str("").expect("empty toml is a valid config");
        assert_eq!(cfg.bind, "127.0.0.1:8088");
        assert_eq!(cfg.max_bytes, 21_474_836_480);
        assert_eq!(cfg.max_concurrent, 8);
        assert_eq!(cfg.timeout_seconds, 30);
        assert!(!cfg.allow_private_hosts);
        assert!(cfg.allow_hosts.is_empty());
        assert!(cfg.allows_any_origin());
    }

    #[test]
    fn a_partial_config_overrides_only_what_it_names() {
        let cfg: Config = toml::from_str("max_concurrent = 2\nallow_hosts = [\"cdn.example.com\"]")
            .expect("partial toml is a valid config");
        assert_eq!(cfg.max_concurrent, 2);
        assert_eq!(cfg.allow_hosts, vec!["cdn.example.com".to_string()]);
        assert_eq!(cfg.bind, "127.0.0.1:8088");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_silent_no_op() {
        let err = toml::from_str::<Config>("max_bites = 10").unwrap_err();
        assert!(err.to_string().contains("max_bites"), "{err}");
    }

    #[test]
    fn an_explicit_origin_list_is_not_a_wildcard() {
        let cfg: Config = toml::from_str("allowed_origins = [\"https://app.example.com\"]")
            .expect("valid config");
        assert!(!cfg.allows_any_origin());
    }
}
