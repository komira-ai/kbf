//! What an Execute's input check leaves, in the [`Backend`] it shares with the executor's
//! preparation, for the run it starts (`input_walk` has the rules): the Directories it reads,
//! kept with their names in the preparation's cache, and when it found every input of the
//! Action held, which the preparation's closure walk takes instead of asking the store again.
//! A plane whose walk is not over a store (the cache in this process) keeps neither, and its
//! preparation walks and checks for itself.

use std::sync::Arc;

use kbf_types::Digest;
use tokio::time::Instant;

use super::Backend;
use crate::prepare_run::DirectoryCache;

impl Backend {
    /// From now on, every Directory the input check of an Execute over this backend reads is
    /// also kept in `cache`, decoded with its names, and so is every Directory a client
    /// uploads through the backend's packs. Over a map still on its one bootstrap node, that
    /// node's walk keeps nothing here unless the frontend has packs, which the two walks
    /// share.
    pub(crate) fn keep_directories(&self, cache: Arc<DirectoryCache>) {
        let trees = self.0.trees().into_iter();
        trees.for_each(|trees| trees.keep_into(Arc::clone(&cache)));
    }

    /// An Execute found the Command and every blob of the input tree of `action` held, by
    /// reads and FindMissing calls that all began at `at` or later.
    pub(crate) fn vouch(&self, action: Digest, at: Instant) {
        let trees = self.0.trees().into_iter();
        trees.for_each(|trees| trees.vouch(action, at));
    }

    /// Whether an Execute over this backend found the inputs of `action` held recently enough
    /// that its run's preparation need not ask the store again; the finding is used up.
    pub(crate) fn vouched(&self, action: &Digest) -> bool {
        let now = Instant::now();
        self.0
            .trees()
            .is_some_and(|trees| trees.vouched(action, now))
    }
}
