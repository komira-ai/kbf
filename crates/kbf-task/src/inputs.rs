//! Where a run's inputs come from: what the tree job hands the run ([`Inputs`], read by
//! [`crate::tree`]), and the network its Command gets ([`network_of`]).

use std::path::PathBuf;

use kbf_proto::reapi::{Action, Command};
use kbf_task_pack::InputStats;

use crate::network;
use crate::task::{Cli, Failure};

/// What a run starts from, however its inputs reached it.
pub(crate) struct Inputs {
    /// The Action, checked against `--action` and decoded.
    pub(crate) action: Action,
    /// Its Command, checked against the Action's `command_digest` and decoded.
    pub(crate) command: Command,
    /// Whether the Command may use the network.
    pub(crate) network: network::Network,
    /// The input root, laid out: the Command's paths are relative to it, and the outputs are
    /// collected from it.
    pub(crate) root: PathBuf,
    /// The run's own directory, outside `root`: its `stdout`, `stderr` and `private/`.
    pub(crate) run_dir: PathBuf,
    /// The directories the sandbox hides from the Command (none in a tree job, whose job is the
    /// sandbox).
    pub(crate) hidden: Vec<PathBuf>,
    /// How long reading the inputs took, in milliseconds (`timing.fetch_ms`).
    pub(crate) fetch_ms: u64,
    /// The manifest's advisory `inputs`, if the run says.
    pub(crate) stats: Option<InputStats>,
}

/// The network the Command gets, from the Action's and the Command's platform properties, with
/// the line that says so when the action asked for it.
pub(crate) fn network_of(
    cli: &Cli,
    action: &Action,
    command: &Command,
) -> Result<network::Network, Failure> {
    let network = network::from_platform(action, command)?;
    if network == network::Network::Allowed {
        eprintln!("{}", network::allowed_notice(&cli.action));
    }
    Ok(network)
}
