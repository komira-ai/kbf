//! The flags of `kbf update-hook`, the ones before the step: the declaration's `hook_args`.
//! Every setting is a flag; none is read from the environment.
//!
//! ```text
//! kbf update-hook --record-dir DIR --channel NAME
//!                 (--enrollment-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
//!                 [--frontend <dns name>:<port>]
//!                 [--operator-api http://IP:PORT --token-file FILE] [--daemon-node NODE_ID]
//!                 [--cordon-dir DIR] [--helper-socket PATH]
//!                 [--drain-deadline-secs N] [--health-timeout-secs N] [--health-poll-secs N]
//!                 [--retry-secs N]
//!                 <step> [<artifact>] [--params FILE]
//! ```
//!
//! They are the flags of `kbf-updater agent` that the update itself reads, with the same
//! meaning and the same defaults (`agent::flags`), except:
//!
//! - `--record-dir` is the hook's own record directory, the declaration's `state_dir`
//!   ([`super::record`]); it is required, and it is locked for the whole of a step.
//! - `--enrollment-dir` is what the agent calls `--state-dir`: the directory `kbf-updater
//!   enroll` filled. The hook never renews the certificate.
//! - `--retry-secs` is the `retry_after_secs` of a step that exits `75`.
//! - `--cordon-dir` defaults to the record directory.
//!
//! Nothing here names the coordinator, the heartbeat or the services of an agent: the updater
//! that runs the hook has already decided which update this is.

use std::path::PathBuf;

use super::super::build::Access;
use super::super::coordinator::Endpoint;
use super::super::flags::{
    DEFAULT_HELPER_SOCKET, NodeApi, Timing, channel_ok, collect, drain_secs, secs, tls_files,
};
use crate::daemon::parse_operator_url;
use crate::inventory::valid_node_id;

const FLAGS: [&str; 16] = [
    "--record-dir",
    "--channel",
    "--enrollment-dir",
    "--client-cert",
    "--client-key",
    "--ca",
    "--frontend",
    "--operator-api",
    "--token-file",
    "--daemon-node",
    "--cordon-dir",
    "--helper-socket",
    "--drain-deadline-secs",
    "--health-timeout-secs",
    "--health-poll-secs",
    "--retry-secs",
];

/// Why the flags before the step were refused. The text names the flag; it is the `detail`
/// of the step's last line (`bad_arguments`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FlagsError {
    /// An argument the agent's own reader refused: one that is no flag of the hook, a flag
    /// without a value, a flag given twice.
    #[error("{0}")]
    Unreadable(String),
    /// A flag the hook cannot run without.
    #[error("{flag} is required")]
    Required { flag: &'static str },
    /// `--channel` is not a channel name.
    #[error("--channel must be a channel name: [a-z0-9][a-z0-9-]{{0,31}}")]
    Channel,
    /// Neither the enrollment directory nor all three files, or both.
    #[error("give --enrollment-dir, or all of --client-cert, --client-key and --ca (and not both)")]
    Tls,
    /// `--frontend` is not `<dns name>:<port>`.
    #[error("--frontend: {0}")]
    Frontend(String),
    /// `--operator-api` is not a loopback `http://IP:PORT`; the text says how.
    #[error("{0}")]
    OperatorApi(String),
    /// One of `--operator-api` and `--token-file` without the other.
    #[error("--operator-api and --token-file are given together, or not at all")]
    OperatorApiAlone,
    /// `--daemon-node` is not a node id.
    #[error("--daemon-node must be a node id: a DNS label")]
    DaemonNode,
    /// A number of seconds out of its range; the text names the flag and the range.
    #[error("{0}")]
    Seconds(String),
}

/// The settings of one hook run.
#[derive(Clone, Debug)]
pub struct Flags {
    /// The hook's record directory.
    pub record_dir: PathBuf,
    /// The release channel the helper's allowlist names.
    pub channel: String,
    /// The socket of `kbf-host-install --serve`.
    pub helper_socket: PathBuf,
    /// How the node is reached at the frontend.
    pub access: Access,
    /// How long, and how often, the update waits for things.
    pub timing: Timing,
}

/// The flags before the step, and the words from the step on.
///
/// A word that starts with `--` is a flag and takes the next word as its value (or has it after
/// an `=`); the first word that does not is the step.
///
/// # Errors
/// What is wrong with the flags, by kind ([`FlagsError`]).
pub fn split(args: &[String]) -> Result<(Flags, &[String]), FlagsError> {
    let mut at = 0;
    while let Some(word) = args.get(at).filter(|w| w.starts_with("--")) {
        at += if word.contains('=') { 1 } else { 2 };
    }
    let (flags, rest) = args.split_at(at.min(args.len()));
    Ok((parse(flags)?, rest))
}

fn parse(args: &[String]) -> Result<Flags, FlagsError> {
    let mut found = collect(args, &FLAGS).map_err(FlagsError::Unreadable)?;
    let mut take = |flag: &'static str| found.remove(flag).ok_or(FlagsError::Required { flag });
    let record_dir = PathBuf::from(take("--record-dir")?);
    let channel = take("--channel")?;
    if !channel_ok(&channel) {
        return Err(FlagsError::Channel);
    }
    let files = tls_files(
        found
            .remove("--enrollment-dir")
            .map(PathBuf::from)
            .as_deref(),
        [
            found.remove("--client-cert"),
            found.remove("--client-key"),
            found.remove("--ca"),
        ],
    )
    .map_err(|_| FlagsError::Tls)?;
    let frontend = found
        .remove("--frontend")
        .map(|text| Endpoint::parse(&text).map_err(FlagsError::Frontend))
        .transpose()?;
    let node_api = match (found.remove("--operator-api"), found.remove("--token-file")) {
        (Some(api), Some(token)) => Some(NodeApi {
            addr: parse_operator_url(&api).map_err(FlagsError::OperatorApi)?,
            token_file: PathBuf::from(token),
        }),
        (None, None) => None,
        _ => return Err(FlagsError::OperatorApiAlone),
    };
    let daemon_node = found.remove("--daemon-node");
    if daemon_node.as_deref().is_some_and(|id| !valid_node_id(id)) {
        return Err(FlagsError::DaemonNode);
    }
    let cordon_dir = found
        .remove("--cordon-dir")
        .map_or_else(|| record_dir.clone(), PathBuf::from);
    let helper_socket = found
        .remove("--helper-socket")
        .map_or_else(|| PathBuf::from(DEFAULT_HELPER_SOCKET), PathBuf::from);
    let mut timing = Timing::DEFAULT;
    if let Some(text) = found.remove("--drain-deadline-secs") {
        timing.drain_deadline =
            Some(drain_secs("--drain-deadline-secs", &text).map_err(FlagsError::Seconds)?);
    }
    for (flag, slot) in [
        ("--health-timeout-secs", &mut timing.health_timeout),
        ("--health-poll-secs", &mut timing.health_poll),
        ("--retry-secs", &mut timing.retry),
    ] {
        if let Some(text) = found.remove(flag) {
            *slot = secs(flag, &text).map_err(FlagsError::Seconds)?;
        }
    }
    Ok(Flags {
        record_dir,
        channel,
        helper_socket,
        access: Access {
            files,
            frontend,
            node_api,
            cordon_dir,
            daemon_node,
        },
        timing,
    })
}
