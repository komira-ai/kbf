//! The frontend's miss path for a tree list a host refused as not mountable through tierstore
//! ([`Ingest::list_refused`](crate::executor::Ingest::list_refused); tierstore's `docs/design/fetch-cache-serve.md`, 3.5).
//!
//! A host whose reader finds a version 2 tree list with no readable L2 sidecar refuses the job's
//! start with `ListVersionUnsupported`. The tree is not the action's fault, so the job is not
//! failed for it: per action, the first refusal takes the action's remembered trees away so its
//! next tree is written again with its sidecar republished ([`MissPath::Republish`]), and the
//! second makes the action's jobs read the tree as manifests, which tierstore's file system
//! serves with no mount provider ([`MissPath::Manifests`]). What the frontend remembers of an
//! action is one [`MissPath`], in a memo bounded and ordered as the action trees' are, so an
//! action it has forgotten starts again at the first step.

use std::sync::{Mutex, MutexGuard, PoisonError};

use kbf_front::action_tree::ListingWrite;

use super::Front;
use crate::executor::config::InputTree;
use crate::executor::front_cache::Bounded;
use crate::executor::traits::MissPath;

/// The step of the miss path each action is on.
pub(super) type Misses = Mutex<Bounded<kbf_types::Digest, MissPath>>;

impl Front {
    fn misses(&self) -> MutexGuard<'_, Bounded<kbf_types::Digest, MissPath>> {
        self.misses.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the next step of the miss path for `action` and returns it: the first refusal is
    /// [`MissPath::Republish`], every one after [`MissPath::Manifests`]. The action's remembered
    /// trees, in both forms, are dropped, so its next mint writes the tree.
    pub(super) fn miss_step(&self, action: &kbf_types::Digest) -> MissPath {
        let step = match self.misses().get(action) {
            None => MissPath::Republish,
            Some(_) => MissPath::Manifests,
        };
        self.forget_early(action);
        for form in [InputTree::Manifests, InputTree::Provider] {
            self.trees().remove(&(*action, form));
        }
        self.misses().put(*action, step);
        self.metrics.list_refused(step);
        tracing::warn!(%action, step = step.label(), "a host refused an action's tree list as not mountable through tierstore: its next tree is written by the miss path");
        step
    }

    /// The form the next tree of `action` is written and mounted in: manifests once the miss path
    /// has taken the action there, else `configured`.
    pub(super) fn form_of(&self, action: &kbf_types::Digest, configured: InputTree) -> InputTree {
        match self.misses().get(action) {
            Some(MissPath::Manifests) => InputTree::Manifests,
            Some(MissPath::Republish) | None => configured,
        }
    }

    /// How the tree list of `action` in `form` is written, `checked` when a job of the action
    /// found an object of its tree lost: with its L2 sidecar republished (and so checked
    /// against the store) while the action is on the first step of the miss path.
    pub(super) fn listing_write(
        &self,
        action: &kbf_types::Digest,
        form: InputTree,
        checked: bool,
    ) -> ListingWrite {
        let republish =
            form == InputTree::Provider && self.misses().get(action) == Some(MissPath::Republish);
        ListingWrite {
            checked: checked || republish,
            republish,
        }
    }
}
