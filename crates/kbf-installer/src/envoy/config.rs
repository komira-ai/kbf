//! The settings of a box's Envoy: one Envoy in front of two kbf instances on one host.
//!
//! The installer's caller builds a [`BoxSettings`] from what the host already decides: the
//! ports and files from `installer.toml` and the frontend's flags, the limits from the
//! flags `kbf frontend` is started with (`--reapi-max-streams`, the stream ceiling, the
//! longest action). Nothing here reads a file or the environment, and there is no
//! `KEY=VALUE` text of its own. [`BoxSettings::validate`] refuses what the render cannot
//! use, and [`super::render`] and [`super::check`] call it first.

/// The offsets of one instance's loopback ports from its base: REAPI, worker, API.
pub const REAPI_OFFSET: u16 = 0;
pub const WORKER_OFFSET: u16 = 1;
pub const API_OFFSET: u16 = 2;
/// Instance `b`'s ports are instance `a`'s plus this.
pub const SECOND_INSTANCE: u16 = 10;

/// The extra time after the longest action that Envoy's old epoch is kept, in seconds.
pub const PARENT_SLACK_SECS: u64 = 900;

/// The most streams one upstream connection may carry that kbf's HTTP/2 stack accepts.
pub const MAX_STREAMS_LIMIT: u32 = 32767;
/// The most open streams the ceiling may name (a breaker limit Envoy takes as a u32).
pub const STREAM_CEILING_LIMIT: u32 = 100_000_000;
/// The longest action the settings may name: one year.
pub const LONGEST_ACTION_LIMIT: u64 = 31_536_000;

/// One kbf instance's loopback ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance {
    pub reapi: u16,
    pub worker: u16,
    pub api: u16,
}

/// What the Envoy of one box is rendered from. Plain values, so a caller fills them from
/// its own settings; [`BoxSettings::validate`] says whether they can be rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxSettings {
    /// The DNS name the kbf instances' REAPI certificate carries; Envoy's SNI and name
    /// check on the hop to them, and the Host of the health checks.
    pub host: String,
    /// The public REAPI port (clients' TLS ends in Envoy).
    pub reapi_port: u16,
    /// The public worker port (the daemons' mutual TLS passes through Envoy).
    pub worker_port: u16,
    /// Envoy's admin port, on loopback.
    pub admin_port: u16,
    /// Instance `a`'s first loopback port; `b`'s is [`SECOND_INSTANCE`] above.
    pub backend_port_base: u16,
    /// The certificate chain Envoy shows REAPI clients (an absolute path).
    pub cert_file: String,
    /// The key of that certificate (an absolute path).
    pub key_file: String,
    /// The CA the instances' certificates chain to (an absolute path).
    pub ca_file: String,
    /// kbf's `--reapi-max-streams`: the streams one upstream connection may carry.
    pub reapi_max_streams: u32,
    /// The concurrent streams Envoy must hold open across the REAPI cluster.
    pub stream_ceiling: u32,
    /// The longest action the farm runs, in seconds.
    pub longest_action_secs: u64,
}

/// Why the settings cannot be rendered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("{field}={value:?} is not {want}")]
    Field {
        field: &'static str,
        value: String,
        want: &'static str,
    },
    #[error("{a} and {b} are both port {port}")]
    PortTwice { a: String, b: String, port: u16 },
    #[error("backend_port_base={0}: the second instance's ports run past 65535")]
    BasePastRange(u16),
}

impl BoxSettings {
    /// Whether the settings can be rendered: each value of the shape its field takes, and
    /// no port used twice.
    pub fn validate(&self) -> Result<(), SettingsError> {
        let port = |field, value: u16| {
            require(
                field,
                &value.to_string(),
                "a port from 1 to 65535",
                value != 0,
            )
        };
        let dns = "a DNS name (DNS labels joined by .)";
        require("host", &self.host, dns, self.host.split('.').all(dns_label))?;
        port("reapi_port", self.reapi_port)?;
        port("worker_port", self.worker_port)?;
        port("admin_port", self.admin_port)?;
        port("backend_port_base", self.backend_port_base)?;
        require_path("cert_file", &self.cert_file)?;
        require_path("key_file", &self.key_file)?;
        require_path("ca_file", &self.ca_file)?;
        let streams = (1..=MAX_STREAMS_LIMIT).contains(&self.reapi_max_streams);
        require(
            "reapi_max_streams",
            &self.reapi_max_streams.to_string(),
            "a number from 1 to 32767",
            streams,
        )?;
        let ceiling = (1..=STREAM_CEILING_LIMIT).contains(&self.stream_ceiling);
        require(
            "stream_ceiling",
            &self.stream_ceiling.to_string(),
            "a number from 1 to 100000000",
            ceiling,
        )?;
        let longest = (1..=LONGEST_ACTION_LIMIT).contains(&self.longest_action_secs);
        require(
            "longest_action_secs",
            &self.longest_action_secs.to_string(),
            "a number of seconds from 1 to 31536000",
            longest,
        )?;
        self.distinct_ports()
    }

    /// The two instances, `a` then `b`. Meaningful for validated settings only; a base
    /// too high for the second instance saturates rather than panics.
    pub fn instances(&self) -> [Instance; 2] {
        let at = |base: u16| Instance {
            reapi: base.saturating_add(REAPI_OFFSET),
            worker: base.saturating_add(WORKER_OFFSET),
            api: base.saturating_add(API_OFFSET),
        };
        let a = self.backend_port_base;
        [at(a), at(a.saturating_add(SECOND_INSTANCE))]
    }

    /// Envoy's `--parent-shutdown-time-s`: the longest action plus [`PARENT_SLACK_SECS`].
    pub fn parent_shutdown_secs(&self) -> u64 {
        self.longest_action_secs.saturating_add(PARENT_SLACK_SECS)
    }

    /// The nine ports the box binds on loopback or publicly, none twice.
    fn distinct_ports(&self) -> Result<(), SettingsError> {
        if self.backend_port_base > 65535 - SECOND_INSTANCE - API_OFFSET {
            return Err(SettingsError::BasePastRange(self.backend_port_base));
        }
        let mut named: Vec<(String, u16)> = vec![
            ("reapi_port".to_owned(), self.reapi_port),
            ("worker_port".to_owned(), self.worker_port),
            ("admin_port".to_owned(), self.admin_port),
        ];
        for (name, i) in [("a", self.instances()[0]), ("b", self.instances()[1])] {
            named.push((format!("instance {name} REAPI"), i.reapi));
            named.push((format!("instance {name} worker"), i.worker));
            named.push((format!("instance {name} API"), i.api));
        }
        for (i, (a, pa)) in named.iter().enumerate() {
            if let Some((b, _)) = named[i + 1..].iter().find(|(_, pb)| pb == pa) {
                return Err(SettingsError::PortTwice {
                    a: a.clone(),
                    b: b.clone(),
                    port: *pa,
                });
            }
        }
        Ok(())
    }
}

fn require(
    field: &'static str,
    value: &str,
    want: &'static str,
    ok: bool,
) -> Result<(), SettingsError> {
    ok.then_some(()).ok_or_else(|| SettingsError::Field {
        field,
        value: value.to_owned(),
        want,
    })
}

/// An absolute path of printable characters other than space, quote and backslash.
fn require_path(field: &'static str, value: &str) -> Result<(), SettingsError> {
    let ok = value.starts_with('/')
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    require(
        field,
        value,
        "an absolute path (no spaces, quotes or control characters)",
        ok,
    )
}

/// One DNS label: 1 to 63 lower-case letters, digits and `-`, no `-` at either end.
fn dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && b.first() != Some(&b'-')
        && b.last() != Some(&b'-')
}
