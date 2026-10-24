//! The flags of `kbf-updater agent`. Every setting is a flag; none is read from the
//! environment.
//!
//! ```text
//! kbf-updater agent --coordinator <dns name>:<port> --client-cert FILE --client-key FILE --ca FILE
//!                   --channel NAME --operator-api http://IP:PORT --token-file FILE
//!                   [--operator-proxy-auth-file FILE] [--helper-socket PATH]
//!                   [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
//!                   [--health-poll-secs N] [--retry-secs N]
//! ```
//!
//! - `--coordinator`: where the coordinator is; its certificate must be for that name.
//! - `--client-cert`, `--client-key`, `--ca`: files an operator placed (enrollment is a later
//!   step): this host's updater certificate and key, and the farm CA that signed the
//!   coordinator's certificate. The certificate says which node this is and what the host
//!   runs; neither is a flag.
//! - `--channel`: the release channel the helper's allowlist names; the version the
//!   coordinator names is looked up in it by the helper, never by URL.
//! - `--helper-socket`: the socket of `kbf-host-install --serve` (default
//!   `/run/kbf-host-install/local.sock`).
//! - `--operator-api`, `--token-file`, `--operator-proxy-auth-file`: the frontend's loopback
//!   operator API, as in the roll mode, for the drain of the node and the node's health.
//!   **There is no scoped credential for these calls yet** (`docs/api.md`: the operator API's
//!   roles are planned), so the token is the full operator token and the API is the frontend's
//!   loopback: the agent updates only where both are within reach.
//! - Times, in seconds: `--heartbeat-secs` (default 15), `--drain-deadline-secs` (600, how long
//!   the node's leases are waited for), `--health-timeout-secs` (300, how long the release on
//!   trial has to show healthy), `--health-poll-secs` (5) and `--retry-secs` (900, how long a
//!   target that failed is left alone before the same order is tried again).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::coordinator::{Endpoint, TlsFiles};
use crate::daemon::parse_operator_url;

pub const USAGE: &str = "usage: kbf-updater agent --coordinator DNSNAME:PORT --client-cert FILE \
--client-key FILE --ca FILE --channel NAME --operator-api http://IP:PORT --token-file FILE \
[--operator-proxy-auth-file FILE] [--helper-socket PATH] [--heartbeat-secs N] \
[--drain-deadline-secs N] [--health-timeout-secs N] [--health-poll-secs N] [--retry-secs N]\n";

const FLAGS: [&str; 14] = [
    "--coordinator",
    "--client-cert",
    "--client-key",
    "--ca",
    "--channel",
    "--operator-api",
    "--token-file",
    "--operator-proxy-auth-file",
    "--helper-socket",
    "--heartbeat-secs",
    "--drain-deadline-secs",
    "--health-timeout-secs",
    "--health-poll-secs",
    "--retry-secs",
];

/// Where the helper listens unless a flag says otherwise.
pub const DEFAULT_HELPER_SOCKET: &str = "/run/kbf-host-install/local.sock";

/// How long, and how often, the agent waits for things.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Between two heartbeats.
    pub heartbeat: Duration,
    /// How long the node's leases are waited for once it is drained.
    pub drain_deadline: Duration,
    /// How often they are looked at.
    pub drain_poll: Duration,
    /// How long a release on trial has to show healthy.
    pub health_timeout: Duration,
    /// How often it is looked at.
    pub health_poll: Duration,
    /// How long a target that failed is left alone.
    pub retry: Duration,
    /// How much of the helper's window is kept for going back: the health wait ends this
    /// long before the helper's own deadline.
    pub margin: Duration,
}

impl Timing {
    pub const DEFAULT: Self = Self {
        heartbeat: Duration::from_secs(15),
        drain_deadline: Duration::from_secs(600),
        drain_poll: Duration::from_secs(5),
        health_timeout: Duration::from_secs(300),
        health_poll: Duration::from_secs(5),
        retry: Duration::from_secs(900),
        margin: Duration::from_secs(10),
    };
}

/// The settings of an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub coordinator: Endpoint,
    pub files: TlsFiles,
    pub channel: String,
    pub helper_socket: PathBuf,
    pub operator_api: SocketAddr,
    pub token_file: PathBuf,
    pub operator_proxy_auth_file: Option<PathBuf>,
    pub timing: Timing,
}

/// A number of seconds from 1 to a day.
fn secs(flag: &str, text: &str) -> Result<Duration, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| (1..=86_400).contains(n))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{flag} must be a number of seconds from 1 to 86400"))
}

/// Read the flags (after `agent`). Each is given once, as `--flag value` or `--flag=value`.
///
/// # Errors
/// What is wrong with them, in a line.
pub fn parse_flags(args: &[String]) -> Result<Config, String> {
    let mut found: BTreeMap<&str, String> = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&flag) = FLAGS.iter().find(|f| **f == name) else {
            return Err(format!("unknown argument {name}"));
        };
        let value = match inline {
            Some(value) => value,
            None => rest
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))?,
        };
        if found.insert(flag, value).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let coordinator =
        Endpoint::parse(&take("--coordinator")?).map_err(|why| format!("--coordinator: {why}"))?;
    let files = TlsFiles {
        cert: PathBuf::from(take("--client-cert")?),
        key: PathBuf::from(take("--client-key")?),
        ca: PathBuf::from(take("--ca")?),
    };
    let channel = take("--channel")?;
    if !channel_ok(&channel) {
        return Err("--channel must be a channel name: [a-z0-9][a-z0-9-]{0,31}".to_owned());
    }
    let operator_api = parse_operator_url(&take("--operator-api")?)?;
    let token_file = PathBuf::from(take("--token-file")?);
    let operator_proxy_auth_file = found
        .remove("--operator-proxy-auth-file")
        .map(PathBuf::from);
    let helper_socket = found
        .remove("--helper-socket")
        .map_or_else(|| PathBuf::from(DEFAULT_HELPER_SOCKET), PathBuf::from);
    let mut timing = Timing::DEFAULT;
    for (flag, slot) in [
        ("--heartbeat-secs", &mut timing.heartbeat),
        ("--drain-deadline-secs", &mut timing.drain_deadline),
        ("--health-timeout-secs", &mut timing.health_timeout),
        ("--health-poll-secs", &mut timing.health_poll),
        ("--retry-secs", &mut timing.retry),
    ] {
        if let Some(text) = found.remove(flag) {
            *slot = secs(flag, &text)?;
        }
    }
    Ok(Config {
        coordinator,
        files,
        channel,
        helper_socket,
        operator_api,
        token_file,
        operator_proxy_auth_file,
        timing,
    })
}

/// The grammar of a channel name, as the helper checks it (`[a-z0-9][a-z0-9-]{0,31}`).
fn channel_ok(text: &str) -> bool {
    let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let Some((first, rest)) = text.split_at_checked(1) else {
        return false;
    };
    text.len() <= 32
        && first.bytes().all(lower_or_digit)
        && rest.bytes().all(|b| lower_or_digit(b) || b == b'-')
}
