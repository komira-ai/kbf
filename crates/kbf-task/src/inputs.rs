//! Where a run's inputs come from: what every kind of job hands the run ([`Inputs`]), and
//! today's job's way of getting them, through a read grant ([`fetch_inputs`]). A tree job's way
//! is [`crate::tree`].

use std::path::PathBuf;
use std::time::Instant;

use kbf_proto::reapi::{Action, Command};
use kbf_task_pack::InputStats;

use crate::digest;
use crate::fetch::{BlobSource, fetch_message, materialize};
use crate::network;
use crate::sandbox;
use crate::task::{Cli, Failure, millis, named};

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
    /// The directories the sandbox hides from the Command ([`sandbox::layout`]).
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

/// Today's job: the Action, its Command and the input tree read through `source`, the tree laid
/// out under `<work-dir>/in`, and the run's directory `<work-dir>` itself.
pub(crate) fn fetch_inputs(cli: &Cli, source: &dyn BlobSource) -> Result<Inputs, Failure> {
    let fetching = Instant::now();
    let action_digest = digest::to_reapi(&cli.action);
    let action: Action = fetch_message(source, &action_digest)?;
    let command: Command =
        fetch_message(source, &named("command", action.command_digest.as_ref())?)?;
    // Read before the inputs are fetched: a refused property costs no download.
    let network = network_of(cli, &action, &command)?;
    let input_root = named("input root", action.input_root_digest.as_ref())?;
    let root = cli.work_dir.join("in");
    materialize(
        source,
        &input_root,
        &root,
        usize::from(cli.fetch_concurrency),
    )?;
    let fetch_ms = millis(fetching.elapsed());
    if let Some(report) = source.report() {
        eprintln!("kbf-task: {report}");
    }
    Ok(Inputs {
        action,
        command,
        network,
        root,
        run_dir: cli.work_dir.clone(),
        hidden: sandbox::hidden_for(&cli.work_dir),
        fetch_ms,
        stats: None,
    })
}
