//! Where the time of a run's preparation and submit goes, step by step:
//! `kbf_executor_step_seconds{step=...}`, beside the phases of [`crate::executor::phases`].
//!
//! Each step is timed on the executor's clock around the one await it names, and observed when
//! that await returns an answer, a failure included. A step whose await is cut short, or that
//! has no answer to time, is not observed: a call or a wait a cancel drops, a `tree` the writer
//! refuses or cannot look up (it has no name), a `row` with no state store, and the closure's
//! parts (`action`, `walk`, `held`, timed by the frontend's preparation) of a closure that was
//! not collected. A step a run does not take is not observed either: a run whose action tree is
//! remembered has no `walk` and no `tree`, and a run whose submit is answered at once has no
//! `backoff`.
//!
//! The steps, in the order a run takes them:
//!
//! - `facts`: the action's facts, the Action and the Command read from the CAS, or remembered;
//! - `put_dir`: the run's put directory from the frontend's preparation, with the input closure
//!   collected when the action tree is not remembered (`action`, `walk` and `held` are its
//!   parts), or the directory alone when it is;
//! - `action`: the closure's read of the Action;
//! - `walk`: the closure's walk of the input root's `Directory` messages;
//! - `held`: the closure's check that the store holds every blob of the closure;
//! - `tree`: the action tree, from the collected closure to the tree's name, its blobs located
//!   and its manifests or tree list built; the writes of the tree are not in it;
//! - `tree_wait`: the run waiting for its tree's writes, before its submit, while its row is
//!   written;
//! - `tree_blobs`, `tree_manifests` and `tree_root`: the stages of the tree's writes as
//!   manifests, timed on the task that writes it, which begins when the tree is named, before
//!   `tree_wait`: the blobs that were nowhere, written whole (a tree with none has no
//!   `tree_blobs`); the input root's manifests the memo did not have and `.kbf`, written
//!   together; and the root, written once both are stored. A tree list is one write and has
//!   none of them;
//! - `row`: the operation's row written to the state store, while the tree's writes end and
//!   before the submit;
//! - `rpc`: one `SubmitJob` call, from the call to its answer or to the end of its limit;
//! - `backoff`: the wait before a submit whose answer was lost is sent again.

use kbf_front::action_tree::WriteStage;

use crate::executor::histogram::{Histograms, Series};

/// One step of a run's preparation or submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The action's facts.
    Facts,
    /// The run's put directory, and the input closure when the tree is not remembered.
    PutDir,
    /// The closure's read of the Action.
    Action,
    /// The closure's walk of the input root.
    Walk,
    /// The closure's check that every blob is held.
    Held,
    /// The action tree up to its name.
    Tree,
    /// The wait for the tree's writes.
    TreeWait,
    /// The tree's blobs that were nowhere, written whole.
    TreeBlobs,
    /// The tree's input manifests and `.kbf`, written together.
    TreeManifests,
    /// The tree's root manifest.
    TreeRoot,
    /// The operation's row in the state store.
    Row,
    /// One `SubmitJob` call.
    Rpc,
    /// The wait before a submit is sent again.
    Backoff,
}

impl Step {
    /// Every step, in the order the metric lists them.
    pub(crate) const ALL: [Self; 13] = [
        Self::Facts,
        Self::PutDir,
        Self::Action,
        Self::Walk,
        Self::Held,
        Self::Tree,
        Self::TreeWait,
        Self::TreeBlobs,
        Self::TreeManifests,
        Self::TreeRoot,
        Self::Row,
        Self::Rpc,
        Self::Backoff,
    ];

    /// The `step` label.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::PutDir => "put_dir",
            Self::Action => "action",
            Self::Walk => "walk",
            Self::Held => "held",
            Self::Tree => "tree",
            Self::TreeWait => "tree_wait",
            Self::TreeBlobs => "tree_blobs",
            Self::TreeManifests => "tree_manifests",
            Self::TreeRoot => "tree_root",
            Self::Row => "row",
            Self::Rpc => "rpc",
            Self::Backoff => "backoff",
        }
    }

    /// The step a stage of an action tree's writes is timed as.
    pub(crate) const fn of_write(stage: WriteStage) -> Self {
        match stage {
            WriteStage::Blobs => Self::TreeBlobs,
            WriteStage::Manifests => Self::TreeManifests,
            WriteStage::Root => Self::TreeRoot,
        }
    }
}

impl Series for Step {
    const NAME: &'static str = "kbf_executor_step_seconds";
    const HELP: &'static str = "How long the steps of an executor run's preparation and submit \
                                took, by step, each timed around its one wait";
    const LABEL: &'static str = "step";
    const ALL: &'static [Self] = &Self::ALL;

    fn label(self) -> &'static str {
        Self::label(self)
    }

    fn slot(self) -> usize {
        self as usize
    }
}

/// `kbf_executor_step_seconds`: a histogram of the steps, one series per step.
pub(crate) type StepHistograms = Histograms<Step>;

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
