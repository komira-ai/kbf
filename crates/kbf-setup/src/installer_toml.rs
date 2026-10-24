//! `/etc/kbf/installer.toml`: the installer's own settings (docs/design/kbf-installer.md
//! section 3). `setup` is its only writer; the installer reads it and nothing else decides
//! where a release comes from: not an order, an offer, an environment variable or a flag
//! outside `setup`.
//!
//! The file is closed: an unknown key, a wrong type or an out-of-bounds value is refused, and
//! every problem found is reported, not only the first. Every key has a default, so an empty
//! file means a frontend source with the defaults below.
//!
//! `release_source` is one of three. `url`: a file server (`release_url`, and `release_ca` for
//! roots to trust instead of the system's). `frontend`: the frontend's release store, with the
//! node identity (`node_cert`, `node_key`) and, for a pull over the LAN, the snapshot of its
//! worker listener (`frontend_url`, `frontend_ca`, `frontend_ca_sha256`: the three come
//! together). `bundled`: nothing is fetched, the release is the one the package carries. Each
//! source takes the keys of its own and refuses the others'. Parsing checks the shape of every
//! value and reads no file: the CA files are looked at when they are used.
//!
//! `frontend_config` is an absolute path. Nothing reads it since two-slot supervision was
//! removed; it is accepted and written back so that the file of an earlier build, whose `setup`
//! wrote it on an all-in-one host behind the proxy, still parses and round-trips. `setup` writes
//! it no more and keeps the one a file has, and the tolerant reader takes it. It is how `setup`
//! knows that layout: `--mode=all-in-one` is refused on a host whose file names it
//! (`setup::plan::refuse_proxy_layout`).
//!
//! [`InstallerToml::parse`] is the closed reader, for whoever decides where a release comes
//! from (the fetcher) or rewrites the file (`setup`): it must understand all of it. A reader
//! that takes only some of the keys (the proxy verb) reads it with
//! [`InstallerToml::parse_tolerant`], which ignores every other key, whatever its value, so a
//! file a later build wrote does not stop it after a package downgrade.
//!
//! This module reads text and checks its shape; it does not look at who owns the file. The
//! read that holds the file to the trust files' rule (root's alone) is
//! `kbf_installer::helper::trusted_toml`, for the release fetcher. The `[proxy]` table and the
//! release-source keys (`release_source`, `release_url`, `source_refs`, the frontend snapshot,
//! `release_ca`, the node identity and the limits) are still read and checked here: hosts set
//! up by an earlier build carry them, and the fetcher and the stage policy still consume them
//! until the release-fetching chain is removed. `setup` keeps the ones a file has and writes
//! none of its own, except the node identity that goes with a `frontend` source.
//!
//! [`InstallerToml::render`] writes the one canonical form (every key but the unset optional
//! ones, in a fixed order); the golden fixtures `testdata/golden/installer*.toml` are that form.

use std::path::PathBuf;

#[cfg(feature = "proxy")]
use kbf_proxy::config::ProxyConfig;
use serde::Deserialize;

use crate::sha256_hex::Sha256Hex;

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
    frontend_url: Option<String>,
    frontend_ca: Option<String>,
    frontend_ca_sha256: Option<String>,
    release_ca: Option<String>,
    /// The proxy's table, checked by `kbf_proxy::config::ProxyConfig::from_value`; its own
    /// keys are closed the same way. A build without the `proxy` feature cannot check it and
    /// refuses a file that has one.
    proxy: Option<toml::Value>,
    database: Option<String>,
    frontend_config: Option<String>,
}

/// The keys [`InstallerToml::parse_tolerant`] reads; it drops every other before it looks.
const TOLERANT_KEYS: [&str; 5] = [
    "confirm_window_s",
    "confirm_window_max_s",
    "fetch_timeout_s",
    "proxy",
    "frontend_config",
];

/// The keys of the frontend snapshot, in the order `render` writes them.
const FRONTEND_KEYS: [&str; 3] = ["frontend_url", "frontend_ca", "frontend_ca_sha256"];

/// Where releases are fetched from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The frontend's release store, over the worker listener with mutual TLS.
    Frontend,
    /// An https file server serving `files/<sha256>`: the base URL, without a trailing `/`.
    Url(String),
    /// Nothing is fetched: the release is the one the package carries.
    Bundled,
}

impl Source {
    /// The word `release_source` holds for this source in the file, which `status.json` tells
    /// the frontend.
    pub fn word(&self) -> &'static str {
        match self {
            Source::Frontend => "frontend",
            Source::Url(_) => "url",
            Source::Bundled => "bundled",
        }
    }
}

/// The frontend's worker listener and the CA that authenticates it, as `setup` saw them: a
/// snapshot, taken once, because the root half cannot take its source from the service user's
/// directory. The CA is meant to be a copy that root owns; whether it is, and whether it still
/// has the digest, is looked at when it is used, not when the file is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendPull {
    /// `https://host[:port]` of the worker listener the frontend release source dials.
    pub url: String,
    /// The copy of the CA the listener's certificate chains to.
    pub ca: PathBuf,
    /// The sha256 of that copy.
    pub ca_sha256: Sha256Hex,
}

impl FrontendPull {
    /// The three values as a snapshot, each held to its rule. `names` are what the address, the
    /// CA path and the digest are called where they came from (the keys); every bad value is a
    /// problem pushed to `problems`, which names it and does not repeat its text.
    pub(crate) fn checked(
        names: [&str; 3],
        url: &str,
        ca: &str,
        sha256: &str,
        problems: &mut Vec<String>,
    ) -> Option<FrontendPull> {
        let url = https_authority_named(names[0], url)
            .map_err(|e| problems.push(e))
            .ok();
        let ca = absolute_path(names[1], ca)
            .map_err(|e| problems.push(e))
            .ok();
        let ca_sha256 = sha256_named(names[2], sha256)
            .map_err(|e| problems.push(e))
            .ok();
        Some(FrontendPull {
            url: url?,
            ca: ca?,
            ca_sha256: ca_sha256?,
        })
    }
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
    /// The roots a `url` source is checked against instead of the system's. Only for the `url`
    /// source.
    pub release_ca: Option<PathBuf>,
    /// The frontend's worker listener and its CA, for pulling over the LAN. Only for the
    /// `frontend` source.
    pub frontend: Option<FrontendPull>,
    /// The `[proxy]` table, read by `kbf-installer proxy`. No flag of `setup` writes it (the
    /// operator does, by hand); [`InstallerToml::render`] writes the whole table, so a file that
    /// is read and rendered keeps every key of it, the slot table of an earlier build included
    /// (the proxy refuses that table at its start, setup and the release fetcher do not).
    /// Only a build with the `proxy` feature has it: without, a file with a `[proxy]` table is
    /// not read at all.
    #[cfg(feature = "proxy")]
    pub proxy: Option<ProxyConfig>,
    /// The state database of a frontend or all-in-one host: a `postgres://` URL without a
    /// password (the file is world-readable). `crate::database` reads this file a second time,
    /// leniently (only this key, and it refuses a password); this field exists so that the
    /// closed reader here does not reject the key, and it has no other consumer.
    pub database: Option<String>,
    /// An absolute path that nothing reads since two-slot supervision was removed; it is accepted
    /// and written back so that the file of an earlier build (whose `setup` wrote it on an
    /// all-in-one host) still parses and round-trips. `setup` keeps the one a file has and
    /// writes none.
    pub frontend_config: Option<PathBuf>,
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
            release_ca: None,
            frontend: None,
            #[cfg(feature = "proxy")]
            proxy: None,
            database: None,
            frontend_config: None,
        }
    }
}

/// What a reader that fetches nothing takes from the file ([`InstallerToml::parse_tolerant`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tolerant {
    /// `confirm_window_s`, checked as the closed reader checks it.
    pub confirm_window_s: u32,
    /// `fetch_timeout_s`, checked as the closed reader checks it.
    pub fetch_timeout_s: u32,
    /// The `[proxy]` table, which is closed whoever reads it.
    #[cfg(feature = "proxy")]
    pub proxy: Option<ProxyConfig>,
    /// `frontend_config`, checked as the closed reader checks it.
    pub frontend_config: Option<PathBuf>,
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
    absolute_path(key, v).map_err(|e| problems.push(e)).ok()
}

/// `v` as an absolute path of plain characters with no empty, `.` or `..` part. `label` (the
/// key or flag the text came from) is in the refusal; the text is not.
pub(crate) fn absolute_path(label: &str, v: &str) -> Result<PathBuf, String> {
    let plain = v
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    if plain && v.starts_with('/') && v.split('/').skip(1).all(|c| !matches!(c, "" | "." | "..")) {
        Ok(PathBuf::from(v))
    } else {
        Err(format!(
            "{label}: must be an absolute path of plain characters with no empty, . or .. part"
        ))
    }
}

/// `text` as a sha256 digest, 64 lowercase hex characters. The refusal names `label` and does
/// not repeat the text.
pub(crate) fn sha256_named(label: &str, text: &str) -> Result<Sha256Hex, String> {
    Sha256Hex::try_from(text.to_owned())
        .map_err(|_| format!("{label}: must be 64 lowercase hex characters"))
}

/// `https://host[:port][/path]` with no user information, query or fragment, lower-cased
/// host, no trailing `/`.
pub fn https_base(url: &str) -> Result<String, String> {
    https_base_named("release_url =", url)
}

/// [`https_base`], with `label` (the key or flag the text came from) in each message.
pub(crate) fn https_base_named(label: &str, url: &str) -> Result<String, String> {
    // No message repeats the URL: a credential can be in any part of it, and the scheme
    // check runs first, so even a URL that is not https can hold one.
    let bad = |why: &str| format!("{label}: {why}");
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| bad("must start with https://"))?;
    // These two parts are where a credential goes (`user:password@`, `?token=`): the message
    // names the rule and never repeats the text.
    if rest.contains('@') {
        return Err(format!("{label}: must not carry user information"));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(format!("{label}: must not carry a query or a fragment"));
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

/// [`https_base_named`] for an address with nothing after the port: `https://host[:port]`.
pub(crate) fn https_authority_named(label: &str, url: &str) -> Result<String, String> {
    let base = https_base_named(label, url)?;
    if base["https://".len()..].contains('/') {
        return Err(format!(
            "{label}: must be https://host[:port] and nothing after it"
        ));
    }
    Ok(base)
}

/// The frontend snapshot of a file: the three keys together or none, each held to its rule.
fn frontend_group(
    url: Option<&str>,
    ca: Option<&str>,
    sha256: Option<&str>,
    problems: &mut Vec<String>,
) -> Option<FrontendPull> {
    match (url, ca, sha256) {
        (None, None, None) => None,
        (Some(url), Some(ca), Some(sha256)) => {
            FrontendPull::checked(FRONTEND_KEYS, url, ca, sha256, problems)
        }
        _ => {
            problems
                .push("frontend_url, frontend_ca and frontend_ca_sha256 come together".to_owned());
            None
        }
    }
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

/// A parser message without the text of the file in it: serde and the parser quote the value
/// or key they met (between backticks or quotes), and that text can be a password typed
/// into the wrong place. Everything from the first quote on is dropped.
pub(crate) fn redact(message: &str) -> String {
    let kept = message
        .find(['`', '"', '\''])
        .map_or(message, |at| &message[..at]);
    let kept = kept.trim_end_matches([' ', ',', ':']);
    format!("{kept} (the text is not shown)")
}

/// Where and what the parser objected to, without its quote of the offending line: that line
/// can be a `database` URL, and a password a person typed into it must not reach a log.
fn syntax_line(text: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let before = text.get(..span.start).unwrap_or_default();
            format!(
                "line {}: {}",
                before.matches('\n').count() + 1,
                redact(error.message())
            )
        }
        None => redact(error.message()),
    }
}

impl InstallerToml {
    /// The closed reader: every key of the file is known, every value is the right type and
    /// inside its bounds, and the keys of one source are not beside another's, or it is
    /// refused with every problem found.
    ///
    /// # Errors
    /// [`ConfigError`]: the file is too big, is not TOML of this shape, or holds a bad value.
    pub fn parse(text: &str) -> Result<InstallerToml, ConfigError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge(text.len()));
        }
        let raw: Raw =
            toml::from_str(text).map_err(|e| ConfigError::Syntax(syntax_line(text, &e)))?;
        InstallerToml::from_raw(raw)
    }

    /// The tolerant reader, for a reader that fetches nothing and so needs only the confirm
    /// window, the fetch timeout, the `[proxy]` table and `frontend_config` (`database` it
    /// reads on its own, leniently, as the proxy verb does): every other key is dropped before the
    /// file is looked at, so a key or a value this build has never heard of (a source a later
    /// build wrote) cannot stop it. What it does read is checked as [`InstallerToml::parse`]
    /// checks it, the `[proxy]` table stays closed, and a message repeats no text of the file.
    /// The file still has to be TOML and within the size cap.
    ///
    /// Whoever chooses where a release comes from must not use this: it would read the source
    /// of a later build as nothing.
    ///
    /// # Errors
    /// [`ConfigError`]: the file is too big or is not TOML, or a key this reads is wrong.
    pub fn parse_tolerant(text: &str) -> Result<Tolerant, ConfigError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge(text.len()));
        }
        let mut table: toml::Table =
            toml::from_str(text).map_err(|e| ConfigError::Syntax(syntax_line(text, &e)))?;
        table.retain(|key, _| TOLERANT_KEYS.contains(&key));
        let raw: Raw = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Syntax(redact(e.message())))?;
        let file = InstallerToml::from_raw(raw)?;
        Ok(Tolerant {
            confirm_window_s: file.confirm_window_s,
            fetch_timeout_s: file.fetch_timeout_s,
            #[cfg(feature = "proxy")]
            proxy: file.proxy,
            frontend_config: file.frontend_config,
        })
    }

    fn from_raw(raw: Raw) -> Result<InstallerToml, ConfigError> {
        let d = InstallerToml::default();
        let mut problems = Vec::new();

        let declared = raw.release_source.as_deref().unwrap_or("frontend");
        let source = match (declared, &raw.release_url) {
            ("frontend", None) => Some(Source::Frontend),
            ("bundled", None) => Some(Source::Bundled),
            ("frontend" | "bundled", Some(_)) => {
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
            (_, _) => {
                problems.push(
                    "release_source: must be \"url\", \"frontend\" or \"bundled\"".to_owned(),
                );
                None
            }
        };
        // The keys of one source are not beside another's: a bundled host fetches nothing, and
        // a key that names where to fetch from beside it says two things.
        let not_frontend = matches!(declared, "url" | "bundled");
        let not_url = matches!(declared, "frontend" | "bundled");
        let refs = raw.source_refs.unwrap_or_default();
        for _ in refs.iter().filter(|r| !ref_ok(r)) {
            problems.push("source_refs: an entry is not a refs/... name".to_owned());
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
        let frontend = frontend_group(
            raw.frontend_url.as_deref(),
            raw.frontend_ca.as_deref(),
            raw.frontend_ca_sha256.as_deref(),
            &mut problems,
        );
        let release_ca = absolute("release_ca", &raw.release_ca, &mut problems);
        let frontend_config = absolute("frontend_config", &raw.frontend_config, &mut problems);
        let identity_given = raw.node_cert.is_some() || raw.node_key.is_some();
        let snapshot_given = raw.frontend_url.is_some()
            || raw.frontend_ca.is_some()
            || raw.frontend_ca_sha256.is_some();
        if not_frontend && identity_given {
            problems
                .push("node_cert and node_key are for release_source = \"frontend\"".to_owned());
        }
        if not_frontend && snapshot_given {
            problems.push(
                "frontend_url, frontend_ca and frontend_ca_sha256 are for release_source = \"frontend\"".to_owned(),
            );
        }
        if not_url && raw.release_ca.is_some() {
            problems.push("release_ca is for release_source = \"url\"".to_owned());
        }
        #[cfg(feature = "proxy")]
        let proxy = raw.proxy.and_then(|table| {
            ProxyConfig::from_value(table)
                .map_err(|e| problems.push(format!("proxy: {e}")))
                .ok()
        });
        // A build without the proxy cannot check the table, and a rewrite would drop it.
        #[cfg(not(feature = "proxy"))]
        {
            if raw.proxy.is_some() {
                problems.push(
                    "proxy: kbf-installer setup keeps this table; this setup does not read it"
                        .to_owned(),
                );
            }
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
                release_ca,
                frontend,
                #[cfg(feature = "proxy")]
                proxy,
                database: raw.database,
                frontend_config,
            }),
            _ => Err(ConfigError::Invalid(problems)),
        }
    }

    /// The canonical text: every key in a fixed order, the unset optional ones left out. With the
    /// `proxy` feature the `[proxy]` table, when there is one, comes last with every key it has,
    /// defaults included, so what an operator hand-edited in it survives a rewrite; a build
    /// without the feature has no table to write (it refuses a file that has one).
    pub fn render(&self) -> String {
        let mut out = String::new();
        match &self.source {
            Source::Frontend => out.push_str("release_source = \"frontend\"\n"),
            Source::Url(u) => out.push_str(&format!(
                "release_source = \"url\"\nrelease_url = \"{u}\"\n"
            )),
            Source::Bundled => out.push_str("release_source = \"bundled\"\n"),
        }
        if let Some(p) = &self.release_ca {
            out.push_str(&format!("release_ca = \"{}\"\n", p.display()));
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
        if let Some(pull) = &self.frontend {
            let digest = String::from(pull.ca_sha256.clone());
            out.push_str(&format!("frontend_url = \"{}\"\n", pull.url));
            out.push_str(&format!("frontend_ca = \"{}\"\n", pull.ca.display()));
            out.push_str(&format!("frontend_ca_sha256 = \"{digest}\"\n"));
        }
        if let Some(url) = &self.database {
            out.push_str(&format!("database = {}\n", toml_string(url)));
        }
        if let Some(p) = &self.frontend_config {
            out.push_str(&format!("frontend_config = \"{}\"\n", p.display()));
        }
        #[cfg(feature = "proxy")]
        {
            if let Some(proxy) = &self.proxy {
                render_proxy(proxy, &mut out);
            }
        }
        out
    }

    /// The address the `[proxy]` table moves the public socket to, when it gives one.
    #[cfg(feature = "proxy")]
    pub fn proxy_listen(&self) -> Option<std::net::SocketAddr> {
        self.proxy.as_ref().and_then(|p| p.listen)
    }

    /// Without the `proxy` feature there is no table, so no address.
    #[cfg(not(feature = "proxy"))]
    pub fn proxy_listen(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Whether the file has a `[proxy]` table.
    #[cfg(feature = "proxy")]
    pub fn has_proxy_table(&self) -> bool {
        self.proxy.is_some()
    }

    /// Without the `proxy` feature a file with a `[proxy]` table is not read, so none has one.
    #[cfg(not(feature = "proxy"))]
    pub fn has_proxy_table(&self) -> bool {
        false
    }
}

/// The `[proxy]` table, every key, in the order of `kbf_proxy::config::ProxyConfig`; its
/// sub-tables follow its plain keys. Paths go through [`toml_string`].
#[cfg(feature = "proxy")]
fn render_proxy(p: &ProxyConfig, out: &mut String) {
    let path = |p: &std::path::Path| toml_string(&p.display().to_string());
    out.push_str("\n[proxy]\n");
    if let Some(listen) = p.listen {
        out.push_str(&format!("listen = \"{listen}\"\n"));
    }
    out.push_str(&format!(
        "operator_token_file = {}\n",
        path(&p.operator_token_file)
    ));
    if let Some(file) = &p.server_cert_file {
        out.push_str(&format!("server_cert_file = {}\n", path(file)));
    }
    if let Some(file) = &p.server_key_file {
        out.push_str(&format!("server_key_file = {}\n", path(file)));
    }
    out.push_str(&format!("client_ca_file = {}\n", path(&p.client_ca_file)));
    out.push_str(&format!("buck2_auth = \"{}\"\n", p.buck2_auth.name()));
    if let Some(file) = &p.buck2_token_file {
        out.push_str(&format!("buck2_token_file = {}\n", path(file)));
    }
    if let Some(file) = &p.remote_operator_file {
        out.push_str(&format!("remote_operator_file = {}\n", path(file)));
    }
    out.push_str(&format!("proxy_auth_file = {}\n", path(&p.proxy_auth_file)));
    out.push_str(&format!("state_dir = {}\n", path(&p.state_dir)));
    out.push_str(&format!("hold_max_s = {}\n", p.hold_max_s));
    out.push_str(&format!("ui = {}\n", p.ui));
    if let Some(admin) = p.admin_listen {
        out.push_str(&format!("admin_listen = \"{admin}\"\n"));
    }
    for cert in &p.server_cert {
        out.push_str("\n[[proxy.server_cert]]\n");
        let names: Vec<String> = cert.names.iter().map(|n| toml_string(n)).collect();
        out.push_str(&format!("names = [{}]\n", names.join(", ")));
        out.push_str(&format!("cert_file = {}\n", path(&cert.cert_file)));
        out.push_str(&format!("key_file = {}\n", path(&cert.key_file)));
        if cert.default {
            out.push_str("default = true\n");
        }
    }
    if let Some(upstream) = p.upstream {
        out.push_str("\n[proxy.upstream]\n");
        out.push_str(&format!("base_port = {}\n", upstream.base_port));
    }
    for frontend in &p.frontend {
        out.push_str("\n[[proxy.frontend]]\n");
        out.push_str(&format!("name = {}\n", toml_string(&frontend.name)));
        out.push_str(&format!("base_port = {}\n", frontend.base_port));
    }
    let l = &p.limits;
    out.push_str("\n[proxy.limits]\n");
    for (key, value) in [
        ("pre_stream_timeout_s", l.pre_stream_timeout_s),
        ("max_connections", l.max_connections as u64),
        ("max_connections_per_ip", l.max_connections_per_ip as u64),
        ("max_streams", l.max_streams as u64),
        ("memory_budget_mib", l.memory_budget_mib),
        ("max_header_list_bytes", u64::from(l.max_header_list_bytes)),
        ("keepalive_interval_s", l.keepalive_interval_s),
        ("keepalive_ack_s", l.keepalive_ack_s),
    ] {
        out.push_str(&format!("{key} = {value}\n"));
    }
}

/// `text` as a TOML basic string: Rust's `{:?}` is not TOML (it writes `\'` and `\u{..}`).
pub(crate) fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// These tests build `InstallerToml` with its `[proxy]` table, so they run with the `proxy`
// feature; the build without it has `no_proxy_tests`.
#[cfg(all(test, feature = "proxy"))]
#[path = "installer_toml_tests.rs"]
mod tests;

#[cfg(all(test, feature = "proxy"))]
#[path = "installer_toml_release_tests.rs"]
mod release_tests;

#[cfg(all(test, not(feature = "proxy")))]
#[path = "installer_toml_no_proxy_tests.rs"]
mod no_proxy_tests;

#[cfg(all(test, feature = "proxy"))]
#[path = "installer_toml_pool_tests.rs"]
mod pool_tests;
