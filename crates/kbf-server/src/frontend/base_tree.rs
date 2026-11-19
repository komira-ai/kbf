//! The frontend's start publishing the kbf base tree (docs/design/actions-as-jobs.md, 3.3):
//! under `--executor js-agent`, before the executor is spawned and so before any job names a
//! tree, [`Start::publish`] reads the release's bundle for the executor's `--executor-arch` from the
//! cache `--executor-base-tree` names, which fetches it by its release's digest when it lacks it
//! ([`super::base_fetch`]), then publishes it ([`kbf_front::base_publish`]): it writes the objects the pack store lacks
//! to `cas/l/`, and, with a state store, records the tree as `job-tree/<digest>`
//! ([`crate::job_tree`]).
//!
//! The commit is the one this binary was built from ([`crate::BUILD_COMMIT_FULL`]): the bundle
//! of another release is not taken, and a build that does not know its commit publishes nothing.
//! Without `--executor js-agent` there is no [`Start`]: nothing is read or written, whatever the
//! flag says.
//!
//! A failure at any step fails the start ([`ServeError::BaseTree`]), naming the flag and the
//! cause. The [`Published`] tree is held by `FrontendHandle::serve` until it ends, and its base
//! ([`BaseTree`]) is set in the slot the executor writes its action trees over
//! (`--task-out-mode put-dir`) before the executor is started.
//!
//! **Published again.** The store is a general-purpose one, and its objects can vanish while
//! the frontend holds the base as published. Once published, the same publish is set in the
//! executor's slot ([`JsAgentFlags::republish`]): when a job did not start for an object the
//! store lost that is none of its action tree's own, the executor runs it again, which writes
//! every object of the bundle the store lacks and costs one length check for each it has.

use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use futures::FutureExt as _;
use kbf_front::Packs;
use kbf_front::action_tree::BaseTree;
use kbf_front::base_publish::{Published, publish};
use kbf_state::Store;

use super::JsAgentFlags;
use super::base_fetch::Cache;
use crate::executor::{InputTree, Republish};
use crate::job_tree::{self, Recorded};
use crate::serve::ServeError;

/// What the publish needs, taken from the flags when the frontend is started.
pub(super) struct Start {
    cache: Cache,
    platform: String,
    packs: Packs,
    state: Option<Arc<dyn Store>>,
    /// Where the published base is set for the executor.
    slot: Arc<OnceLock<BaseTree>>,
    /// Whether the base is listed: the executor writes tree lists
    /// (`--executor-input-tree provider`).
    list: bool,
    /// Where the publish again is set for the executor.
    republish: Arc<OnceLock<Republish>>,
}

impl Start {
    /// The publish `js` describes, into `packs`, with `state` for the record, setting the
    /// published base in `js`'s slot for the executor ([`JsAgentFlags::published`]).
    pub(super) fn new(js: &JsAgentFlags, packs: Packs, state: Option<Arc<dyn Store>>) -> Self {
        Self {
            cache: Cache {
                dir: js.base_tree.clone(),
                url: js.base_tree_url.clone(),
                keep: js.base_tree_keep,
                timeout: js.base_tree_timeout,
                instance: js.config.instance.clone(),
                held: Arc::default(),
            },
            platform: format!("linux-{}", js.config.arch),
            packs,
            state,
            slot: Arc::clone(&js.published),
            list: js.config.input_tree == InputTree::Provider,
            republish: Arc::clone(&js.republish),
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
        let (dir, origin) = self
            .cache
            .ensure(commit, &self.platform)
            .await
            .map_err(|why| refused(&why))?;
        tracing::info!(dir = %dir.display(), ?origin, "the kbf base tree bundle is ready");
        let store: Arc<Packs> = Arc::clone(&packs);
        let published = publish(&dir, commit, &self.platform, store, self.list)
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
        // A frontend publishes once; a slot set already holds the same release's base.
        let _ = self.slot.set(published.base.clone());
        let again = again(self.cache, commit, self.platform, packs, self.list);
        let _ = self.republish.set(again);
        Ok(published)
    }
}

/// The publish of the bundle of `commit` for `platform` into `packs` again, as [`Start`] made it
/// the first time: the bundle from the cache, and each of its objects the store lacks written.
/// Nothing is recorded: the tree is the one recorded the first time.
fn again(cache: Cache, commit: &str, platform: String, packs: Arc<Packs>, list: bool) -> Republish {
    let commit = commit.to_owned();
    Republish::new(move || {
        let (cache, commit, platform, packs) = (
            cache.clone(),
            commit.clone(),
            platform.clone(),
            Arc::clone(&packs),
        );
        async move {
            let (dir, _) = (cache.ensure(&commit, &platform).await).map_err(|e| e.to_string())?;
            let store: Arc<Packs> = packs;
            let published = publish(&dir, &commit, &platform, store, list).await;
            let published = published.map_err(|e| e.to_string())?;
            tracing::warn!(
                root = published.root.hash_hex(),
                written = published.written,
                held = published.held,
                "published the kbf base tree again"
            );
            Ok(())
        }
        .boxed()
    })
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
