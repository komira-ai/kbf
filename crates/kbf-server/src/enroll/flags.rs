//! The enroll listener's flags on `kbf frontend` (`docs/design/install-and-enrollment.md`,
//! section 8.4). They are the frontend's alone: `kbf-server`'s own command and a storage
//! node do not take them, so a flag that does nothing there is refused by clap.
//!
//! `--enroll-listen` turns enrollment on, and everything else here needs it. With it, a
//! frontend also needs a state store (the registry is in it), `--ca-dir`, one
//! `--frontend-name` at least (the names the listener's certificate carries and no node
//! may claim) and exactly one of `--ca-signer` (production) and `--ca-dev-file-signer`.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Arg, ArgAction, ArgMatches};

use super::ca::SignerChoice;
use super::limits::Limits;
use super::service::NodeSettings;

const LISTEN: &str = "enroll_listen";
const CA_DIR: &str = "ca_dir";
const CA_SIGNER: &str = "ca_signer";
const CA_DEV: &str = "ca_dev_file_signer";
const NAME: &str = "frontend_name";
const LIFETIME: &str = "enroll_cert_lifetime_secs";
const RATE: &str = "enroll_rate_per_minute";
const BURST: &str = "enroll_rate_burst";
const IN_FLIGHT: &str = "enroll_max_in_flight";
const UNENFORCED: &str = "enroll_allow_unenforced_deny";
const WORKER: &str = "enroll_worker_endpoint";
const CAS: &str = "enroll_cas_endpoint";
const STORAGE_PORT: &str = "enroll_storage_port";

/// What `--enroll-listen` and its companions say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollFlags {
    /// Where the enroll listener listens.
    pub listen: SocketAddr,
    /// The CA directory.
    pub ca_dir: PathBuf,
    /// Where the CA key lives.
    pub signer: SignerChoice,
    /// The names this frontend serves, lower-case.
    pub frontend_names: Vec<String>,
    /// How long the certificates the CA issues live, in seconds.
    pub cert_lifetime_secs: i64,
    /// The limits of the listener.
    pub limits: Limits,
    /// Whether the operator accepts that deny rows are not enforced in storage.
    pub allow_unenforced_deny: bool,
    /// What every node is told about the farm.
    pub settings: NodeSettings,
}

fn flag(id: &'static str, long: &'static str, value: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .value_name(value)
        .requires(LISTEN)
        .help(help)
}

/// The flags, to add to the frontend's command.
#[must_use]
pub fn args() -> Vec<Arg> {
    vec![
        Arg::new(LISTEN)
            .long("enroll-listen")
            .value_name("ADDR")
            .value_parser(clap::value_parser!(SocketAddr))
            .requires_all([CA_DIR, "state_store", NAME])
            .help(
                "Serve kbf.enroll.v1 (Enroll and Renew) on this address, over TLS with the \
                 frontend's own certificate from the CA. Off unless given. It needs \
                 --state-store, --ca-dir, --frontend-name, and --ca-signer (or, for \
                 development, --ca-dev-file-signer). While this build does not enforce \
                 deny rows in storage it also needs --enroll-allow-unenforced-deny.",
            ),
        flag(
            CA_DIR,
            "ca-dir",
            "DIR",
            "The CA directory (ca.key mode 0600, ca.pem). With --ca-dev-file-signer an \
             empty directory gets a new CA and a directory holding one is loaded, never \
             overwritten; with --ca-signer it must hold ca.pem, which the signer made.",
        )
        .value_parser(clap::value_parser!(PathBuf)),
        flag(
            CA_SIGNER,
            "ca-signer",
            "SOCKET",
            "The socket of the kbf-signer process that holds the CA key (production). This \
             process holds no CA key.",
        )
        .value_parser(clap::value_parser!(PathBuf))
        .conflicts_with(CA_DEV),
        Arg::new(CA_DEV)
            .long("ca-dev-file-signer")
            .action(ArgAction::SetTrue)
            .requires(LISTEN)
            .help(
                "Keep the CA key as a file in this process (development and tests only; \
                 production uses --ca-signer and refuses to start without it).",
            ),
        flag(
            NAME,
            "frontend-name",
            "NAME",
            "A name this frontend serves, a DNS name or an IP address; repeat it for \
             each. The enroll listener's certificate carries these names, and no node may \
             advertise one or a host under one.",
        )
        .action(ArgAction::Append),
        flag(
            LIFETIME,
            "enroll-cert-lifetime-secs",
            "SECONDS",
            "How long the certificates the CA issues live (3600 to 2592000); a node renews \
             at half. The signer's own --lifetime-secs must be at least this.",
        )
        .value_parser(clap::value_parser!(i64).range(3600..=2_592_000))
        .default_value("604800"),
        flag(
            RATE,
            "enroll-rate-per-minute",
            "N",
            "Enroll and Renew calls per minute from one source address, once its burst is \
             spent. The count is in memory and starts again at a restart or a handover.",
        )
        .value_parser(clap::value_parser!(u32).range(1..))
        .default_value("60"),
        flag(
            BURST,
            "enroll-rate-burst",
            "N",
            "Calls one source address may make at once.",
        )
        .value_parser(clap::value_parser!(u32).range(1..))
        .default_value("30"),
        flag(
            IN_FLIGHT,
            "enroll-max-in-flight",
            "N",
            "Calls the listener works on at once, from all sources; the next is refused \
             `rate limited`.",
        )
        .value_parser(clap::value_parser!(u32).range(1..))
        .default_value("64"),
        Arg::new(UNENFORCED)
            .long("enroll-allow-unenforced-deny")
            .action(ArgAction::SetTrue)
            .requires(LISTEN)
            .help(
                "Allow --enroll-listen although this build's storage does not enforce the \
                 deny rows a node revocation writes: a revoked node is refused renewal at \
                 once but storage serves it until its certificate expires.",
            ),
        flag(
            WORKER,
            "enroll-worker-endpoint",
            "URL",
            "The https URL a daemon dials (--server), told to every node. Empty: none.",
        ),
        flag(
            CAS,
            "enroll-cas-endpoint",
            "URL",
            "What --cas is for a daemon-only node, told to every node. Empty: none.",
        ),
        flag(
            STORAGE_PORT,
            "enroll-storage-port",
            "PORT",
            "The port a storage node serves on, told to every node.",
        )
        .value_parser(clap::value_parser!(u16)),
    ]
}

/// The enroll flags `m` holds, or `None` without `--enroll-listen`.
///
/// # Errors
/// A flag combination clap does not express: a companion flag without `--enroll-listen`
/// is clap's (`requires`); this refuses `--enroll-listen` with neither signer flag, and a
/// `--frontend-name` that is not a name.
pub fn from_matches(m: &ArgMatches) -> Result<Option<EnrollFlags>, String> {
    let Some(listen) = m.get_one::<SocketAddr>(LISTEN).copied() else {
        return Ok(None);
    };
    let signer = match (m.get_one::<PathBuf>(CA_SIGNER), m.get_flag(CA_DEV)) {
        // clap refuses both together.
        (Some(socket), _) => SignerChoice::External(socket.clone()),
        (None, true) => SignerChoice::DevFile,
        (None, false) => {
            return Err(
                "--enroll-listen needs --ca-signer=<socket> (production: the CA key is held \
                 by a kbf-signer process, never by the frontend); --ca-dev-file-signer \
                 keeps the key as a file in this process and is for development and tests"
                    .to_owned(),
            );
        }
    };
    let names: Vec<String> = m
        .get_many::<String>(NAME)
        .into_iter()
        .flatten()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    if let Some(bad) = names.iter().find(|name| !is_name(name)) {
        return Err(format!(
            "--frontend-name {bad}: not a DNS name or an IP address"
        ));
    }
    let number = |id: &str| m.get_one::<u32>(id).copied().unwrap_or(1);
    Ok(Some(EnrollFlags {
        listen,
        ca_dir: m
            .get_one::<PathBuf>(CA_DIR)
            .cloned()
            .ok_or("--enroll-listen needs --ca-dir")?,
        signer,
        frontend_names: names,
        cert_lifetime_secs: m.get_one::<i64>(LIFETIME).copied().unwrap_or(604_800),
        limits: Limits {
            per_minute: number(RATE),
            burst: number(BURST),
            in_flight: number(IN_FLIGHT),
        },
        allow_unenforced_deny: m.get_flag(UNENFORCED),
        settings: NodeSettings {
            worker_endpoint: m.get_one::<String>(WORKER).cloned().unwrap_or_default(),
            cas_endpoint: m.get_one::<String>(CAS).cloned().unwrap_or_default(),
            storage_port: m
                .get_one::<u16>(STORAGE_PORT)
                .map_or(0, |port| u32::from(*port)),
        },
    }))
}

/// Whether `text` is an IP address or a DNS name (lower-case labels of `a-z`, `0-9` and
/// `-`, at most 253 characters).
fn is_name(text: &str) -> bool {
    let label = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    text.parse::<std::net::IpAddr>().is_ok() || (text.len() <= 253 && text.split('.').all(label))
}

#[cfg(test)]
mod tests;
