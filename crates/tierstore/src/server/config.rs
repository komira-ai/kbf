//! The configuration file of the `tierstore` binary: `--config <path>`, a TOML file that holds
//! what the flags that describe the data path hold, in named tables.
//!
//! The file is read into the same [`Cli`] the flags make, so a server started from a file runs
//! the code of one started from the flags and cannot differ from it. Defaults are the flags'
//! defaults (the tests check that a file that gives the least a start needs and the flags that
//! give the least make equal settings).
//!
//! There are two shapes, told apart by the tables the file has:
//!
//! - a storage node: `[hot_store]` and `[cold_store]`, as the flags are;
//! - a build node: `[local_cache]` and `[cold_store]` and no `[hot_store]`, a read cache in
//!   front of one upstream (see [`config_cache`](super::config_cache) and
//!   [`local_cache`](super::local_cache)). It has no flag form.
//!
//! The tables:
//!
//! - `[server]`: the address, the id, the bucket and region, the key files, the limits on
//!   requests and bodies, and the start-up self-test's wait.
//! - `[hot_store]`: the hot tier and what only it has: its budget and water marks, the journal,
//!   the reconciler and its lease.
//! - `[cold_store]`: the cold tier, and the start-up check it may skip.
//! - `[local_cache]`: the cache directory, its budget and marks (build node only).
//! - `[oci]`: the read-only OCI front, left out when there is none: `listen` (needed in the
//!   table), the certificate and key files, the key prefix and the connection limits. They
//!   follow the rules of the `--oci-*` flags. Both shapes take it: a build node serves the
//!   registry from its cache.
//! - `[[policy]]`: the per-prefix settings. On a build node `cache = true` keeps the keys under
//!   the prefix in the cache; on a storage node a policy is read and checked and has no
//!   effect, and `cache = true` is refused.
//!
//! The three tables together are refused (the hot store is already local disk). Any other table
//! or key is refused, and so is a key that holds a secret: keys and credentials are paths of
//! files (a key ending in `_file` or `_files`).
//!
//! Per-run knobs are flags only: `--config` and the two parts of the drain (see [`PER_RUN`]).
//! [`KEYS`] says which key each other flag has; the tests walk the flags of [`Cli`] against it,
//! so a flag added without a key fails them.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::cli::{
    Cli, DEFAULT_MAX_CONCURRENT_PUTS, DEFAULT_MAX_REQUEST_BYTES, DEFAULT_PUT_MIN_BYTES_PER_SEC,
    DEFAULT_PUT_PERMIT_WAIT, DEFAULT_REGION, SkippedCheck, parse_duration, parse_skip,
};
use super::config_cache::{CacheSettings, LocalCacheTable};
use super::front::FrontSettings;
use super::listen::{Addrs, has_duplicate};
use super::lock::check_lock_path;
use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX};

/// The flags that stay flags: the file is for the data path, and these are per run.
pub(super) const PER_RUN: &[&str] = &[
    "config",
    "print-config",
    "check-config",
    "capabilities",
    "drain-lb-wait",
    "drain-deadline",
];

/// The key of each flag that the file holds: the flag's name without `--`, then the keys that
/// stand for it, the first of them the one an error message names in its place. A tier flag
/// (`--hot`) has a key for each part of its spec.
pub(super) const KEYS: &[(&str, &[&str])] = &[
    ("listen", &["server.listen"]),
    ("proxy-id", &["server.proxy_id"]),
    (
        "hot",
        &["hot_store.kind", "hot_store.path", "hot_store.bucket"],
    ),
    (
        "cold",
        &["cold_store.kind", "cold_store.path", "cold_store.bucket"],
    ),
    ("hot-endpoint", &["hot_store.endpoint"]),
    ("hot-region", &["hot_store.region"]),
    ("hot-credentials-file", &["hot_store.credentials_file"]),
    ("cold-endpoint", &["cold_store.endpoint"]),
    ("cold-region", &["cold_store.region"]),
    ("cold-credentials-file", &["cold_store.credentials_file"]),
    ("s3-request-timeout", &["server.s3_request_timeout"]),
    ("hot-bytes", &["hot_store.budget"]),
    ("high-water", &["hot_store.high_water"]),
    ("low-water", &["hot_store.low_water"]),
    ("critical", &["hot_store.critical"]),
    ("bucket", &["server.bucket"]),
    ("region", &["server.region"]),
    ("access-key-file", &["server.access_key_file"]),
    ("read-only-key-file", &["server.read_only_key_files"]),
    ("presign-key-file", &["server.presign_key_files"]),
    ("gc-key-file", &["server.gc_key_files"]),
    ("scoped-key-file", &["server.scoped_key_files"]),
    ("presign-max-expires", &["server.presign_max_expires"]),
    ("journal-linger", &["hot_store.journal_linger"]),
    ("journal-retention", &["hot_store.journal_retention"]),
    ("copy-parallel", &["hot_store.copy_parallel"]),
    ("lease-ttl", &["hot_store.lease_ttl"]),
    ("reconcile-interval", &["hot_store.reconcile_interval"]),
    ("lock-file", &["hot_store.lock_file"]),
    ("usage-stale-after", &["hot_store.usage_stale_after"]),
    ("max-request-bytes", &["server.max_request_bytes"]),
    ("max-concurrent-puts", &["server.max_concurrent_puts"]),
    ("put-permit-wait", &["server.put_permit_wait"]),
    ("put-min-bytes-per-sec", &["server.put_min_bytes_per_sec"]),
    ("put-rate-grace", &["server.put_rate_grace"]),
    ("body-idle-timeout", &["server.body_idle_timeout"]),
    ("skip-backend-check", &["cold_store.skip_checks"]),
    ("backend-check-wait", &["server.backend_check_wait"]),
    ("oci-listen", &["oci.listen"]),
    // The names end in `_file`, as every key that holds a path to a secret does.
    ("oci-tls-cert", &["oci.tls_cert_file"]),
    ("oci-tls-key", &["oci.tls_key_file"]),
    ("oci-prefix", &["oci.prefix"]),
    ("oci-max-connections", &["oci.max_connections"]),
    ("oci-connection-lifetime", &["oci.connection_lifetime"]),
];

/// The default of `--drain-lb-wait`, which a start from a file takes when the flag is not given.
pub(super) const DEFAULT_DRAIN_LB_WAIT: Duration = Duration::from_secs(10);

/// The default of `--drain-deadline`.
pub(super) const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// The prefixes of keys that tierstore keeps for itself: no policy may name them.
const RESERVED: [&str; 2] = [JOURNAL_PREFIX, TIERING_PREFIX];

/// Why a start from the file `--config` names, or `--print-config`, is refused. Every one of
/// them but an unreadable file is a start that a restart cannot mend; all exit with status 2.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("reading the config file {}: {source}", .path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: std::io::Error,
    },
    /// The file is not TOML, or has a key or a table it must not (the message names it), or lacks
    /// one it must, or a value of the wrong type. The message holds the place and the key, never
    /// the line or the value: the line may hold a secret, and the message goes to a journal.
    #[error("line {line}, column {column}: {what}")]
    Syntax {
        /// The line of the fault, from 1.
        line: usize,
        /// The column of the fault, from 1.
        column: usize,
        /// What is wrong, without the value.
        what: String,
    },
    /// A key that holds a secret.
    #[error(
        "{key}: a secret is never in the file; give the path of the file that holds it (a key whose name ends in _file)"
    )]
    InlineSecret {
        /// The key, with its table.
        key: String,
    },
    /// A key whose value, or whose absence, the settings cannot be made of.
    #[error("{key}: {why}")]
    Invalid {
        /// The key, with its table.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// Flags that describe the data path were given with the file that holds them.
    #[error(
        "--config cannot be given with {}: the file holds what they hold (only --drain-lb-wait and --drain-deadline go with it)",
        .flags.join(", ")
    )]
    Conflict {
        /// The flags, with their `--`.
        flags: Vec<String>,
    },
    /// `--print-config` writes the file for flags; it has no use with a file.
    #[error("--print-config writes the file for the flags given; do not give it with --config")]
    PrintWithConfig,
    /// `--check-config` checks the file `--config` names; there is nothing to check without it.
    #[error("--check-config checks the file that --config names; give --config")]
    CheckWithoutConfig,
    /// The settings could not be written as TOML.
    #[error("--print-config: {0}")]
    Print(#[from] toml::ser::Error),
    /// A refusal inside the file `path`.
    #[error("config file {}: {source}", .path.display())]
    In {
        /// The file.
        path: PathBuf,
        /// What is wrong in it.
        source: Box<ConfigError>,
    },
}

/// The knobs of one run, which are flags even when the rest is in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PerRun {
    /// `--drain-lb-wait`.
    pub drain_lb_wait: Duration,
    /// `--drain-deadline`.
    pub drain_deadline: Duration,
}

/// A duration in the file: a string such as `250ms` or `15m`, as the flags have it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Dur(pub Duration);

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(DurVisitor)
    }
}

struct DurVisitor;

impl Visitor<'_> for DurVisitor {
    type Value = Dur;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a duration such as \"15m\" (a number and ms, s, m or h)")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Dur, E> {
        match parse_duration(text) {
            Ok(duration) => Ok(Dur(duration)),
            // An invalid value, not a custom message and not the reason of the flag (which holds
            // the text): the refusal of a file names the key and the kind of value it takes and
            // never shows what was written (see `syntax`).
            Err(_) => Err(E::invalid_value(de::Unexpected::Str(text), &self)),
        }
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(self.0))
    }
}

/// The duration as the largest of `h`, `m`, `s` and `ms` that holds it in whole numbers, such
/// that [`parse_duration`] reads the text back to the same duration.
pub(super) fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis == 0 {
        return "0s".to_owned();
    }
    let units = [(3_600_000, "h"), (60_000, "m"), (1000, "s")];
    for (size, unit) in units {
        if millis.is_multiple_of(size) {
            return format!("{}{unit}", millis / size);
        }
    }
    format!("{millis}ms")
}

/// A number of bytes in the file: an integer, or a string of a number and `B`, `KiB`, `MiB`,
/// `GiB` or `TiB`, such as `500GiB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Size(pub u64);

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SizeVisitor)
    }
}

struct SizeVisitor;

impl Visitor<'_> for SizeVisitor {
    type Value = Size;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number of bytes, or a string such as \"500GiB\"")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Size, E> {
        match u64::try_from(value) {
            Ok(bytes) => Ok(Size(bytes)),
            Err(_) => Err(E::custom("a size is not negative")),
        }
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Size, E> {
        parse_size(text).map(Size).map_err(E::custom)
    }
}

/// A size: a whole number and `B`, `KiB`, `MiB`, `GiB` or `TiB`.
pub(super) fn parse_size(text: &str) -> Result<u64, String> {
    let bad = || "not a size; use a number and B, KiB, MiB, GiB or TiB, as in 500GiB".to_owned();
    let unit_at = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (digits, unit) = text.split_at(unit_at);
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let shift = match unit {
        "B" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        "TiB" => 40,
        _ => return Err(bad()),
    };
    count.checked_mul(1 << shift).ok_or_else(bad)
}

/// What a tier is made of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    /// Objects kept in the process only.
    #[default]
    Memory,
    /// A directory of the local file system: needs `path`.
    Dir,
    /// A bucket of an S3-compatible store: needs `bucket`, `endpoint` and `credentials_file`.
    S3,
}

/// The whole file.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct File {
    pub server: ServerTable,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_store: Option<TierTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_store: Option<TierTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci: Option<OciTable>,
    /// The cache of a build node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_cache: Option<LocalCacheTable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy: Vec<Policy>,
}

/// `[server]`: what the process is, and its limits. A key that is left out has the default of
/// the flag it stands for.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServerTable {
    /// One address, or a list of them.
    pub listen: Addrs,
    pub proxy_id: String,
    pub bucket: String,
    pub region: Option<String>,
    pub access_key_file: PathBuf,
    #[serde(default)]
    pub read_only_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub presign_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub gc_key_files: Vec<PathBuf>,
    #[serde(default)]
    pub scoped_key_files: Vec<PathBuf>,
    pub presign_max_expires: Option<Dur>,
    pub max_request_bytes: Option<u64>,
    pub max_concurrent_puts: Option<u32>,
    pub put_permit_wait: Option<Dur>,
    pub put_min_bytes_per_sec: Option<u64>,
    pub put_rate_grace: Option<Dur>,
    pub body_idle_timeout: Option<Dur>,
    pub s3_request_timeout: Option<Dur>,
    pub backend_check_wait: Option<Dur>,
}

/// `[hot_store]` or `[cold_store]`: the tier and how to reach it, and the keys only one of the
/// two has (the others are refused in the other).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TierTable {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<PathBuf>,
    // Only `[hot_store]` has these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_water: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_water: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_parallel: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile_interval: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_stale_after: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_linger: Option<Dur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_retention: Option<Dur>,
    // Only `[cold_store]` has these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_checks: Option<Vec<String>>,
    /// Only the `[cold_store]` of a build node: the upstream, as a list of one in place of
    /// `endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<String>>,
}

/// `[oci]`: the read-only OCI front. The table is the front: `listen` is needed in it, and the
/// other keys are refused without it, as the `--oci-*` flags are.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciTable {
    /// One address, or a list of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<Addrs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_lifetime: Option<Dur>,
}

/// `[[policy]]`: what is done with the keys under a prefix.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    /// The keys it is about: empty for all the rest, or ending in `/`.
    pub prefix: String,
    /// Whether to keep the keys under the prefix in the local cache. Only a build node has one.
    #[serde(default)]
    pub cache: bool,
}

pub(super) fn invalid(key: &str, why: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        why: why.into(),
    }
}

/// The keys of `table` that are in `keys` and given, refused with `why`.
pub(super) fn refuse(table: &str, why: &str, keys: &[(&str, bool)]) -> Result<(), ConfigError> {
    match keys.iter().find(|(_, given)| *given) {
        Some((key, _)) => Err(invalid(&format!("{table}.{key}"), why)),
        None => Ok(()),
    }
}

/// The value of a key that must be given.
pub(super) fn require<T>(table: &str, key: &str, value: Option<T>) -> Result<T, ConfigError> {
    match value {
        Some(value) => Ok(value),
        None => Err(invalid(&format!("{table}.{key}"), "it is needed")),
    }
}

/// `value` if it is between `low` and `high`, both included.
pub(super) fn within<T: PartialOrd + fmt::Display>(
    table: &str,
    key: &str,
    value: T,
    low: T,
    high: T,
) -> Result<T, ConfigError> {
    if value < low || value > high {
        let why = format!("{value} is not between {low} and {high}");
        return Err(invalid(&format!("{table}.{key}"), why));
    }
    Ok(value)
}

/// The duration of a key, or the default of its flag.
pub(super) fn duration(value: Option<Dur>, default: Duration) -> Duration {
    value.map_or(default, |dur| dur.0)
}

/// What a tier table says about the tier: the spec of `--hot` or `--cold`, and the endpoint,
/// region and credentials file of its S3 flags.
#[derive(Debug)]
pub(super) struct TierParts {
    pub spec: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub credentials_file: Option<PathBuf>,
}

/// The tier `table` names, if its keys fit its kind.
pub(super) fn tier(name: &str, table: &TierTable) -> Result<TierParts, ConfigError> {
    let s3_only = [
        ("endpoint", table.endpoint.is_some()),
        ("region", table.region.is_some()),
        ("credentials_file", table.credentials_file.is_some()),
    ];
    let bucket = ("bucket", table.bucket.is_some());
    let path = ("path", table.path.is_some());
    let spec = match (table.kind, &table.path, &table.bucket) {
        (Kind::Memory, ..) => {
            let keys = [path, bucket, s3_only[0], s3_only[1], s3_only[2]];
            refuse(name, "not for kind \"memory\"", &keys)?;
            "memory".to_owned()
        }
        (Kind::Dir, path_value, _) => {
            let keys = [bucket, s3_only[0], s3_only[1], s3_only[2]];
            refuse(name, "not for kind \"dir\"", &keys)?;
            format!(
                "dir:{}",
                require(name, "path", path_value.as_ref())?.display()
            )
        }
        (Kind::S3, _, bucket_value) => {
            refuse(name, "not for kind \"s3\"", &[path])?;
            let bucket = require(name, "bucket", bucket_value.as_ref())?;
            require(name, "endpoint", table.endpoint.as_ref())?;
            require(name, "credentials_file", table.credentials_file.as_ref())?;
            format!("s3:{bucket}")
        }
    };
    Ok(TierParts {
        spec,
        endpoint: table.endpoint.clone(),
        region: table.region.clone(),
        credentials_file: table.credentials_file.clone(),
    })
}

/// Whether a key's name says it holds a secret: it has a word of one, or is a key itself and
/// not the path of a file. A hint for a message that says what to do, not a gate: what keeps a
/// secret out of the file is that an unknown key is refused, and the name of a known key that
/// takes a path (`credentials_file`) cannot tell a path from a secret by its value. The messages
/// of the file never repeat a value, so a secret that is given anyway is not echoed.
fn looks_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_file") || key.ends_with("_files") {
        return false;
    }
    let words = [
        "secret",
        "password",
        "passphrase",
        "token",
        "credentials",
        "apikey",
        "psk",
    ];
    words.iter().any(|word| key.contains(word))
        || key == "key"
        || key.starts_with("access_key")
        || key.ends_with("_key")
}

/// The keys of `[local_cache]` that are in no flag, and so not in [`KEYS`].
const LOCAL_CACHE_KEYS: &[&str] = &[
    "path",
    "layout",
    "max_concurrent_fills",
    "max_concurrent_keeps",
    "keep_wait",
    "keep_deadline",
    "write_through",
    "whole_fill_limit",
    "scrub_rate",
];

/// Whether `key` is a name some table of the file holds: the last part of a key in [`KEYS`], a
/// `[local_cache]` key or a `[[policy]]` key.
fn known_key(key: &str) -> bool {
    matches!(key, "prefix" | "cache")
        || LOCAL_CACHE_KEYS.contains(&key)
        || KEYS
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .any(|name| name.rsplit('.').next() == Some(key))
}

/// The fault `error` found in `text`, as a [`ConfigError::Syntax`] that does not repeat the line
/// or the value (the display of the `toml` error does: it shows the line, and its messages for a
/// value of the wrong type show the value).
fn syntax(text: &str, error: &toml::de::Error) -> ConfigError {
    let at = error.span().map_or(0, |span| span.start).min(text.len());
    let before = &text.as_bytes()[..at];
    let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = before
        .iter()
        .rev()
        .take_while(|byte| **byte != b'\n')
        .count()
        + 1;
    let (key, value) = match text.lines().nth(line - 1).and_then(|l| l.split_once('=')) {
        Some((key, value)) => (Some(key.trim()), value.trim().trim_matches('"')),
        None => (None, ""),
    };
    // Only a name the file can hold is shown as the key: inside a multi-line string the text before
    // an `=` is part of a value, which may be a pasted secret.
    let key = key.filter(|key| known_key(key));
    let message = error.message();
    let shows_value = [
        "invalid type",
        "invalid value",
        "invalid length",
        "unknown variant",
    ];
    let what = if shows_value.iter().any(|start| message.starts_with(start)) {
        let expected = message
            .rsplit_once(", expected ")
            .map_or("another value", |(_, expected)| expected);
        let key = key.unwrap_or("a value");
        format!("{key}: not a value this key takes (it is not shown); expected {expected}")
    } else {
        let message = if value.is_empty() {
            message.to_owned()
        } else {
            message.replace(value, "(value)")
        };
        // A value's own parse error (a size, a duration, an address) does not name its key.
        match key {
            Some(key) => format!("{key}: {message}"),
            None => message,
        }
    };
    ConfigError::Syntax { line, column, what }
}

/// Refuses the first key of `table` (and of the tables inside it) that looks like a secret.
fn find_secret(path: &str, table: &toml::Table) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if looks_secret(key) {
            return Err(ConfigError::InlineSecret { key: here });
        }
        match value {
            toml::Value::Table(inner) => find_secret(&here, inner)?,
            toml::Value::Array(items) => {
                for inner in items.iter().filter_map(toml::Value::as_table) {
                    find_secret(&here, inner)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuses the policies that are duplicates, overlap or name a reserved prefix, and those that
/// ask for a cache when the shape has none (`caching` says whether it has).
pub(super) fn check_policies(policies: &[Policy], caching: bool) -> Result<(), ConfigError> {
    for (at, policy) in policies.iter().enumerate() {
        let key = format!("policy[{at}]");
        let prefix = policy.prefix.as_str();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            return Err(invalid(
                &format!("{key}.prefix"),
                format!("{prefix:?} does not end in /"),
            ));
        }
        if let Some(reserved) = RESERVED.iter().find(|name| prefix.starts_with(**name)) {
            let why = format!("{prefix:?} is under {reserved:?}, which tierstore keeps for itself");
            return Err(invalid(&format!("{key}.prefix"), why));
        }
        if policy.cache && !caching {
            let why = "a storage node keeps no cache: it needs [local_cache] and no [hot_store]";
            return Err(invalid(&format!("{key}.cache"), why));
        }
        for (before, other) in policies.iter().enumerate().take(at) {
            let (theirs, ours) = (other.prefix.as_str(), prefix);
            if theirs == ours {
                let why = format!("{ours:?} is also the prefix of policy[{before}]");
                return Err(invalid(&format!("{key}.prefix"), why));
            }
            let nested = !theirs.is_empty() && !ours.is_empty();
            if nested && (theirs.starts_with(ours) || ours.starts_with(theirs)) {
                let why = format!("{ours:?} overlaps {theirs:?} of policy[{before}]");
                return Err(invalid(&format!("{key}.prefix"), why));
            }
        }
    }
    Ok(())
}

/// The addresses of the `listen` key `key`: at least one, and none twice. The message names the
/// key and not an address.
fn addresses(key: &str, listen: Addrs) -> Result<Vec<SocketAddr>, ConfigError> {
    if listen.0.is_empty() {
        return Err(invalid(key, "it needs at least one address"));
    }
    if has_duplicate(&listen.0) {
        return Err(invalid(key, "the same address is listed twice"));
    }
    Ok(listen.0)
}

/// The OCI front's table checked as its flags are: the values of the `--oci-*` flags.
#[derive(Clone, Debug)]
pub(super) struct OciFlags {
    /// The addresses; none when there is no front.
    pub listen: Vec<SocketAddr>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub prefix: Option<String>,
    pub max_connections: Option<u32>,
    pub connection_lifetime: Option<Duration>,
}

/// The flags `[oci]` stands for: none when the table is left out. A table without `listen` is
/// refused (naming the key it has that needs it, if it has one), the certificate and the key go
/// together, and the count is at least one.
pub(super) fn oci(table: Option<OciTable>) -> Result<OciFlags, ConfigError> {
    let Some(table) = table else {
        return Ok(OciFlags {
            listen: Vec::new(),
            tls_cert: None,
            tls_key: None,
            prefix: None,
            max_connections: None,
            connection_lifetime: None,
        });
    };
    if table.listen.is_none() {
        let others = [
            ("tls_cert_file", table.tls_cert_file.is_some()),
            ("tls_key_file", table.tls_key_file.is_some()),
            ("prefix", table.prefix.is_some()),
            ("max_connections", table.max_connections.is_some()),
            ("connection_lifetime", table.connection_lifetime.is_some()),
        ];
        refuse("oci", "it needs oci.listen", &others)?;
    }
    let listen = require("oci", "listen", table.listen)?;
    let listen = addresses("oci.listen", listen)?;
    match (&table.tls_cert_file, &table.tls_key_file) {
        (Some(_), None) => {
            let why = "it is needed with oci.tls_cert_file: give both, or neither";
            return Err(invalid("oci.tls_key_file", why));
        }
        (None, Some(_)) => {
            let why = "it is needed with oci.tls_key_file: give both, or neither";
            return Err(invalid("oci.tls_cert_file", why));
        }
        _ => {}
    }
    let max_connections = match table.max_connections {
        Some(count) => Some(within("oci", "max_connections", count, 1, u32::MAX)?),
        None => None,
    };
    Ok(OciFlags {
        listen,
        tls_cert: table.tls_cert_file,
        tls_key: table.tls_key_file,
        prefix: table.prefix,
        max_connections,
        connection_lifetime: table.connection_lifetime.map(|dur| dur.0),
    })
}

/// What a file makes: the settings of one of the two shapes.
#[derive(Debug)]
pub(super) enum Settings {
    /// A storage node: the settings the flags make.
    Storage(Box<Cli>),
    /// A build node: a read cache, which has no flags.
    Cache(Box<CacheSettings>),
}

/// Refuses the keys only `[hot_store]` has, in `cold`.
pub(super) fn refuse_hot_keys(cold: &TierTable) -> Result<(), ConfigError> {
    let cold_has = [
        ("budget", cold.budget.is_some()),
        ("high_water", cold.high_water.is_some()),
        ("low_water", cold.low_water.is_some()),
        ("critical", cold.critical.is_some()),
        ("copy_parallel", cold.copy_parallel.is_some()),
        ("lease_ttl", cold.lease_ttl.is_some()),
        ("reconcile_interval", cold.reconcile_interval.is_some()),
        ("lock_file", cold.lock_file.is_some()),
        ("usage_stale_after", cold.usage_stale_after.is_some()),
        ("journal_linger", cold.journal_linger.is_some()),
        ("journal_retention", cold.journal_retention.is_some()),
    ];
    refuse("cold_store", "only [hot_store] has it", &cold_has)
}

#[cfg(test)]
impl Settings {
    /// The settings of a storage node, if that is what these are.
    pub(super) fn into_storage(self) -> Option<Cli> {
        match self {
            Settings::Storage(cli) => Some(*cli),
            Settings::Cache(_) => None,
        }
    }

    /// The settings of a build node, if that is what these are.
    pub(super) fn into_cache(self) -> Option<CacheSettings> {
        match self {
            Settings::Storage(_) => None,
            Settings::Cache(cache) => Some(*cache),
        }
    }
}

/// The settings a file stands for, with the knobs of this run, by the shape its tables make.
fn settings(file: File, per_run: PerRun) -> Result<Settings, ConfigError> {
    let File {
        server,
        hot_store,
        cold_store,
        oci,
        local_cache,
        policy,
    } = file;
    match (hot_store, cold_store, local_cache) {
        (Some(hot), Some(cold), None) => {
            let cli = storage(server, (hot, cold), oci, &policy, per_run)?;
            Ok(Settings::Storage(Box::new(cli)))
        }
        (None, Some(cold), Some(cache)) => {
            let cache = CacheSettings::new(server, (cache, cold), oci, &policy, per_run)?;
            Ok(Settings::Cache(Box::new(cache)))
        }
        (Some(_), Some(_), Some(_)) => {
            let why = "is refused with [hot_store]: the hot store is already local disk, and a disk cache in front of it would cache a disk with a disk";
            Err(invalid("local_cache", why))
        }
        _ => {
            let why = "the file needs both [hot_store] and [cold_store] (the storage-node shape), or [local_cache] and [cold_store] without [hot_store] (the build-node shape)";
            Err(invalid("hot_store, cold_store, local_cache", why))
        }
    }
}

/// The settings of a storage-node file.
fn storage(
    server: ServerTable,
    tiers: (TierTable, TierTable),
    oci_table: Option<OciTable>,
    policy: &[Policy],
    per_run: PerRun,
) -> Result<Cli, ConfigError> {
    let (hot, cold) = tiers;
    check_policies(policy, false)?;
    refuse_hot_keys(&cold)?;
    let why = "only the [cold_store] of a build node takes a list; give endpoint";
    refuse("hot_store", why, &[("endpoints", hot.endpoints.is_some())])?;
    refuse(
        "cold_store",
        why,
        &[("endpoints", cold.endpoints.is_some())],
    )?;
    let skipping = [("skip_checks", hot.skip_checks.is_some())];
    refuse(
        "hot_store",
        "only [cold_store] has it: the check it skips runs there",
        &skipping,
    )?;
    let mut skip_backend_check: Vec<SkippedCheck> = Vec::new();
    for text in cold.skip_checks.iter().flatten() {
        match parse_skip(text) {
            Ok(skipped) => skip_backend_check.push(skipped),
            // Not the reason of the flag: it holds the text, free text that is never repeated.
            Err(_) => {
                let why = "each is CHECK=REASON; only multipart-conditional can be skipped, and it needs a reason";
                return Err(invalid("cold_store.skip_checks", why));
            }
        }
    }
    let hot_parts = tier("hot_store", &hot)?;
    let cold_parts = tier("cold_store", &cold)?;
    let budget = require("hot_store", "budget", hot.budget)?.0;
    let hot_budget = within("hot_store", "budget", budget, 1, u64::MAX)?;
    let percent = |key: &str, value: Option<u8>, default: u8| {
        within("hot_store", key, value.unwrap_or(default), 1, 100)
    };
    let high_water = percent("high_water", hot.high_water, 80)?;
    let low_water = percent("low_water", hot.low_water, 70)?;
    let critical = percent("critical", hot.critical, 95)?;
    let copy_parallel = hot.copy_parallel.unwrap_or(16);
    let copy_parallel = within("hot_store", "copy_parallel", copy_parallel, 1, u32::MAX)?;
    let lock_file = hot.lock_file.clone();
    if let Some(path) = &lock_file {
        check_lock_path(path)
            .map_err(|why| invalid("hot_store.lock_file", format!("{path:?} {why}")))?;
    }
    let oci = oci(oci_table)?;
    let shared = front_of(server, per_run)?;
    let front = shared.front;
    Ok(Cli {
        config: None,
        print_config: false,
        check_config: false,
        capabilities: false,
        listen: front.listen,
        proxy_id: front.proxy_id,
        hot: hot_parts.spec,
        cold: cold_parts.spec,
        hot_endpoint: hot_parts.endpoint,
        hot_region: hot_parts.region,
        hot_credentials_file: hot_parts.credentials_file,
        cold_endpoint: cold_parts.endpoint,
        cold_region: cold_parts.region,
        cold_credentials_file: cold_parts.credentials_file,
        s3_request_timeout: shared.s3_request_timeout,
        hot_bytes: hot_budget,
        high_water,
        low_water,
        critical,
        bucket: front.bucket,
        region: front.region,
        access_key_file: front.access_key_file,
        read_only_key_file: front.read_only_key_file,
        presign_key_file: front.presign_key_file,
        gc_key_file: front.gc_key_file,
        scoped_key_file: front.scoped_key_file,
        presign_max_expires: front.presign_max_expires,
        drain_lb_wait: front.drain_lb_wait,
        drain_deadline: front.drain_deadline,
        journal_linger: duration(hot.journal_linger, Duration::from_millis(20)),
        journal_retention: duration(hot.journal_retention, Duration::from_secs(24 * 3600)),
        copy_parallel,
        lease_ttl: duration(hot.lease_ttl, Duration::from_secs(30)),
        reconcile_interval: duration(hot.reconcile_interval, Duration::from_secs(5)),
        lock_file,
        usage_stale_after: duration(hot.usage_stale_after, Duration::from_secs(120)),
        max_request_bytes: front.max_request_bytes,
        max_concurrent_puts: front.max_concurrent_puts,
        put_permit_wait: front.put_permit_wait,
        put_min_bytes_per_sec: front.put_min_bytes_per_sec,
        put_rate_grace: front.put_rate_grace,
        body_idle_timeout: front.body_idle_timeout,
        skip_backend_check,
        backend_check_wait: shared.backend_check_wait,
        oci_listen: oci.listen,
        oci_tls_cert: oci.tls_cert,
        oci_tls_key: oci.tls_key,
        oci_prefix: oci.prefix,
        oci_max_connections: oci.max_connections,
        oci_connection_lifetime: oci.connection_lifetime,
    })
}

/// The settings of `[server]` that both shapes have, and the two limits of the tiers' requests
/// that are in the same table.
pub(super) struct Shared {
    pub front: FrontSettings,
    pub s3_request_timeout: Duration,
    pub backend_check_wait: Duration,
}

/// The settings `[server]` makes: a key that is left out has the default of its flag.
pub(super) fn front_of(server: ServerTable, per_run: PerRun) -> Result<Shared, ConfigError> {
    let max_request_bytes = server
        .max_request_bytes
        .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    let max_request_bytes = within(
        "server",
        "max_request_bytes",
        max_request_bytes,
        1,
        u64::MAX,
    )?;
    let max_concurrent_puts = server
        .max_concurrent_puts
        .unwrap_or(DEFAULT_MAX_CONCURRENT_PUTS);
    let max_concurrent_puts = within(
        "server",
        "max_concurrent_puts",
        max_concurrent_puts,
        1,
        u32::MAX,
    )?;
    let front = FrontSettings {
        listen: addresses("server.listen", server.listen)?,
        proxy_id: server.proxy_id,
        bucket: server.bucket,
        region: server.region.unwrap_or_else(|| DEFAULT_REGION.to_owned()),
        access_key_file: server.access_key_file,
        read_only_key_file: server.read_only_key_files,
        presign_key_file: server.presign_key_files,
        gc_key_file: server.gc_key_files,
        scoped_key_file: server.scoped_key_files,
        presign_max_expires: duration(server.presign_max_expires, Duration::from_secs(15 * 60)),
        max_request_bytes,
        max_concurrent_puts,
        put_permit_wait: duration(server.put_permit_wait, DEFAULT_PUT_PERMIT_WAIT),
        put_min_bytes_per_sec: server
            .put_min_bytes_per_sec
            .unwrap_or(DEFAULT_PUT_MIN_BYTES_PER_SEC),
        put_rate_grace: duration(server.put_rate_grace, Duration::from_secs(30)),
        body_idle_timeout: duration(server.body_idle_timeout, Duration::from_secs(30)),
        drain_lb_wait: per_run.drain_lb_wait,
        drain_deadline: per_run.drain_deadline,
    };
    Ok(Shared {
        front,
        s3_request_timeout: duration(server.s3_request_timeout, Duration::from_secs(60)),
        backend_check_wait: duration(server.backend_check_wait, Duration::from_secs(300)),
    })
}

/// The settings of the TOML `text`, with the knobs of this run.
///
/// # Errors
/// A [`ConfigError`]: text that is not a file of this format.
pub(super) fn settings_from_text(text: &str, per_run: PerRun) -> Result<Settings, ConfigError> {
    let table: toml::Table = toml::from_str(text).map_err(|e| syntax(text, &e))?;
    find_secret("", &table)?;
    let file: File = toml::from_str(text).map_err(|e| syntax(text, &e))?;
    settings(file, per_run)
}

/// The settings of a storage-node file, which are the settings of flags: the flags' form
/// (`--print-config` checks that what it prints loads this way).
///
/// # Errors
/// A [`ConfigError`]: text that is not a file of this format, or is a build node's.
pub(super) fn from_text(text: &str, per_run: PerRun) -> Result<Cli, ConfigError> {
    match settings_from_text(text, per_run)? {
        Settings::Storage(cli) => Ok(*cli),
        Settings::Cache(_) => {
            let why = "is the file of a build node, which has no flag form";
            Err(invalid("local_cache", why))
        }
    }
}

/// The settings of the file at `path`, with the knobs of this run.
///
/// # Errors
/// A [`ConfigError`] that names the file: it cannot be read, or is not a file of this format.
pub(super) fn load(path: &Path, per_run: PerRun) -> Result<Settings, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) => {
            let path = path.to_owned();
            return Err(ConfigError::Read { path, source });
        }
    };
    settings_from_text(&text, per_run).map_err(|source| ConfigError::In {
        path: path.to_owned(),
        source: Box::new(source),
    })
}

/// `message` with the name of each flag in it replaced by the key that stands for it in the
/// file, so that a refusal in a start from a file speaks of the keys the operator wrote.
pub(super) fn keys_for_flags(message: &str) -> String {
    let mut names: Vec<(&str, &str)> = KEYS
        .iter()
        .filter_map(|(flag, keys)| keys.first().map(|key| (*flag, *key)))
        .collect();
    names.sort_by_key(|(flag, _)| std::cmp::Reverse(flag.len()));
    let mut text = message.to_owned();
    for (flag, key) in names {
        text = text.replace(&format!("--{flag}"), key);
    }
    text
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
