//! The `[proxy]` table of `installer.toml`.
//!
//! [`ProxyConfig`] is read once at start (SIGHUP reloads the certificate files only, not
//! this table). Unknown keys are refused, so a typo is an error at start, not a default.
//!
//! The children are given as `[[frontend]]` tables: any number of frontends that run on their
//! own, which the proxy balances over and drains one at a time (`admin_listen` is where the
//! drain is asked for). The `upstream` table of the removed two-slot supervision is still read
//! and written back, so that a file written before keeps its table when `kbf-installer setup`
//! rewrites it and the release fetcher reads it ([`ProxyConfig::check`] accepts it). The proxy's
//! own start refuses it ([`ProxyConfig::check_pool`], [`SLOT_MODE_REMOVED`]).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use kbf_tls::{CertEntry, SniCerts};
use serde::{Deserialize, Serialize};

use crate::auth::Buck2Auth;
use crate::limits::{HEADER_BYTES, STREAM_WINDOW_BYTES};
use crate::replay::REPLAY_BYTES;

/// What a table with the `upstream` table is refused with.
pub const SLOT_MODE_REMOVED: &str = "two-slot supervision was removed: configure pool mode, a \
     [[proxy.frontend]] table for each frontend that runs on its own, in place of \
     [proxy.upstream]";

/// Why a config is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Not valid TOML for this table.
    #[error("{0}")]
    Parse(String),
    /// A rule between keys is broken.
    #[error("{0}")]
    Invalid(&'static str),
    /// The server certificate list breaks a rule of [`kbf_tls::SniCerts::check_entries`].
    #[error("server_cert: {0}")]
    ServerCerts(String),
}

/// The `upstream` table of the removed two-slot supervision: the first port of its slot A. It
/// is kept as it was written; nothing reads the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTable {
    /// The first port of slot A.
    pub base_port: u16,
}

/// One `[[frontend]]` table: a frontend that runs on its own, which the proxy balances over.
/// Its listeners are at fixed offsets from `base_port` (REAPI 0, worker 1, operator 2, admin
/// calls 3, enrollment 4), on loopback; the offsets are the frozen contract with the
/// children (design section 7).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frontend {
    /// What the admin API drains it by and the metrics label it with: 1 to
    /// [`MAX_NAME_BYTES`] of letters, digits, `-` and `_`; unique in the table.
    pub name: String,
    /// The first of its [`FRONTEND_SPAN`] ports (1024 to [`MAX_FRONTEND_PORT`]).
    pub base_port: u16,
}

impl Frontend {
    /// Whether `port` is one of its [`FRONTEND_SPAN`] ports (a `base_port` in range, which the
    /// table check has settled).
    fn owns(&self, port: u16) -> bool {
        let last = self.base_port + (FRONTEND_SPAN - 1);
        (self.base_port..=last).contains(&port)
    }
}

/// The ports a frontend uses from its `base_port` up: REAPI, worker, operator, admin calls and
/// enrollment. The base ports of two frontends are at least this far apart.
pub const FRONTEND_SPAN: u16 = 5;
/// The largest frontend `base_port`: its highest offset (enrollment, 4) must still be a port.
pub const MAX_FRONTEND_PORT: u16 = u16::MAX - (FRONTEND_SPAN - 1);
/// The longest frontend name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// Whether `name` is a frontend name: it goes into a URL path and a metrics label unescaped.
fn name_is_plain(name: &str) -> bool {
    let plain = |b: u8| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_');
    (1..=MAX_NAME_BYTES).contains(&name.len()) & name.bytes().all(plain)
}

/// Pre-stream and keepalive limits (design section 3). Every field has a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// TLS handshake, HTTP/2 preface and request-head timeouts, in seconds.
    pub pre_stream_timeout_s: u64,
    /// Most open client connections.
    pub max_connections: usize,
    /// Most open connections from one address.
    pub max_connections_per_ip: usize,
    /// Most streams in flight across all connections (dialing and bound); one more is
    /// refused at header time. Memory is bounded by this times the stream windows.
    pub max_streams: usize,
    /// The memory the proxy may need at the worst: [`Limits::max_streams`] times
    /// [`Limits::stream_ceiling_bytes`] must fit in it, or the config is refused at start. The
    /// default assumes a host of 16 GiB or more with the proxy at half.
    pub memory_budget_mib: u64,
    /// Most header-list bytes per request.
    pub max_header_list_bytes: u32,
    /// Interval of the downstream PING that finds dead peers, in seconds.
    pub keepalive_interval_s: u64,
    /// How long a PING may go unanswered before the connection is closed, in seconds.
    pub keepalive_ack_s: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pre_stream_timeout_s: 10,
            max_connections: 50_000,
            max_connections_per_ip: 2_000,
            max_streams: 8_192,
            memory_budget_mib: 8_192,
            max_header_list_bytes: 16 * 1024,
            keepalive_interval_s: 30,
            keepalive_ack_s: 20,
        }
    }
}

impl Limits {
    /// The most memory one stream can pin: its window in each direction, the replay buffer
    /// (it keeps the first bytes after their window credit was returned) and the header list.
    #[must_use]
    pub const fn stream_ceiling_bytes() -> u64 {
        2 * STREAM_WINDOW_BYTES as u64 + REPLAY_BYTES as u64 + HEADER_BYTES
    }

    /// `max_streams` streams at their ceiling.
    #[must_use]
    pub const fn worst_case_bytes(&self) -> u64 {
        self.max_streams as u64 * Self::stream_ceiling_bytes()
    }

    /// The pre-stream timeout.
    #[must_use]
    pub const fn pre_stream_timeout(&self) -> Duration {
        Duration::from_secs(self.pre_stream_timeout_s)
    }
}

/// One `[[server_cert]]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCert {
    /// The DNS names this certificate is served for; each must be in its subject alternative
    /// names. May be empty only on the default.
    #[serde(default)]
    pub names: Vec<String>,
    /// The PEM certificate chain, leaf first.
    pub cert_file: PathBuf,
    /// The PEM private key of the leaf.
    pub key_file: PathBuf,
    /// Whether this is the certificate for a client that names no host or an unknown one.
    #[serde(default)]
    pub default: bool,
}

/// The `[proxy]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// The `upstream` table of the removed two-slot supervision. [`ProxyConfig::check`]
    /// accepts it, so that a file written before is read and written back whole;
    /// [`ProxyConfig::check_pool`] refuses it with [`SLOT_MODE_REMOVED`].
    #[serde(default)]
    pub upstream: Option<UpstreamTable>,
    /// The frontends that run on their own, which the proxy balances over (the least busy
    /// that is ready takes the next stream) and drains one at a time. Names are unique and
    /// base ports at least [`FRONTEND_SPAN`] apart. At least one is needed.
    #[serde(default)]
    pub frontend: Vec<Frontend>,
    /// Where the admin API listens, plain HTTP on a loopback address with a port
    /// (`host:port`). It lists the frontends and drains or undrains one (`pool_admin`). Unset,
    /// there is no admin API. Refused for an address that is not loopback (nothing on this
    /// socket asks for a credential) and for a port of a frontend.
    #[serde(default, deserialize_with = "admin_address")]
    pub admin_listen: Option<SocketAddr>,
    /// The public address, as `host:port`. The proxy does not read it: it takes the socket
    /// `kbf-proxy.socket` hands it. `kbf-installer setup` turns it into the socket's drop-in
    /// (`ListenStream=`); unset, the socket keeps the port the package ships.
    #[serde(default, deserialize_with = "listen_address")]
    pub listen: Option<SocketAddr>,
    /// REAPI and Buck2 authentication; default `none`.
    #[serde(default)]
    pub buck2_auth: Buck2Auth,
    /// The Buck2 API key file, required by `buck2_auth = "bearer"`.
    pub buck2_token_file: Option<PathBuf>,
    /// The operator token file (`/etc/kbf/operator.token`), read by the proxy itself at start.
    pub operator_token_file: PathBuf,
    /// No longer read: the allow-list for CI jobs that called with a GitHub OIDC token instead
    /// of the operator token went with the update API's handler. The key is still accepted, so
    /// that a file written before keeps starting the proxy; a CI job's token is no longer
    /// admitted.
    #[serde(default)]
    pub remote_operator_file: Option<PathBuf>,
    /// The server certificate chain and key, reloaded on SIGHUP together with `client_ca`:
    /// the single-certificate form, served for every name. It is the same as one
    /// `[[server_cert]]` with no names; give this pair or `server_cert`, not both.
    pub server_cert_file: Option<PathBuf>,
    /// See `server_cert_file`.
    pub server_key_file: Option<PathBuf>,
    /// The server certificates chosen by SNI: `[[server_cert]]` tables, each with the `names`
    /// it is served for, its `cert_file` and `key_file`, and `default = true` on exactly one
    /// when there are two or more (the one a handshake with no name or an unknown name gets).
    #[serde(default)]
    pub server_cert: Vec<ServerCert>,
    /// The farm CA bundle that verifies client certificates.
    pub client_ca_file: PathBuf,
    /// The secret file (default `/run/kbf-proxy/proxy-auth`).
    #[serde(default = "default_secret_file")]
    pub proxy_auth_file: PathBuf,
    /// No longer read: the proxy's state directory (default `/var/lib/kbf-proxy`) belonged to
    /// the swap. The key is still accepted, so that a file written before keeps starting the
    /// proxy.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// No longer read: the hold bound of a frontend swap, in seconds (default 20). The key is
    /// still accepted, so that a file written before keeps starting the proxy.
    #[serde(default = "default_hold_s")]
    pub hold_max_s: u64,
    /// Serve the read-only web UI at `/ui` (default `true`). The UI is open: it asks for no
    /// certificate and no token, and only GET and HEAD reach it. `false` makes `/ui` a 404 at
    /// the proxy.
    #[serde(default = "default_ui")]
    pub ui: bool,
    /// Limits.
    #[serde(default)]
    pub limits: Limits,
}

const fn default_ui() -> bool {
    true
}

/// Reads `listen`. The error names the key and never the value, as the rest of the file's
/// refusals do.
fn listen_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SocketAddr>, D::Error> {
    const WHY: &str = "listen: must be a socket address, host:port with a port from 1 to 65535 (an IPv6 host in brackets)";
    socket_address(deserializer, WHY)
}

/// Reads `admin_listen` the way `listen` is read, naming its own key.
fn admin_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SocketAddr>, D::Error> {
    const WHY: &str = "admin_listen: must be a socket address, host:port with a port from 1 to 65535 (an IPv6 host in brackets)";
    socket_address(deserializer, WHY)
}

/// A socket address with a port, or the refusal `why`.
fn socket_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    why: &'static str,
) -> Result<Option<SocketAddr>, D::Error> {
    use serde::de::Error;
    match toml::Value::deserialize(deserializer)? {
        toml::Value::String(text) => match text.parse::<SocketAddr>() {
            Ok(addr) if addr.port() != 0 => Ok(Some(addr)),
            _ => Err(D::Error::custom(why)),
        },
        _ => Err(D::Error::custom(why)),
    }
}

fn default_secret_file() -> PathBuf {
    PathBuf::from("/run/kbf-proxy/proxy-auth")
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/kbf-proxy")
}

const fn default_hold_s() -> u64 {
    20
}

impl ProxyConfig {
    /// Reads a `[proxy]` table (the text of the table's body) and checks it as the proxy does at
    /// start ([`ProxyConfig::check_pool`]).
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.check_pool()?;
        Ok(config)
    }

    /// The server certificates as the TLS layer takes them: the single pair as one default
    /// entry with no names, or the `[[server_cert]]` list (a checked table has only one of
    /// the two).
    #[must_use]
    pub fn cert_entries(&self) -> Vec<CertEntry> {
        match (&self.server_cert_file, &self.server_key_file) {
            (Some(cert), Some(key)) => vec![CertEntry {
                names: Vec::new(),
                cert: cert.clone(),
                key: key.clone(),
                default: true,
            }],
            _ => self
                .server_cert
                .iter()
                .map(|c| CertEntry {
                    names: c.names.clone(),
                    cert: c.cert_file.clone(),
                    key: c.key_file.clone(),
                    default: c.default,
                })
                .collect(),
        }
    }

    /// Reads and checks the `[proxy]` table of a larger file (`installer.toml`) that the caller
    /// has already parsed, with [`ProxyConfig::check`]: the table of a file written before the
    /// two-slot supervision was removed is read, and refused only at the proxy's start.
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_value(table: toml::Value) -> Result<Self, ConfigError> {
        let config: Self = table
            .try_into()
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.check()?;
        Ok(config)
    }

    /// The proxy's check at start: [`ProxyConfig::check`], and no `upstream` table, which it
    /// refuses with [`SLOT_MODE_REMOVED`].
    ///
    /// # Errors
    /// [`ConfigError::Invalid`].
    pub fn check_pool(&self) -> Result<(), ConfigError> {
        if self.upstream.is_some() {
            return Err(ConfigError::Invalid(SLOT_MODE_REMOVED));
        }
        self.check()
    }

    /// The rules between keys that a parse cannot see, for a caller (`kbf-installer setup`, the
    /// release fetcher) that reads the table of a file and must not refuse one written before
    /// the removal of the two-slot supervision: the rules of the pool, unless the table has the
    /// `upstream` table, whose frontends and `admin_listen` are not looked into.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`].
    pub fn check(&self) -> Result<(), ConfigError> {
        let pair = (&self.server_cert_file, &self.server_key_file);
        match pair {
            (Some(_), None) | (None, Some(_)) => {
                return Err(ConfigError::Invalid(
                    "server_cert_file and server_key_file go together",
                ));
            }
            (Some(_), Some(_)) if !self.server_cert.is_empty() => {
                return Err(ConfigError::Invalid(
                    "give server_cert_file/server_key_file or [[server_cert]], not both",
                ));
            }
            (None, None) if self.server_cert.is_empty() => {
                return Err(ConfigError::Invalid(
                    "a server certificate is needed: server_cert_file and server_key_file, or [[server_cert]]",
                ));
            }
            _ => {}
        }
        SniCerts::check_entries(&self.cert_entries())
            .map_err(|e| ConfigError::ServerCerts(e.to_string()))?;
        if self.upstream.is_none() {
            self.check_frontends()?;
        }
        if self.buck2_auth == Buck2Auth::Bearer && self.buck2_token_file.is_none() {
            return Err(ConfigError::Invalid(
                "buck2_auth = \"bearer\" needs buck2_token_file",
            ));
        }
        let l = &self.limits;
        if l.keepalive_ack_s == 0 || l.keepalive_interval_s == 0 || l.pre_stream_timeout_s == 0 {
            return Err(ConfigError::Invalid("limits timeouts must be above 0"));
        }
        if l.max_connections_per_ip == 0 || l.max_connections_per_ip > l.max_connections {
            return Err(ConfigError::Invalid(
                "limits.max_connections_per_ip must be 1 to max_connections",
            ));
        }
        if l.max_streams == 0 {
            return Err(ConfigError::Invalid("limits.max_streams must be above 0"));
        }
        if l.worst_case_bytes() > l.memory_budget_mib.saturating_mul(1 << 20) {
            return Err(ConfigError::Invalid(
                "limits.max_streams times the per-stream ceiling exceeds limits.memory_budget_mib",
            ));
        }
        Ok(())
    }

    /// The rules about the frontends: at least one, plain unique names, base ports that leave
    /// each frontend its [`FRONTEND_SPAN`] ports, and an `admin_listen` on loopback and on none
    /// of those ports.
    fn check_frontends(&self) -> Result<(), ConfigError> {
        if self.frontend.is_empty() {
            return Err(ConfigError::Invalid(
                "the children are needed: [[frontend]] tables",
            ));
        }
        let mut names = BTreeSet::new();
        for frontend in &self.frontend {
            if !name_is_plain(&frontend.name) {
                return Err(ConfigError::Invalid(
                    "frontend.name must be 1 to 64 letters, digits, '-' or '_'",
                ));
            }
            if !names.insert(frontend.name.as_str()) {
                return Err(ConfigError::Invalid("frontend names must be unique"));
            }
            if !(1024..=MAX_FRONTEND_PORT).contains(&frontend.base_port) {
                return Err(ConfigError::Invalid(
                    "frontend.base_port must be 1024 to 65531 so its five ports fit",
                ));
            }
        }
        let mut bases: Vec<u16> = self.frontend.iter().map(|f| f.base_port).collect();
        bases.sort_unstable();
        let apart = |pair: &[u16]| pair[1] - pair[0] >= FRONTEND_SPAN;
        if !bases.windows(2).all(apart) {
            return Err(ConfigError::Invalid(
                "frontend base ports must be at least 5 apart so their ports do not overlap",
            ));
        }
        let loopback = |admin: SocketAddr| admin.ip().is_loopback();
        if !self.admin_listen.is_none_or(loopback) {
            return Err(ConfigError::Invalid(
                "admin_listen must be a loopback address: it asks for no credential",
            ));
        }
        let overlaps = |admin: SocketAddr| self.frontend.iter().any(|f| f.owns(admin.port()));
        if self.admin_listen.is_some_and(overlaps) {
            return Err(ConfigError::Invalid(
                "admin_listen must not use a port of a frontend (base_port to base_port + 4)",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_pool_tests.rs"]
mod pool_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
[[frontend]]
name = "a"
base_port = 18000
"#;

    /// Asserts that a minimal table gets the documented defaults: the retired keys at the values
    /// a file written before them reads as, the limits at theirs, and no admin API.
    #[test]
    fn a_minimal_table_gets_the_documented_defaults() {
        let c = ProxyConfig::from_toml_str(MIN).expect("valid");
        assert_eq!(c.buck2_auth, Buck2Auth::None);
        assert_eq!(c.hold_max_s, 20);
        assert_eq!(c.remote_operator_file, None);
        assert_eq!(c.admin_listen, None);
        assert_eq!(c.limits.max_streams, 8_192);
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.limits.pre_stream_timeout(), Duration::from_secs(10));
        assert_eq!(
            c.proxy_auth_file,
            PathBuf::from("/run/kbf-proxy/proxy-auth")
        );
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/kbf-proxy"));
    }

    /// Asserts that the keys of the removed supervision and of the removed CI credential are
    /// still read, with any value they had, so that a file written before keeps starting the
    /// proxy.
    #[test]
    fn the_retired_keys_are_still_read() {
        let old = format!(
            "hold_max_s = 0\nstate_dir = \"/var/lib/other\"\nremote_operator_file = \"/etc/kbf/ci.toml\"\n{MIN}"
        );
        let c = ProxyConfig::from_toml_str(&old).expect("valid");
        assert_eq!(c.hold_max_s, 0);
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/other"));
        assert_eq!(
            c.remote_operator_file,
            Some(PathBuf::from("/etc/kbf/ci.toml"))
        );
    }

    /// The UI is on unless the table says `ui = false`, and the key is read as a boolean.
    #[test]
    fn the_ui_is_on_by_default_and_ui_false_turns_it_off() {
        assert!(ProxyConfig::from_toml_str(MIN).expect("valid").ui);
        let on = format!("ui = true\n{MIN}");
        assert!(ProxyConfig::from_toml_str(&on).expect("valid").ui);
        let off = format!("ui = false\n{MIN}");
        assert!(!ProxyConfig::from_toml_str(&off).expect("valid").ui);
        let word = format!("ui = \"no\"\n{MIN}");
        assert!(matches!(
            ProxyConfig::from_toml_str(&word),
            Err(ConfigError::Parse(_))
        ));
    }

    const HEAD: &str = r#"
operator_token_file = "/etc/kbf/operator.token"
client_ca_file = "/etc/kbf/tls/ca.pem"
"#;
    const FRONTEND: &str = "[[frontend]]\nname = \"a\"\nbase_port = 18000\n";

    fn two_certs(extra: &str) -> String {
        format!(
            r#"{extra}
{HEAD}
[[server_cert]]
names = ["reapi.example.com"]
cert_file = "/etc/kbf/tls/reapi.pem"
key_file = "/etc/kbf/tls/reapi.key"
default = true
[[server_cert]]
names = ["farm.internal.example"]
cert_file = "/etc/kbf/tls/worker.pem"
key_file = "/etc/kbf/tls/worker.key"
{FRONTEND}"#
        )
    }

    /// The single-pair form is one default entry with no names, so a config written before
    /// SNI keeps serving one certificate for every name.
    #[test]
    fn the_single_certificate_form_is_a_one_entry_list() {
        let c = ProxyConfig::from_toml_str(MIN).expect("valid");
        assert_eq!(
            c.cert_entries(),
            [CertEntry {
                names: Vec::new(),
                cert: "/etc/kbf/tls/server.pem".into(),
                key: "/etc/kbf/tls/server.key".into(),
                default: true,
            }]
        );
    }

    /// A list of two with one marked default reads in file order with its names.
    #[test]
    fn a_server_cert_list_is_read_with_names_and_the_default() {
        let c = ProxyConfig::from_toml_str(&two_certs("")).expect("valid");
        let entries = c.cert_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].names, ["reapi.example.com"]);
        assert!(entries[0].default);
        assert!(!entries[1].default);
        assert_eq!(entries[1].key, PathBuf::from("/etc/kbf/tls/worker.key"));
    }

    /// The forms do not mix, a half pair is refused, a list needs its default, and one
    /// certificate is required.
    #[test]
    fn the_certificate_forms_are_exclusive_and_the_default_is_explicit() {
        let pair = "server_cert_file = \"/c\"\nserver_key_file = \"/k\"\n";
        assert!(matches!(
            ProxyConfig::from_toml_str(&two_certs(pair)),
            Err(ConfigError::Invalid(_))
        ));
        let half = format!("server_cert_file = \"/c\"\n{HEAD}{FRONTEND}");
        assert!(matches!(
            ProxyConfig::from_toml_str(&half),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            ProxyConfig::from_toml_str(&format!("{HEAD}{FRONTEND}")),
            Err(ConfigError::Invalid(_))
        ));
        let no_default = two_certs("").replacen("default = true", "", 1);
        assert!(matches!(
            ProxyConfig::from_toml_str(&no_default),
            Err(ConfigError::ServerCerts(_))
        ));
        let twice = two_certs("").replace("names = [\"farm", "default = true\nnames = [\"farm");
        assert!(matches!(
            ProxyConfig::from_toml_str(&twice),
            Err(ConfigError::ServerCerts(_))
        ));
        let dup = two_certs("").replace("farm.internal.example", "REAPI.example.com");
        assert!(matches!(
            ProxyConfig::from_toml_str(&dup),
            Err(ConfigError::ServerCerts(_))
        ));
    }

    #[test]
    fn a_table_read_from_a_larger_file_gets_the_same_checks() {
        let value: toml::Value = toml::from_str(MIN).expect("toml");
        assert_eq!(
            ProxyConfig::from_value(value).expect("valid"),
            ProxyConfig::from_toml_str(MIN).expect("valid")
        );
        let unknown: toml::Value = toml::from_str(&format!("surprise = 1\n{MIN}")).expect("toml");
        assert!(matches!(
            ProxyConfig::from_value(unknown),
            Err(ConfigError::Parse(_))
        ));
        let low: toml::Value = toml::from_str(&MIN.replace("18000", "80")).expect("toml");
        assert!(matches!(
            ProxyConfig::from_value(low),
            Err(ConfigError::Invalid(_))
        ));
    }

    /// Catches `listen` being unknown to the closed reader (the proxy would refuse the very
    /// file `setup` reads), or read as anything but a socket address.
    #[test]
    fn listen_is_a_socket_address() {
        let with = |v: &str| ProxyConfig::from_toml_str(&format!("listen = {v}\n{MIN}"));
        assert_eq!(ProxyConfig::from_toml_str(MIN).expect("valid").listen, None);
        assert_eq!(
            with("\"192.0.2.10:8443\"").expect("v4").listen,
            Some("192.0.2.10:8443".parse().unwrap())
        );
        assert_eq!(
            with("\"[2001:db8::1]:8980\"").expect("v6").listen,
            Some("[2001:db8::1]:8980".parse().unwrap())
        );
    }

    /// Catches an unvalidated `listen` (a host name, no port, port 0, a number) reaching a
    /// drop-in, and an error that echoes the value instead of naming the key.
    #[test]
    fn a_bad_listen_is_refused_naming_the_key_not_the_value() {
        for bad in [
            "\"farm.example:8980\"",
            "\"192.0.2.10\"",
            "\"192.0.2.10:0\"",
            "\"2001:db8::1:8980\"",
            "\"\"",
            "8980",
            "[\"192.0.2.10:8980\"]",
        ] {
            // Through `from_value`, as `installer.toml` is read: `from_toml_str` adds the
            // source line to a parse error, which is the reader of a bare table's to decide.
            let table: toml::Value =
                toml::from_str(&format!("listen = {bad}\n{MIN}")).expect("toml");
            let refused = ProxyConfig::from_value(table).expect_err("a bad listen is refused");
            assert!(
                matches!(refused, ConfigError::Parse(_)),
                "{bad}: {refused:?}"
            );
            let why = refused.to_string();
            assert!(why.contains("listen"), "{why}");
            for piece in ["farm.example", "192.0.2.10", "2001:db8"] {
                assert!(!why.contains(piece), "{why} echoes the value");
            }
        }
    }

    /// Catches `Serialize` dropping or reshaping a key: `setup` rewrites `installer.toml` from
    /// the parsed table, so a table must come back equal.
    #[test]
    fn a_table_serializes_back_to_itself() {
        let text = format!(
            "listen = \"192.0.2.10:8443\"\nbuck2_auth = \"bearer\"\nbuck2_token_file = \"/k\"\n{MIN}\n[limits]\nmax_streams = 100\n"
        );
        let config = ProxyConfig::from_toml_str(&text).expect("valid");
        let again = toml::to_string(&config).expect("serializes");
        assert_eq!(ProxyConfig::from_toml_str(&again).expect("valid"), config);
    }

    /// Catches `setup`'s rewrite dropping or reshaping the `[[server_cert]]` list: names,
    /// files and the default flag come back equal.
    #[test]
    fn a_table_with_server_certs_serializes_back_to_itself() {
        let config = ProxyConfig::from_toml_str(&two_certs("")).expect("valid");
        assert_eq!(config.server_cert.len(), 2);
        let again = toml::to_string(&config).expect("serializes");
        assert_eq!(ProxyConfig::from_toml_str(&again).expect("valid"), config);
    }

    #[test]
    fn a_broken_table_is_refused_at_start() {
        let bad = |extra: &str| ProxyConfig::from_toml_str(&format!("{extra}\n{MIN}"));
        assert!(matches!(bad("surprise = 1"), Err(ConfigError::Parse(_))));
        assert!(matches!(
            bad("buck2_auth = \"bearer\""),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            bad("buck2_auth = \"basic\""),
            Err(ConfigError::Parse(_))
        ));
        assert!(bad("buck2_auth = \"bearer\"\nbuck2_token_file = \"/k\"").is_ok());
        let low = MIN.replace("18000", "80");
        assert!(matches!(
            ProxyConfig::from_toml_str(&low),
            Err(ConfigError::Invalid(_))
        ));
        let limits = |l: &str| ProxyConfig::from_toml_str(&format!("{MIN}\n[limits]\n{l}\n"));
        for zero in [
            "keepalive_ack_s",
            "keepalive_interval_s",
            "pre_stream_timeout_s",
        ] {
            assert!(
                matches!(limits(&format!("{zero} = 0")), Err(ConfigError::Invalid(_))),
                "{zero}"
            );
        }
        assert!(matches!(
            limits("max_streams = 0"),
            Err(ConfigError::Invalid(_))
        ));
        // 8192 streams at 592 KiB is 4.75 GiB: the default fits its 8 GiB budget, a bigger
        // stream cap or a smaller budget does not.
        assert_eq!(
            Limits::stream_ceiling_bytes(),
            2 * 262_144 + 65_536 + 16_384
        );
        assert_eq!(Limits::default().worst_case_bytes(), 8192 * 606_208);
        assert!(limits("max_streams = 14169").is_ok());
        assert!(matches!(
            limits("max_streams = 14170"),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            limits("memory_budget_mib = 4096"),
            Err(ConfigError::Invalid(_))
        ));
        assert!(limits("memory_budget_mib = 4800").is_ok());
        assert!(matches!(
            limits("max_connections_per_ip = 0"),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            limits("max_connections = 1\nmax_connections_per_ip = 2"),
            Err(ConfigError::Invalid(_))
        ));
    }
}
