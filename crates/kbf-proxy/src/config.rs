//! The `[proxy]` table of `installer.toml` and the static upstream table.
//!
//! [`ProxyConfig`] is read once at start (SIGHUP reloads the certificate files only, not
//! this table). Unknown keys are refused, so a typo is an error at start, not a default.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::auth::Buck2Auth;
use crate::hold::{HEADER_BYTES, STREAM_WINDOW_BYTES};
use crate::replay::REPLAY_BYTES;
use crate::route::Target;
use crate::slot::Slot;

/// Why a config is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Not valid TOML for this table.
    #[error("{0}")]
    Parse(String),
    /// A rule between keys is broken.
    #[error("{0}")]
    Invalid(&'static str),
}

/// Ports of the children: slot `A` starts at `base_port`, slot `B` at `base_port + 10`; in a
/// slot the REAPI, worker and operator listeners are at offsets 0, 1 and 2, the admin
/// calls at 3 and enrollment at 4. The offsets are the frozen contract with the children
/// (design section 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTable {
    /// The first port of slot A (1024 to 65522).
    pub base_port: u16,
}

/// The slot stride in ports.
pub const SLOT_STRIDE: u16 = 10;
/// The admin port's offset in a slot.
pub const ADMIN_OFFSET: u16 = 3;

impl UpstreamTable {
    const fn base(self, slot: Slot) -> u16 {
        match slot {
            Slot::A => self.base_port,
            Slot::B => self.base_port + SLOT_STRIDE,
        }
    }

    /// The loopback port of `slot`'s listener `target`.
    #[must_use]
    pub const fn port(self, slot: Slot, target: Target) -> u16 {
        self.base(slot) + target.offset()
    }

    /// The loopback port of `slot`'s admin calls.
    #[must_use]
    pub const fn admin_port(self, slot: Slot) -> u16 {
        self.base(slot) + ADMIN_OFFSET
    }
}

/// Pre-stream and keepalive limits (design section 3). Every field has a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// TLS handshake, HTTP/2 preface and request-head timeouts, in seconds.
    pub pre_stream_timeout_s: u64,
    /// Most open client connections.
    pub max_connections: usize,
    /// Most open connections from one address.
    pub max_connections_per_ip: usize,
    /// Most streams in flight across all connections (bound, held and dialing); one more
    /// is refused at header time. Memory is bounded by this times the stream windows.
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

/// The `[proxy]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// The children's ports.
    pub upstream: UpstreamTable,
    /// REAPI and Buck2 authentication; default `none`.
    #[serde(default)]
    pub buck2_auth: Buck2Auth,
    /// The Buck2 API key file, required by `buck2_auth = "bearer"`.
    pub buck2_token_file: Option<PathBuf>,
    /// The operator token file (`/etc/kbf/operator.token`), read by the proxy itself at start.
    pub operator_token_file: PathBuf,
    /// The server certificate chain and key, reloaded on SIGHUP together with `client_ca`.
    pub server_cert_file: PathBuf,
    /// See `server_cert_file`.
    pub server_key_file: PathBuf,
    /// The farm CA bundle that verifies client certificates.
    pub client_ca_file: PathBuf,
    /// The secret file (default `/run/kbf-proxy/proxy-auth`).
    #[serde(default = "default_secret_file")]
    pub proxy_auth_file: PathBuf,
    /// The proxy's state directory (default `/var/lib/kbf-proxy`). The swap state machine and
    /// `update.json` belong to U7a's actor, not to the proxy.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Hold bound of a frontend swap, in seconds (default 20: the 15 s takeover budget plus a
    /// 5 s margin).
    #[serde(default = "default_hold_s")]
    pub hold_max_s: u64,
    /// Limits.
    #[serde(default)]
    pub limits: Limits,
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
    /// The hold bound of a frontend swap, to pass to `ActiveSlot::hold`.
    #[must_use]
    pub const fn frontend_hold(&self) -> Duration {
        Duration::from_secs(self.hold_max_s)
    }

    /// Reads and checks a `[proxy]` table (the text of the table's body).
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), ConfigError> {
        if !(1024..=65_522).contains(&self.upstream.base_port) {
            return Err(ConfigError::Invalid(
                "upstream.base_port must be 1024 to 65522 so both slots fit",
            ));
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
        if self.hold_max_s == 0 {
            return Err(ConfigError::Invalid("hold_max_s must be above 0"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
[upstream]
base_port = 18000
"#;

    #[test]
    fn a_minimal_table_gets_the_documented_defaults() {
        let c = ProxyConfig::from_toml_str(MIN).expect("valid");
        assert_eq!(c.buck2_auth, Buck2Auth::None);
        assert_eq!(c.hold_max_s, 20);
        assert_eq!(c.frontend_hold(), Duration::from_secs(20));
        assert_eq!(c.limits.max_streams, 8_192);
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.limits.pre_stream_timeout(), Duration::from_secs(10));
        assert_eq!(
            c.proxy_auth_file,
            PathBuf::from("/run/kbf-proxy/proxy-auth")
        );
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/kbf-proxy"));
    }

    #[test]
    fn slot_ports_are_fixed_offsets() {
        let t = UpstreamTable { base_port: 18000 };
        assert_eq!(t.port(Slot::A, Target::Reapi), 18000);
        assert_eq!(t.port(Slot::A, Target::Operator), 18002);
        assert_eq!(t.port(Slot::B, Target::Worker), 18011);
        assert_eq!(t.admin_port(Slot::A), 18003);
        assert_eq!(t.admin_port(Slot::B), 18013);
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
        assert!(matches!(
            bad("hold_max_s = 0"),
            Err(ConfigError::Invalid(_))
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
