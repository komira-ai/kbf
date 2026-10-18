//! The `[update]` table of `kbf.toml`: where a release is pulled from, and the caps on one
//! fetch. Root's file, read by the installer only; an order or an offer names none of it.
//!
//! ```toml
//! [update]
//! source = "frontend"               # "frontend" (the default) or "url"
//! url = "https://mirror.example/r"  # only with source = "url"
//! fetch_timeout_s = 900             # the whole fetch of one component; 60 to 3600
//! max_component_bytes = 536870912   # 512 MiB; at most 1 GiB
//! min_protocol = 0                  # the operator floor: no release below it is taken
//! min_generation = 0                # the operator floor for an order's generation
//! ca_file = "/etc/kbf/ca.pem"       # trust this PEM instead of the system roots
//! ```
//!
//! Only this table is read: the file's other tables belong to `kbf` and are skipped. The
//! subset of TOML the table may use is a line per key, `key = "text"` or `key = 123`, and a
//! `#` comment after either; anything else inside the table is refused, so a typo cannot
//! quietly leave the default in force. A missing file, or a file with no `[update]` table,
//! is the default: the frontend as source.
//!
//! # The URL rules
//!
//! A `url` source is the base of an HTTPS file server that serves the release store's
//! layout, `<base>/files/<sha256>`. It must be `https`, have a host (a name or an address
//! in brackets) and an optional port, and carry no user information, no query and no
//! fragment; the path is plain characters. The fetch follows no redirect at all.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The shortest and longest `fetch_timeout_s`, and its default.
pub const TIMEOUT_MIN_S: u64 = 60;
pub const TIMEOUT_MAX_S: u64 = 3600;
pub const TIMEOUT_DEFAULT_S: u64 = 900;
/// The most `max_component_bytes` may be, and its default (the store's own cap).
pub const MAX_BYTES_HARD: u64 = 1 << 30;
pub const MAX_BYTES_DEFAULT: u64 = 512 << 20;
/// The biggest `kbf.toml` read.
const FILE_MAX: u64 = 1 << 20;

/// Where a release is pulled from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The frontend's `ReleaseService.Fetch` over mutual TLS.
    Frontend,
    /// A plain HTTPS file server.
    Url(ReleaseUrl),
}

/// Why `kbf.toml` or its `[update]` table is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigFault {
    #[error("kbf.toml: {0}")]
    File(String),
    #[error("[update] line {line}: {why}")]
    Line { line: usize, why: String },
    #[error("[update]: {0}")]
    Value(String),
    #[error("[update] url: {0}")]
    Url(#[from] UrlFault),
}

/// Why a string is not a release URL.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlFault {
    #[error("{0:?} is not an https URL")]
    NotHttps(String),
    #[error("{0:?} carries user information")]
    UserInfo(String),
    #[error("{0:?} carries a query or a fragment")]
    QueryOrFragment(String),
    #[error("{0:?} has no usable host and port")]
    Authority(String),
    #[error("{0:?} has characters a base URL does not (letters, digits and . _ ~ - / only)")]
    Path(String),
}

/// A validated base URL: `https://host[:port][/path]`, with no trailing slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseUrl(String);

impl ReleaseUrl {
    /// `text` as a release URL, or the rule it breaks.
    pub fn parse(text: &str) -> Result<ReleaseUrl, UrlFault> {
        let rest = text
            .strip_prefix("https://")
            .ok_or_else(|| UrlFault::NotHttps(text.to_owned()))?;
        if rest.contains(['?', '#']) {
            return Err(UrlFault::QueryOrFragment(text.to_owned()));
        }
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(UrlFault::UserInfo(text.to_owned()));
        }
        if !authority_ok(authority) {
            return Err(UrlFault::Authority(text.to_owned()));
        }
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-' | '/');
        if !path.chars().all(plain) || path.contains("//") || path.split('/').any(|p| p == "..") {
            return Err(UrlFault::Path(text.to_owned()));
        }
        Ok(ReleaseUrl(format!("https://{authority}{}", path.trim_end_matches('/'))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The URL of the blob whose sha256 is `sha256`.
    pub fn file(&self, sha256: &str) -> String {
        format!("{}/files/{sha256}", self.0)
    }
}

/// `host` or `host:port`, the host a DNS name or an address in brackets, the port 1 to 65535.
fn authority_ok(authority: &str) -> bool {
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let Some((addr, tail)) = inner.split_once(']') else {
                return false;
            };
            let hex = |c: char| c.is_ascii_hexdigit() || matches!(c, ':' | '.');
            if addr.is_empty() || !addr.chars().all(hex) {
                return false;
            }
            (addr, tail.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':');
    let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0));
    !host.is_empty() && host.chars().all(name) && port_ok
}

/// The `[update]` table, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateConfig {
    pub source: Source,
    /// The whole fetch of one component.
    pub fetch_timeout: Duration,
    pub max_component_bytes: u64,
    /// No release whose protocol is below this is taken.
    pub min_protocol: u64,
    /// No order whose generation is below this is taken (the order watcher applies it).
    pub min_generation: u64,
    /// A PEM file of the roots to trust for the fetch, instead of the system's.
    pub ca_file: Option<PathBuf>,
}

impl Default for UpdateConfig {
    fn default() -> UpdateConfig {
        UpdateConfig {
            source: Source::Frontend,
            fetch_timeout: Duration::from_secs(TIMEOUT_DEFAULT_S),
            max_component_bytes: MAX_BYTES_DEFAULT,
            min_protocol: 0,
            min_generation: 0,
            ca_file: None,
        }
    }
}

/// A value on the right of `=`, before any comment.
enum Value {
    Text(String),
    Number(u64),
}

fn value(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('"') {
        let Some((text, tail)) = rest.split_once('"') else {
            return Err("a string with no closing quote".to_owned());
        };
        if text.contains('\\') {
            return Err("a backslash in a string".to_owned());
        }
        let tail = tail.trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            return Err(format!("{tail:?} after the string"));
        }
        return Ok(Value::Text(text.to_owned()));
    }
    let digits = raw.split('#').next().unwrap_or("").trim();
    digits
        .parse::<u64>()
        .map(Value::Number)
        .map_err(|_| format!("{digits:?} is neither a \"string\" nor a whole number"))
}

impl UpdateConfig {
    /// The `[update]` table of `text`, a whole `kbf.toml`.
    pub fn parse(text: &str) -> Result<UpdateConfig, ConfigFault> {
        let mut config = UpdateConfig::default();
        let mut inside = false;
        let mut seen_table = false;
        let mut seen: Vec<String> = Vec::new();
        let mut url = None;
        let mut source = None;
        for (index, line) in text.lines().enumerate() {
            let line_no = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                inside = line.split('#').next().unwrap_or("").trim() == "[update]";
                if inside {
                    if seen_table {
                        return Err(ConfigFault::Line {
                            line: line_no,
                            why: "the [update] table twice".to_owned(),
                        });
                    }
                    seen_table = true;
                }
                continue;
            }
            if !inside {
                continue;
            }
            let at = |why: String| ConfigFault::Line { line: line_no, why };
            let Some((key, raw)) = line.split_once('=') else {
                return Err(at("not a key = value line".to_owned()));
            };
            let key = key.trim().to_owned();
            if seen.contains(&key) {
                return Err(at(format!("{key} twice")));
            }
            let parsed = value(raw).map_err(&at)?;
            match (key.as_str(), parsed) {
                ("source", Value::Text(s)) => source = Some(s),
                ("url", Value::Text(s)) => url = Some(s),
                ("ca_file", Value::Text(s)) => {
                    if !s.starts_with('/') {
                        return Err(at("ca_file is not an absolute path".to_owned()));
                    }
                    config.ca_file = Some(PathBuf::from(s));
                }
                ("fetch_timeout_s", Value::Number(n)) => {
                    if !(TIMEOUT_MIN_S..=TIMEOUT_MAX_S).contains(&n) {
                        return Err(ConfigFault::Value(format!(
                            "fetch_timeout_s is {n}; it is {TIMEOUT_MIN_S} to {TIMEOUT_MAX_S}"
                        )));
                    }
                    config.fetch_timeout = Duration::from_secs(n);
                }
                ("max_component_bytes", Value::Number(n)) => {
                    if n == 0 || n > MAX_BYTES_HARD {
                        return Err(ConfigFault::Value(format!(
                            "max_component_bytes is {n}; it is 1 to {MAX_BYTES_HARD}"
                        )));
                    }
                    config.max_component_bytes = n;
                }
                ("min_protocol", Value::Number(n)) => config.min_protocol = n,
                ("min_generation", Value::Number(n)) => config.min_generation = n,
                (other, _) => {
                    return Err(at(format!("{other} is not a key of [update], or has the wrong kind of value")));
                }
            }
            seen.push(key);
        }
        config.source = match (source.as_deref(), url) {
            (None | Some("frontend"), None) => Source::Frontend,
            (Some("frontend"), Some(_)) => {
                return Err(ConfigFault::Value(
                    "url is set but source is \"frontend\"".to_owned(),
                ));
            }
            (None, Some(_)) => {
                return Err(ConfigFault::Value("url is set but source is not".to_owned()));
            }
            (Some("url"), Some(u)) => Source::Url(ReleaseUrl::parse(&u)?),
            (Some("url"), None) => {
                return Err(ConfigFault::Value("source = \"url\" needs a url".to_owned()));
            }
            (Some(other), _) => {
                return Err(ConfigFault::Value(format!(
                    "source is {other:?}; it is \"frontend\" or \"url\""
                )));
            }
        };
        Ok(config)
    }

    /// The config in the file at `path`. A file that is not there is the default; one that is
    /// must be a regular file (no link followed) owned by `owner` and writable by no one else,
    /// since what it says decides where a root program connects.
    pub fn load(path: &Path, owner: u32) -> Result<UpdateConfig, ConfigFault> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(UpdateConfig::default());
            }
            Err(e) => return Err(ConfigFault::File(format!("{}: {e}", path.display()))),
        };
        let meta = file
            .metadata()
            .map_err(|e| ConfigFault::File(format!("{}: {e}", path.display())))?;
        if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err(ConfigFault::File(format!(
                "{} is not a regular file of uid {owner} that no one else can write",
                path.display()
            )));
        }
        let mut text = String::new();
        file.take(FILE_MAX)
            .read_to_string(&mut text)
            .map_err(|e| ConfigFault::File(format!("{}: {e}", path.display())))?;
        UpdateConfig::parse(&text)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
