//! The frontend's start publishing the kbf base tree (docs/design/actions-as-jobs.md, 3.3):
//! under `--executor js-agent`, before the executor is spawned and so before any job names a
//! tree, [`Start::publish`] reads the release's bundle for the executor's `--executor-arch` from
//! `--executor-base-tree` ([`kbf_front::base_publish`]), writes the objects the pack store lacks
//! to `cas/l/`, and, with a state store, records the tree as `job-tree/<digest>`
//! ([`crate::job_tree`]).
//!
//! The commit is the one this binary was built from ([`crate::BUILD_COMMIT_FULL`]): the bundle
//! of another release is not taken, and a build that does not know its commit publishes nothing.
//! Under `--executor kbf-daemon` there is no [`Start`]: nothing is read or written, whatever the
//! flag says.
//!
//! A failure at any step fails the start ([`ServeError::BaseTree`]), naming the flag and the
//! cause. The [`Published`] tree is held by `FrontendHandle::serve` until it ends; the executor
//! will take its root manifest and facts from it when its jobs name trees.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use kbf_front::Packs;
use kbf_front::base_publish::{Published, publish};
use kbf_state::Store;

use super::JsAgentFlags;
use crate::job_tree::{self, Recorded};
use crate::serve::ServeError;

/// What the publish needs, taken from the flags when the frontend is started.
pub(super) struct Start {
    dir: PathBuf,
    platform: String,
    packs: Packs,
    state: Option<Arc<dyn Store>>,
}

impl Start {
    /// The publish `js` describes, into `packs`, with `state` for the record.
    pub(super) fn new(js: &JsAgentFlags, packs: Packs, state: Option<Arc<dyn Store>>) -> Self {
        Self {
            dir: js.base_tree.clone(),
            platform: format!("linux-{}", js.config.arch),
            packs,
            state,
        }
    }

    /// [`Start::publish_as`] for the release this binary is.
    async fn publish(self) -> Result<Published, ServeError> {
        self.publish_as(crate::BUILD_COMMIT_FULL, SystemTime::now)
            .await
    }

    /// Publishes the tree of the release `commit` and records it, at the time `now` gives.
    async fn publish_as(
        self,
        commit: &str,
        now: fn() -> SystemTime,
    ) -> Result<Published, ServeError> {
        let refused = |why: &dyn std::fmt::Display| ServeError::BaseTree(why.to_string());
        let packs = Arc::new(self.packs);
        let published = publish(&self.dir, commit, &self.platform, packs)
            .await
            .map_err(|why| refused(&why))?;
        let root = published.root.hash_hex();
        tracing::info!(
            root,
            written = published.written,
            held = published.held,
            entries = published.facts.entries,
            depth = published.facts.depth,
            platform = self.platform,
            "published the kbf base tree"
        );
        if let Some(state) = self.state {
            let recorded = job_tree::record_blocking(state, root.clone(), commit.to_owned(), now)
                .await
                .map_err(|why| refused(&why))?;
            if recorded == Recorded::Again {
                tracing::info!(root, "the kbf base tree was recorded by an earlier publish");
            }
        }
        Ok(published)
    }
}

/// Publishes the base tree `start` describes, if the frontend has one to publish (it runs
/// `--executor js-agent`): the tree, which the caller holds, or `None`.
///
/// # Errors
/// [`ServeError::BaseTree`]: the bundle cannot be used, the pack store refused a write, or the
/// state store failed the record.
pub(super) async fn publish_all(start: Option<Start>) -> Result<Option<Published>, ServeError> {
    match start {
        Some(start) => start.publish().await.map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "base_tree_tests.rs"]
pub(super) mod tests;
