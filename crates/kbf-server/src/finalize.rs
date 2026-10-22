//! A storage node's side of `kbf cluster finalize` (`docs/api.md`, `POST /v1/finalize`).
//!
//! The frontend's answer to a registration and to every heartbeat may carry `finalize_to`,
//! the metadata-log machine version the operator asked the farm's logs to be raised to
//! ([`crate::members::Members::finalize_target`]). The [`crate::join::Joiner`] keeps the
//! highest it was told in a [`FinalizeTarget`], and [`run`], a task of the node, acts on it
//! through [`apply`]:
//!
//! * a log below the target, in a build that runs the target, is raised to it
//!   ([`RaftMetaLog::raise_up_to`] with the target as the ceiling, not the start ceiling that
//!   holds a start back), one line is logged, and the node's next heartbeat reports the new
//!   `log_version` (the joiner reads the log's version handle at each call);
//! * a log at the target or past it is left as it is;
//! * a target past what the build runs is logged and ignored: the frontend refuses to write
//!   one its own build does not run, so it comes from a frontend newer than this node, and
//!   this node's own build could not read its log after such a raise.
//!
//! A node restarted later is told the target when it registers, and raises then. A node with
//! no frontend has no one to tell it: it raises only when started with `--finalize-log`
//! ([`at_start`]). A raise that fails is not tried again until the target rises or the node
//! restarts.
//!
//! The raise is one-way, and so is this: the target only rises, and nothing here lowers a
//! log.

use std::num::NonZeroU32;
use std::sync::Arc;

use clap::{Arg, ArgMatches};
use kbf_front::Cache;
use kbf_log::MachineVersion;
use kbf_objstore::ObjectStore;
use kbf_store::Fs;
use tokio::sync::watch;

use crate::join::{Joined, Joiner};
use crate::raft_log::{RUNS, RaftMetaLog};
use crate::start::AbortOnDrop;

/// The id of `--finalize-log`.
const FLAG: &str = "finalize_log";

/// The highest `finalize_to` a node was told, shared between its [`crate::join::Joiner`],
/// which learns it from the frontend's answers, and [`run`], which acts on it. 0 is "nothing
/// asked".
#[derive(Debug)]
pub struct FinalizeTarget {
    to: watch::Sender<u32>,
}

impl FinalizeTarget {
    /// A node that was told nothing.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            to: watch::Sender::new(0),
        })
    }

    /// The `finalize_to` of a frontend's answer: kept if it is above what the node holds. An
    /// answer that names none (a farm that never finalized, or a frontend of an older build)
    /// changes nothing, and neither does a lower one: the target only rises.
    pub fn learn(&self, to: Option<u32>) {
        let Some(to) = to else { return };
        let _changed = self.to.send_if_modified(|held| {
            let rises = to > *held;
            if rises {
                *held = to;
            }
            rises
        });
    }

    /// The highest target the node was told; 0 if none.
    #[must_use]
    pub fn target(&self) -> u32 {
        *self.to.borrow()
    }

    /// A receiver that wakes when the target rises.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u32> {
        self.to.subscribe()
    }
}

/// What a node does with a target, given where its log is and what its build runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing: the log is at the target or past it.
    Current,
    /// Raise the log to this version.
    Raise(MachineVersion),
    /// Nothing: the target is past what the build runs.
    Beyond,
}

/// What `target` asks of a log at version `at` in a build that runs up to `runs`: nothing if
/// the log is there already (or the target is 0), nothing if the build does not run the
/// target, else a raise to it. Pure.
#[must_use]
pub fn decide(target: u32, at: u32, runs: u32) -> Decision {
    let Some(to) = NonZeroU32::new(target) else {
        return Decision::Current;
    };
    if target <= at {
        Decision::Current
    } else if target > runs {
        Decision::Beyond
    } else {
        Decision::Raise(MachineVersion::new(to))
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The log was at `from` and is at `to`.
    Raised {
        /// The version before.
        from: u32,
        /// The version after: the target.
        to: u32,
    },
    /// The log is at the target or past it; nothing was committed.
    Current,
    /// The target is past `runs`, the newest version this build runs; the log is untouched.
    Beyond {
        /// The newest version this build runs.
        runs: u32,
    },
    /// The log refused the raise (it has stopped, or its disk failed); it is as it was.
    Failed(String),
}

/// Raises `log` to `target` if [`decide`] says to, in a build that runs up to `runs`.
pub fn apply<F>(log: &RaftMetaLog<F>, target: u32, runs: u32) -> Outcome
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    let from = log.machine_version().get();
    match decide(target, from, runs) {
        Decision::Current => Outcome::Current,
        Decision::Beyond => Outcome::Beyond { runs },
        Decision::Raise(to) => match log.raise_up_to(to, to) {
            Ok(()) => {
                let now = to.get();
                Outcome::Raised { from, to: now }
            }
            Err(e) => Outcome::Failed(e.to_string()),
        },
    }
}

/// Says what became of a raise to `target`: one line when the log was raised, a warning when
/// it could not be or the target is past this build, nothing when it was there already.
pub fn log_outcome(target: u32, outcome: &Outcome) {
    match outcome {
        Outcome::Raised { from, to } => {
            tracing::info!(from, to, "the metadata log was raised (finalize)");
        }
        Outcome::Current => {}
        Outcome::Beyond { runs } => {
            tracing::warn!(
                finalize_to = target,
                runs,
                "the finalize target is past what this build runs; ignored"
            );
        }
        Outcome::Failed(why) => {
            tracing::warn!(
                finalize_to = target,
                %why,
                "the finalize raise failed; tried again when the target next rises"
            );
        }
    }
}

/// `--finalize-log VERSION`: raises `log` to `version` before the node serves, for a node
/// with no frontend to tell it. A log already there is left as it is.
///
/// # Errors
/// `version` is past what this build runs, or the log refused the raise.
pub fn at_start<F>(log: &RaftMetaLog<F>, version: u32) -> Result<(), String>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    let outcome = apply(log, version, RUNS.get());
    log_outcome(version, &outcome);
    match outcome {
        Outcome::Raised { .. } | Outcome::Current => Ok(()),
        Outcome::Beyond { runs } => {
            let why = format!("this build runs log versions up to {runs}");
            Err(format!("--finalize-log {version}: {why}"))
        }
        Outcome::Failed(why) => Err(format!("--finalize-log {version}: {why}")),
    }
}

/// `--finalize-log VERSION`, which a node with no frontend (`conflicts`: the id of its
/// `--frontend`) is started with to raise its log ([`at_start`]).
pub(crate) fn flag(conflicts: &'static str) -> Arg {
    Arg::new(FLAG)
        .long("finalize-log")
        .value_name("VERSION")
        .conflicts_with(conflicts)
        .value_parser(clap::value_parser!(u32).range(1..=i64::from(RUNS.get())))
        .help(
            "For a node with no --frontend: raises the metadata log to this machine \
             version before serving, if it is below. One-way: an older kbf no longer \
             reads the log. At most the newest version this build runs. A registered \
             node is raised by kbf cluster finalize instead.",
        )
}

/// The version `--finalize-log` names in `matches`, if it was given.
pub(crate) fn flag_of(matches: &ArgMatches) -> Option<u32> {
    matches.get_one::<u32>(FLAG).copied()
}

/// [`at_start`] for the version `--finalize-log` named, if it did.
///
/// # Errors
/// As [`at_start`].
pub(crate) fn at_start_if<F>(log: &RaftMetaLog<F>, version: Option<u32>) -> Result<(), String>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    version.map_or(Ok(()), |version| at_start(log, version))
}

/// The task of a node that registered with a frontend (`joiner`) that raises `cache`'s log to
/// the target the frontend names ([`run`]); none for a node with no frontend.
pub(crate) fn spawn<F, O>(
    cache: &Arc<Cache<RaftMetaLog<F>, O>>,
    joiner: Option<&(Joiner, Joined)>,
) -> Option<AbortOnDrop>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
    O: ObjectStore,
{
    joiner.map(|(joiner, _)| {
        let raising = run(Arc::clone(cache), joiner.finalize());
        AbortOnDrop(tokio::spawn(raising))
    })
}

/// Raises the log of `cache` to every target `state` learns, for as long as the task lives:
/// to the one the node already holds when the task starts (the registration that came before
/// the cache opened brought it), and again each time it rises. A raise that fails is logged
/// and tried again when the target next rises, or at the node's next start.
pub async fn run<F, O>(cache: Arc<Cache<RaftMetaLog<F>, O>>, state: Arc<FinalizeTarget>)
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
    O: ObjectStore,
{
    let mut asked = state.subscribe();
    loop {
        let target = *asked.borrow_and_update();
        if target > 0 {
            let log = Arc::clone(&cache);
            let raise = move || apply(log.meta(), target, RUNS.get());
            let raised = tokio::task::spawn_blocking(raise).await;
            let outcome = raised.unwrap_or_else(|e| Outcome::Failed(format!("raise task: {e}")));
            log_outcome(target, &outcome);
        }
        // Never an error: this task holds `state`, and `state` holds the sender.
        let _ = asked.changed().await;
    }
}
