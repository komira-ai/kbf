//! The flags of `kbf-updater agent`. Every setting is a flag; none is read from the
//! environment.
//!
//! ```text
//! kbf-updater agent --coordinator <dns name>:<port> --channel NAME
//!                   (--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
//!                   [--frontend <dns name>:<port>] [--enroll-frontend <dns name>:<port>]
//!                   [--operator-api http://IP:PORT --token-file FILE] [--daemon-node NODE_ID]
//!                   [--cordon-dir DIR]
//!                   [--helper-socket PATH] [--service NAME,helper-socket=PATH[,SETTING=VALUE...]]...
//!                   [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
//!                   [--health-poll-secs N] [--retry-secs N]
//! ```
//!
//! - `--coordinator`: where the coordinator is; its certificate must be for that name.
//! - `--state-dir`: the directory `kbf-updater enroll` filled (the key, the certificate and the
//!   CA are its files, [`kbf_enroll::store::Store`]'s). The agent renews the certificate at half
//!   its life and writes the new one there; it needs `--enroll-frontend` for that.
//! - `--client-cert`, `--client-key`, `--ca`: instead, files an operator placed: this host's
//!   updater certificate and key, and the farm CA that signed the coordinator's certificate.
//!   Nothing renews them. The certificate says which node this is and what the host runs;
//!   neither is a flag.
//! - `--frontend`: the frontend's worker listener, `<dns name>:<port>` (its `--worker-listen`;
//!   the name must be one its certificate covers). The node's drain, its undrain, its status
//!   and, for a storage host, its own storage health are four calls there, made over mutual
//!   TLS with the updater's own certificate and no token: the frontend serves them for the node
//!   in the certificate and for no other, and refuses everything else from an updater.
//! - `--enroll-frontend`: the frontend's enroll listener (its `--enroll-listen`), the address
//!   `kbf-updater enroll --frontend` was given. The agent renews its certificate there, with
//!   the key it enrolled with; the listener asks for no client certificate, so a host whose
//!   certificate lapsed during an outage still recovers.
//! - `--daemon-node`: for the agent of a frontend host that also runs a daemon: the daemon's
//!   node id. The frontend's update drains that node around it (the frontend itself is drained
//!   by the helper, inside the A/B); without the flag a frontend host's node is not drained.
//! - `--cordon-dir`: where the agent records, one file per node and unit, that a cordon is its
//!   own, so that a retry or a restart gives back a node it drained and leaves out one an
//!   operator drained; the agent drops the marks of a node it reads serving (a cordon given back
//!   by hand). Default: `--state-dir`, else the directory `--client-key` is in. It must be
//!   writable by the agent, which checks at start and does not start otherwise.
//! - `--channel`: the release channel the helper's allowlist names; the version the
//!   coordinator names is looked up in it by the helper, never by URL.
//! - `--helper-socket`: the socket of `kbf-host-install --serve` (default
//!   `/run/kbf-host-install/local.sock`).
//! - `--operator-api`, `--token-file`: the frontend's loopback
//!   operator API, as in the roll mode, as an **opt-in fallback** for the drain and the
//!   node's health: the credential is the full operator token, which the design keeps off
//!   hosts. Giving it to an agent is a choice of the operator, made by giving both
//!   `--operator-api` and `--token-file`, and it is then used
//!   instead of the updater's certificate for those calls. Without either way (no `--frontend`
//!   and no token) the agent heartbeats and refuses every update order, before it fetches
//!   anything. The packaged units give no token.
//! - `--service NAME,helper-socket=PATH[,channel=C][,drain=node|none][,ready-url=URL]`, as
//!   many times as the host runs services besides `kbf`: the agent lists each in its heartbeat
//!   and runs an order that names it with the service's own ports ([`ServiceConfig`]). Each
//!   names its own helper, which may not be the agent's `--helper-socket`: an install request
//!   names no service, so the agent's helper would install `kbf` at the service's version.
//!   Without the flag the agent is a `kbf`-only agent, as before.
//! - Times, in seconds: `--heartbeat-secs` (default 15), `--drain-deadline-secs` (no default, 1 or
//!   more with no upper bound: without it the node's leases are waited for however long they take;
//!   a deadline that passes fails the update with the node left drained and nothing switched), `--health-timeout-secs` (300, how long the release on
//!   trial has to show healthy), `--health-poll-secs` (5) and `--retry-secs` (900, how long a
//!   target that failed is left alone before the same order is tried again).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_enroll::store::Store;
use kbf_types::Role;

use super::coordinator::{Endpoint, TlsFiles};
use crate::daemon::parse_operator_url;
use crate::inventory::valid_node_id;
use crate::wire::{KBF_SERVICE, MAX_SERVICE_NAME, is_service_name};

pub const USAGE: &str = "usage: kbf-updater agent --coordinator DNSNAME:PORT --channel NAME \
(--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE) [--frontend DNSNAME:PORT] \
[--enroll-frontend DNSNAME:PORT] \
[--operator-api http://IP:PORT --token-file FILE] \
[--daemon-node NODE_ID] [--cordon-dir DIR] [--helper-socket PATH] [--service NAME,helper-socket=PATH[,SETTING=VALUE...]]... [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N] \
[--health-poll-secs N] [--retry-secs N]\n";

const FLAGS: [&str; 18] = [
    "--coordinator",
    "--state-dir",
    "--frontend",
    "--enroll-frontend",
    "--client-cert",
    "--client-key",
    "--ca",
    "--channel",
    "--operator-api",
    "--token-file",
    "--daemon-node",
    "--cordon-dir",
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
    /// How long the node's leases are waited for once it is drained; `None`, the default,
    /// waits however long they take. When a set time is out the host is not switched.
    pub drain_deadline: Option<Duration>,
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
        drain_deadline: None,
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
    /// The frontend's worker listener: where the node's own drain, status and storage health
    /// are asked.
    pub frontend: Option<Endpoint>,
    /// The frontend's enroll listener: where the certificate is renewed.
    pub enroll_frontend: Option<Endpoint>,
    /// The enrollment's state directory, if the certificate is the agent's to renew.
    pub state_dir: Option<PathBuf>,
    /// Where the agent keeps its marks of the cordons it made (`--cordon-dir`); when absent, the
    /// state directory, else the directory of the key file.
    pub cordon_dir: Option<PathBuf>,
    /// The frontend's operator API for the drain and the health, if the operator gave the agent
    /// the credential for it.
    pub node_api: Option<NodeApi>,
    /// The node id of the daemon a frontend host also runs, if it runs one.
    pub daemon_node: Option<String>,
    pub timing: Timing,
    /// The services the host runs besides `kbf`, in the order they were given.
    pub services: Vec<ServiceConfig>,
}

/// How a service's node is drained around its update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceDrain {
    /// The node is drained through the frontend, as `kbf`'s is.
    Node,
    /// There is nothing to drain: the service is replaced in place.
    None,
}

/// How a service's health is judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceHealth {
    /// By the node's health through the frontend, as `kbf`'s is.
    Node,
    /// Healthy when `GET path` on the loopback address answers 2xx.
    Ready { addr: SocketAddr, path: String },
}

/// A service the host runs besides `kbf`, and the ports its A/B uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceConfig {
    pub name: String,
    /// The channel of the helper's allowlist that holds the service's releases.
    pub channel: String,
    /// The helper that installs it: its own, never the agent's `--helper-socket`.
    pub helper_socket: PathBuf,
    pub drain: ServiceDrain,
    pub health: ServiceHealth,
}

/// The frontend's loopback operator API and the credential for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeApi {
    pub addr: SocketAddr,
    pub token_file: PathBuf,
}

/// The `--flag value` and `--flag=value` pairs of `args`, each flag of `known` given at most
/// once.
///
/// # Errors
/// An argument that is no known flag, a flag with no value, a flag given twice.
pub(crate) fn collect<'k>(
    args: &[String],
    known: &[&'k str],
) -> Result<BTreeMap<&'k str, String>, String> {
    let mut found = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&flag) = known.iter().find(|f| **f == name) else {
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
    Ok(found)
}

/// A number of seconds from 1 to a day.
pub(super) fn secs(flag: &str, text: &str) -> Result<Duration, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| (1..=86_400).contains(n))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{flag} must be a number of seconds from 1 to 86400"))
}

/// A drain deadline: a number of seconds from 1 up, with no upper bound. Absent is no deadline.
pub(super) fn drain_secs(flag: &str, text: &str) -> Result<Duration, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| *n >= 1)
        .map(Duration::from_secs)
        .ok_or_else(|| {
            format!(
                "{flag} must be a number of seconds, 1 or more (leave it out to wait however \
                 long it takes)"
            )
        })
}

/// Read the flags (after `agent`). Each is given once, as `--flag value` or `--flag=value`.
///
/// # Errors
/// What is wrong with them, in a line.
pub fn parse_flags(args: &[String]) -> Result<Config, String> {
    let (service_values, rest) = split_services(args);
    let services = parse_services(&service_values)?;
    let mut found = collect(&rest, &FLAGS)?;
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let coordinator =
        Endpoint::parse(&take("--coordinator")?).map_err(|why| format!("--coordinator: {why}"))?;
    let channel = take("--channel")?;
    if !channel_ok(&channel) {
        return Err("--channel must be a channel name: [a-z0-9][a-z0-9-]{0,31}".to_owned());
    }
    let frontend = found
        .remove("--frontend")
        .map(|text| Endpoint::parse(&text).map_err(|why| format!("--frontend: {why}")))
        .transpose()?;
    let enroll_frontend = found
        .remove("--enroll-frontend")
        .map(|text| Endpoint::parse(&text).map_err(|why| format!("--enroll-frontend: {why}")))
        .transpose()?;
    let state_dir = found.remove("--state-dir").map(PathBuf::from);
    let files = tls_files(
        state_dir.as_deref(),
        [
            found.remove("--client-cert"),
            found.remove("--client-key"),
            found.remove("--ca"),
        ],
    )?;
    if state_dir.is_some() && enroll_frontend.is_none() {
        return Err(
            "--state-dir needs --enroll-frontend: the certificate is renewed there".to_owned(),
        );
    }
    let api = found.remove("--operator-api");
    let token = found.remove("--token-file");
    let node_api = match (api, token) {
        (Some(api), Some(token)) => Some(NodeApi {
            addr: parse_operator_url(&api)?,
            token_file: PathBuf::from(token),
        }),
        (None, None) => None,
        _ => {
            return Err(
                "--operator-api and --token-file are given together, or not at all".to_owned(),
            );
        }
    };
    let daemon_node = found.remove("--daemon-node");
    if daemon_node.as_deref().is_some_and(|id| !valid_node_id(id)) {
        return Err("--daemon-node must be a node id: a DNS label".to_owned());
    }
    let cordon_dir = found.remove("--cordon-dir").map(PathBuf::from);
    let helper_socket = found
        .remove("--helper-socket")
        .map_or_else(|| PathBuf::from(DEFAULT_HELPER_SOCKET), PathBuf::from);
    if let Some(service) = services.iter().find(|s| s.helper_socket == helper_socket) {
        return Err(format!(
            "--service {}: helper-socket is the agent's --helper-socket, whose helper installs \
             kbf; give the service its own",
            service.name
        ));
    }
    let mut timing = Timing::DEFAULT;
    if let Some(text) = found.remove("--drain-deadline-secs") {
        timing.drain_deadline = Some(drain_secs("--drain-deadline-secs", &text)?);
    }
    for (flag, slot) in [
        ("--heartbeat-secs", &mut timing.heartbeat),
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
        frontend,
        enroll_frontend,
        state_dir,
        cordon_dir,
        node_api,
        daemon_node,
        timing,
        services,
    })
}

/// The values of the `--service` flags of `args` (the one flag that may be given more than once),
/// and the arguments that are not them.
fn split_services(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut values = Vec::new();
    let mut rest = Vec::new();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if let Some(value) = word.strip_prefix("--service=") {
            values.push(value.to_owned());
        } else if word == "--service" {
            // A missing value is an empty one, which `parse_services` refuses.
            values.push(words.next().cloned().unwrap_or_default());
        } else {
            rest.push(word.clone());
        }
    }
    (values, rest)
}

/// The services the `--service` values name.
fn parse_services(values: &[String]) -> Result<Vec<ServiceConfig>, String> {
    let mut services: Vec<ServiceConfig> = Vec::new();
    for value in values {
        let service = parse_service(value)?;
        if services.iter().any(|s| s.name == service.name) {
            return Err(format!("--service {} is given twice", service.name));
        }
        if let Some(other) = services
            .iter()
            .find(|s| s.helper_socket == service.helper_socket)
        {
            return Err(format!(
                "--service {}: helper-socket is also {}'s; each service needs a helper of its own",
                service.name, other.name
            ));
        }
        services.push(service);
    }
    Ok(services)
}

/// One `--service` value: `NAME` and `,key=value` settings, each key at most once.
fn parse_service(value: &str) -> Result<ServiceConfig, String> {
    let mut parts = value.split(',');
    let name = parts.next().unwrap_or_default();
    if !is_service_name(name) || name == KBF_SERVICE {
        return Err(format!(
            "--service must start with a service name other than {KBF_SERVICE}: lowercase \
             letters, digits and -, at most {MAX_SERVICE_NAME} bytes"
        ));
    }
    let mut settings: BTreeMap<&str, &str> = BTreeMap::new();
    for part in parts {
        let (key, setting) = part
            .split_once('=')
            .ok_or_else(|| format!("--service {name}: {part:?} is not key=value"))?;
        if settings.insert(key, setting).is_some() {
            return Err(format!("--service {name}: {key} is given twice"));
        }
    }
    let mut take = |key: &str| settings.remove(key);
    let channel = take("channel").unwrap_or(name).to_owned();
    if !channel_ok(&channel) {
        return Err(format!(
            "--service {name}: the channel must be a channel name ([a-z0-9][a-z0-9-]{{0,31}}); \
             give one with channel="
        ));
    }
    let helper_socket = PathBuf::from(take("helper-socket").ok_or_else(|| {
        format!("--service {name}: helper-socket= is required, the socket of its own helper")
    })?);
    if !helper_socket.is_absolute() {
        return Err(format!(
            "--service {name}: helper-socket must be an absolute path"
        ));
    }
    if helper_socket
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "--service {name}: helper-socket may not hold a .. component"
        ));
    }
    let drain = match take("drain") {
        None | Some("node") => ServiceDrain::Node,
        Some("none") => ServiceDrain::None,
        Some(_) => return Err(format!("--service {name}: drain is node or none")),
    };
    let health = match take("ready-url") {
        None => ServiceHealth::Node,
        Some(url) => {
            let (addr, path) =
                parse_ready_url(url).map_err(|why| format!("--service {name}: ready-url {why}"))?;
            ServiceHealth::Ready { addr, path }
        }
    };
    if let Some(key) = settings.keys().next() {
        return Err(format!("--service {name}: unknown setting {key}"));
    }
    Ok(ServiceConfig {
        name: name.to_owned(),
        channel,
        helper_socket,
        drain,
        health,
    })
}

/// `http://<loopback ip>:<port>/<path>`: the address and the path (`/` when there is none).
fn parse_ready_url(url: &str) -> Result<(SocketAddr, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("must start with http:// (the call travels in clear, on loopback)")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    let addr: SocketAddr = authority
        .parse()
        .map_err(|_| "must be http://<ip>:<port>/<path>, with an IP address")?;
    if !addr.ip().to_canonical().is_loopback() {
        return Err("must be a loopback address".to_owned());
    }
    if !path.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("has a path with a space or a control character".to_owned());
    }
    Ok((addr, path.to_owned()))
}

/// The files of this host's TLS: those of `state_dir` (`kbf-updater enroll` made them), or the
/// three an operator placed, and never a mixture.
pub(super) fn tls_files(
    state_dir: Option<&Path>,
    placed: [Option<String>; 3],
) -> Result<TlsFiles, String> {
    match (state_dir, placed) {
        (Some(dir), [None, None, None]) => {
            let store = Store::unopened(dir);
            Ok(TlsFiles {
                cert: store.cert_path(Role::Updater),
                key: store.key_path(Role::Updater),
                ca: store.ca_path(),
            })
        }
        (None, [Some(cert), Some(key), Some(ca)]) => Ok(TlsFiles {
            cert: PathBuf::from(cert),
            key: PathBuf::from(key),
            ca: PathBuf::from(ca),
        }),
        _ => Err(
            "give --state-dir, or all of --client-cert, --client-key and --ca (and not \
                  both)"
                .to_owned(),
        ),
    }
}

/// The grammar of a channel name, as the helper checks it (`[a-z0-9][a-z0-9-]{0,31}`).
pub(super) fn channel_ok(text: &str) -> bool {
    let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let Some((first, rest)) = text.split_at_checked(1) else {
        return false;
    };
    text.len() <= 32
        && first.bytes().all(lower_or_digit)
        && rest.bytes().all(|b| lower_or_digit(b) || b == b'-')
}
