//! Where the time of a run's preparation and submit goes, step by step:
//! `kbf_executor_step_seconds{step=...,form=...}`, beside the phases of
//! [`crate::executor::phases`].
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
//! Every step is labelled with the form of the run's action tree, `form`: `manifests` or
//! `provider` (its tree list), as the job's layer names it ([`InputTree`]); `facts`, taken before
//! the form is chosen (a Mac action's facts choose it), is `none`. A step of one form is not
//! counted under the other, so the two forms' trees are told apart.
//!
//! The steps, in the order a run takes them:
//!
//! - `facts`: the action's facts: from the Action and the Command the Execute that opened the
//!   operation decoded, remembered, or, for an operation taken up from another process, read
//!   from the CAS;
//! - `put_dir`: the run's put directory from the frontend's preparation, with the input closure
//!   collected when the action tree is not remembered (`action`, `walk` and `held` are its
//!   parts), or the directory alone when it is;
//! - `action`: the closure's Action: next to nothing when it is the one the Execute decoded, a
//!   read of the CAS when it is not;
//! - `walk`: the closure's walk of the input root's `Directory` messages;
//! - `held`: the closure's check that the store holds every blob of the closure, next to zero
//!   when the Execute that started the run found them all held moments before and the store is
//!   not asked again (`kbf_prepare_held_vouched_total` counts those);
//! - `tree`: the action tree, from the collected closure to the tree's name, its blobs located
//!   and its manifests or tree list built; the writes of the tree are not in it;
//! - `tree_wait`: the run waiting for its tree's writes, before its submit, while its row is
//!   written;
//! - `tree_blobs`, `tree_manifests` and `tree_root`: the stages of the tree's writes as
//!   manifests, timed on the task that writes it, which begins when the tree is named, before
//!   `tree_wait`: the blobs that were nowhere, written whole (a tree with none has no
//!   `tree_blobs`); the input root's manifests the memo did not have and `.kbf`, written
//!   together; and the root, written once both are stored;
//! - `tree_blobs`, `tree_list` and `tree_sidecar`: the stages of the tree's writes as a tree
//!   list, on the same task: the blobs that were nowhere, as for manifests; the list's one write;
//!   and its L2 sidecar, from its start beside the list's write to its end after it (a list whose
//!   write failed has no `tree_sidecar`);
//! - `row`: the operation's row written to the state store, while the tree's writes end and
//!   before the submit;
//! - `rpc`: one `SubmitJob` call, from the call to its answer or to the end of its limit;
//! - `backoff`: the wait before a submit whose answer was lost is sent again.

use kbf_front::action_tree::WriteStage;

use crate::executor::config::InputTree;
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
    /// The tree list's one write.
    TreeList,
    /// The tree list's L2 sidecar.
    TreeSidecar,
    /// The operation's row in the state store.
    Row,
    /// One `SubmitJob` call.
    Rpc,
    /// The wait before a submit is sent again.
    Backoff,
}

impl Step {
    /// Every step, in the order the metric lists them.
    pub(crate) const ALL: [Self; 15] = [
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
        Self::TreeList,
        Self::TreeSidecar,
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
            Self::TreeList => "tree_list",
            Self::TreeSidecar => "tree_sidecar",
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
            WriteStage::List => Self::TreeList,
            WriteStage::Sidecar => Self::TreeSidecar,
        }
    }
}

/// The forms a step is labelled with, in the order the family lists them.
const FORMS: [InputTree; 2] = [InputTree::Manifests, InputTree::Provider];

/// One series of `kbf_executor_step_seconds`: a step, and the form of the run's action tree it
/// was taken for; `facts` is taken before there is one, and has none whatever `form` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StepOf {
    pub(crate) step: Step,
    pub(crate) form: InputTree,
}

impl StepOf {
    /// Every series: `facts` once, then every other step in each form.
    const ALL: [Self; 1 + FORMS.len() * (Step::ALL.len() - 1)] = {
        let facts = Self {
            step: Step::Facts,
            form: InputTree::Manifests,
        };
        let mut all = [facts; 1 + FORMS.len() * (Step::ALL.len() - 1)];
        let mut at = 1;
        while at < Step::ALL.len() {
            let mut form = 0;
            while form < FORMS.len() {
                all[1 + (at - 1) * FORMS.len() + form] = Self {
                    step: Step::ALL[at],
                    form: FORMS[form],
                };
                form += 1;
            }
            at += 1;
        }
        all
    };

    /// The `form` label.
    const fn form_label(self) -> &'static str {
        match (self.step, self.form) {
            (Step::Facts, _) => "none",
            (_, InputTree::Manifests) => "manifests",
            (_, InputTree::Provider) => "provider",
        }
    }
}

impl Series for StepOf {
    const NAME: &'static str = "kbf_executor_step_seconds";
    const HELP: &'static str = "How long the steps of an executor run's preparation and submit \
                                took, by step and by the form of the run's action tree, each \
                                timed around its one wait";
    const ALL: &'static [Self] = &Self::ALL;

    fn labels(self) -> String {
        format!(
            "step=\"{}\",form=\"{}\"",
            self.step.label(),
            self.form_label()
        )
    }

    fn slot(self) -> usize {
        match self.step {
            Step::Facts => 0,
            step => {
                let form = usize::from(self.form == InputTree::Provider);
                1 + (step as usize - 1) * FORMS.len() + form
            }
        }
    }
}

/// `kbf_executor_step_seconds`: a histogram of the steps, one series per step and form.
pub(crate) type StepHistograms = Histograms<StepOf>;

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
