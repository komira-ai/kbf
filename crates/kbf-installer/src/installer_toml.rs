//! `/etc/kbf/installer.toml`: the installer's own settings (docs/design/kbf-installer.md
//! section 3). `setup` is its only writer; the installer reads it and nothing else decides
//! where a release comes from: not an order, an offer, an environment variable or a flag
//! outside `setup`.
//!
//! The file is closed: an unknown key, a wrong type or an out-of-bounds value is refused, and
//! every problem found is reported, not only the first. Every key has a default, so an empty
//! file means a frontend source with the defaults below.
//!
//! [`InstallerToml::render`] writes the one canonical form (every key but the unset optional
//! ones, in a fixed order); the golden fixture `testdata/golden/installer.toml` is that form.

use std::path::PathBuf;

use serde::Deserialize;

/// The largest file read.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const CONFIRM_WINDOW_S: std::ops::RangeInclusive<u32> = 120..=1800;
pub const FETCH_TIMEOUT_S: std::ops::RangeInclusive<u32> = 60..=3600;
/// The store's own cap per component.
pub const MAX_COMPONENT_BYTES_CEILING: u64 = 1 << 30;
/// A day: the longest minimum gap between two activations.
pub const MIN_ACTIVATION_INTERVAL_CEILING_S: u32 = 86_400;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    release_source: Option<String>,
    release_url: Option<String>,
    source_refs: Option<Vec<String>>,
    confirm_window_s: Option<u32>,
    confirm_window_max_s: Option<u32>,
    min_generation: Option<u64>,
    min_protocol: Option<u32>,
    fetch_timeout_s: Option<u32>,
    max_component_bytes: Option<u64>,
    min_activation_interval_s: Option<u32>,
    node_cert: Option<String>,
    node_key: Option<String>,
    database: Option<String>,
}

/// Where releases are fetched from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The frontend's release store, over the worker listener with mutual TLS.
    Frontend,
    /// An https file server serving `files/<sha256>`: the base URL, without a trailing `/`.
    Url(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallerToml {
    pub source: Source,
    /// Source refs the installer accepts, narrowing the trust files' list; empty = no narrowing.
    pub source_refs: Vec<String>,
    pub confirm_window_s: u32,
    pub confirm_window_max_s: u32,
    pub min_generation: u64,
    pub min_protocol: u32,
    pub fetch_timeout_s: u32,
    pub max_component_bytes: u64,
    pub min_activation_interval_s: u32,
    /// The node identity the fetch child is lent, for the frontend source.
    pub node_cert: Option<PathBuf>,
    pub node_key: Option<PathBuf>,
    /// The state database of a frontend or all-in-one host: a `postgres://` URL without a
    /// password (the file is world-readable). `crate::database` checks and uses it.
    pub database: Option<String>,
}

impl Default for InstallerToml {
    fn default() -> Self {
        InstallerToml {
            source: Source::Frontend,
            source_refs: Vec::new(),
            confirm_window_s: 900,
            confirm_window_max_s: 1800,
            min_generation: 0,
            min_protocol: 0,
            fetch_timeout_s: 900,
            max_component_bytes: 512 << 20,
            min_activation_interval_s: 60,
            node_cert: None,
            node_key: None,
            database: None,
        }
    }
}

/// Why a file is not an `installer.toml`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("installer.toml is {0} bytes; at most {MAX_FILE_BYTES}")]
    TooLarge(usize),
    #[error("installer.toml: {0}")]
    Syntax(String),
    #[error("installer.toml:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

fn absolute(key: &str, value: &Option<String>, problems: &mut Vec<String>) -> Option<PathBuf> {
    let v = value.as_deref()?;
    let plain = v
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    if plain && v.starts_with('/') && v.split('/').skip(1).all(|c| !matches!(c, "" | "." | "..")) {
        Some(PathBuf::from(v))
    } else {
        problems.push(format!("{key} = {v:?}: must be an absolute path of plain characters with no empty, . or .. part"));
        None
    }
}

/// `https://host[:port][/path]` with no user information, query or fragment, lower-cased
/// host, no trailing `/`.
fn https_base(url: &str) -> Result<String, String> {
    let bad = |why: &str| format!("release_url = {url:?}: {why}");
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| bad("must start with https://"))?;
    if rest.contains('@') {
        return Err(bad("must not carry user information"));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(bad("must not carry a query or a fragment"));
    }
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let path = path.trim_end_matches('/');
    let path_ok = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._~/-".contains(&b))
        && !path.contains("//")
        && !path.split('/').any(|c| c == "." || c == "..");
    if !path_ok {
        return Err(bad(
            "the path holds a character or part that is not allowed",
        ));
    }
    let (host, port) = match authority.rfind(':').filter(|_| !authority.ends_with(']')) {
        Some(i) => (&authority[..i], Some(&authority[i + 1..])),
        None => (authority, None),
    };
    if let Some(p) = port
        && !(p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n != 0))
    {
        return Err(bad("the port is not 1 to 65535"));
    }
    let host = host.to_ascii_lowercase();
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        host.len() <= 253
            && host.split('.').all(|l| {
                (1..=63).contains(&l.len())
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
    };
    if !host_ok {
        return Err(bad("the host is not a DNS name or an address"));
    }
    let port = port.map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("https://{host}{port}{path}"))
}

fn ref_ok(r: &str) -> bool {
    r.starts_with("refs/")
        && r.len() <= 200
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        && !r.contains("//")
        && !r.contains("..")
        && !r.ends_with('/')
}

fn bounded<T: PartialOrd + std::fmt::Display + Copy>(
    key: &str,
    value: Option<T>,
    default: T,
    lo: T,
    hi: T,
    problems: &mut Vec<String>,
) -> T {
    let v = value.unwrap_or(default);
    if v < lo || v > hi {
        problems.push(format!("{key} = {v}: must be {lo} to {hi}"));
        return default;
    }
    v
}

impl InstallerToml {
    pub fn parse(text: &str) -> Result<InstallerToml, ConfigError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge(text.len()));
        }
        let raw: Raw = toml::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
        let d = InstallerToml::default();
        let mut problems = Vec::new();

        let source = match (
            raw.release_source.as_deref().unwrap_or("frontend"),
            &raw.release_url,
        ) {
            ("frontend", None) => Some(Source::Frontend),
            ("frontend", Some(_)) => {
                problems.push("release_url is only for release_source = \"url\"".to_owned());
                None
            }
            ("url", None) => {
                problems.push("release_source = \"url\" needs release_url".to_owned());
                None
            }
            ("url", Some(u)) => https_base(u)
                .map(Source::Url)
                .map_err(|e| problems.push(e))
                .ok(),
            (other, _) => {
                problems.push(format!(
                    "release_source = {other:?}: must be \"url\" or \"frontend\""
                ));
                None
            }
        };
        let refs = raw.source_refs.unwrap_or_default();
        for r in refs.iter().filter(|r| !ref_ok(r)) {
            problems.push(format!("source_refs: {r:?} is not a refs/... name"));
        }
        let confirm_window_s = bounded(
            "confirm_window_s",
            raw.confirm_window_s,
            d.confirm_window_s,
            *CONFIRM_WINDOW_S.start(),
            *CONFIRM_WINDOW_S.end(),
            &mut problems,
        );
        let confirm_window_max_s = bounded(
            "confirm_window_max_s",
            raw.confirm_window_max_s,
            d.confirm_window_max_s,
            *CONFIRM_WINDOW_S.start(),
            *CONFIRM_WINDOW_S.end(),
            &mut problems,
        );
        if confirm_window_s > confirm_window_max_s {
            problems.push(format!(
                "confirm_window_s = {confirm_window_s} is above confirm_window_max_s = {confirm_window_max_s}"
            ));
        }
        let fetch_timeout_s = bounded(
            "fetch_timeout_s",
            raw.fetch_timeout_s,
            d.fetch_timeout_s,
            *FETCH_TIMEOUT_S.start(),
            *FETCH_TIMEOUT_S.end(),
            &mut problems,
        );
        let max_component_bytes = bounded(
            "max_component_bytes",
            raw.max_component_bytes,
            d.max_component_bytes,
            1,
            MAX_COMPONENT_BYTES_CEILING,
            &mut problems,
        );
        let min_activation_interval_s = bounded(
            "min_activation_interval_s",
            raw.min_activation_interval_s,
            d.min_activation_interval_s,
            0,
            MIN_ACTIVATION_INTERVAL_CEILING_S,
            &mut problems,
        );
        let node_cert = absolute("node_cert", &raw.node_cert, &mut problems);
        let node_key = absolute("node_key", &raw.node_key, &mut problems);
        if node_cert.is_some() != node_key.is_some() && problems.is_empty() {
            problems.push("node_cert and node_key come together".to_owned());
        }
        if (raw.node_cert.is_some() || raw.node_key.is_some())
            && matches!(raw.release_source.as_deref(), Some("url"))
        {
            problems
                .push("node_cert and node_key are for release_source = \"frontend\"".to_owned());
        }
        match source {
            Some(source) if problems.is_empty() => Ok(InstallerToml {
                source,
                source_refs: refs,
                confirm_window_s,
                confirm_window_max_s,
                min_generation: raw.min_generation.unwrap_or(d.min_generation),
                min_protocol: raw.min_protocol.unwrap_or(d.min_protocol),
                fetch_timeout_s,
                max_component_bytes,
                min_activation_interval_s,
                node_cert,
                node_key,
                database: raw.database,
            }),
            _ => Err(ConfigError::Invalid(problems)),
        }
    }

    /// The canonical text: every key in a fixed order, the unset optional ones left out.
    pub fn render(&self) -> String {
        let mut out = String::new();
        match &self.source {
            Source::Frontend => out.push_str("release_source = \"frontend\"\n"),
            Source::Url(u) => out.push_str(&format!(
                "release_source = \"url\"\nrelease_url = \"{u}\"\n"
            )),
        }
        if !self.source_refs.is_empty() {
            let refs: Vec<_> = self
                .source_refs
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect();
            out.push_str(&format!("source_refs = [{}]\n", refs.join(", ")));
        }
        out.push_str(&format!("confirm_window_s = {}\n", self.confirm_window_s));
        out.push_str(&format!(
            "confirm_window_max_s = {}\n",
            self.confirm_window_max_s
        ));
        out.push_str(&format!("min_generation = {}\n", self.min_generation));
        out.push_str(&format!("min_protocol = {}\n", self.min_protocol));
        out.push_str(&format!("fetch_timeout_s = {}\n", self.fetch_timeout_s));
        out.push_str(&format!(
            "max_component_bytes = {}\n",
            self.max_component_bytes
        ));
        out.push_str(&format!(
            "min_activation_interval_s = {}\n",
            self.min_activation_interval_s
        ));
        if let Some(p) = &self.node_cert {
            out.push_str(&format!("node_cert = \"{}\"\n", p.display()));
        }
        if let Some(p) = &self.node_key {
            out.push_str(&format!("node_key = \"{}\"\n", p.display()));
        }
        if let Some(url) = &self.database {
            out.push_str(&format!("database = {url:?}\n"));
        }
        out
    }
}

#[cfg(test)]
#[path = "installer_toml_tests.rs"]
mod tests;
