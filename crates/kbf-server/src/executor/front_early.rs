//! Action trees written ahead of their runs ([`Ingest::start_tree`](crate::executor::Ingest::start_tree)):
//! an Execute that missed the action cache starts its tree's write once its input walk has read
//! every Directory, while the walk still asks the store whether the files are held.
//!
//! - **Started** for an action and the form its run will be minted in (a Mac's tree list, else
//!   the form [`Front::form_of`] gives), unless the base tree is not published, the action's
//!   facts refuse it, its next tree is to be written checked, a tree of it is remembered, or a
//!   write of it in that form is already under way. The write collects the closure from the
//!   Directories the walk kept, with no held check and no put directory
//!   ([`RunPrepare::collect_unchecked`]), then writes the tree as a mint would: blobs, then the
//!   tree's last object (its root manifest, or its tree list).
//! - **Taken** by the next [`Ingest::mint`](crate::executor::Ingest::mint) of the action in that
//!   form, once: the mint mints the run's put directory only, and parks the write as its own, so
//!   the run's job is still submitted only once the tree's last object is stored
//!   ([`Ingest::stored`](crate::executor::Ingest::stored)). A write that ended before it named
//!   the tree is not taken: the mint collects and writes the tree itself, and answers its own
//!   refusals as before.
//! - **Remembered** for later runs only once every write succeeded and the Execute found every
//!   input held (its [`HeldFinding`]), and only if nothing dropped the action's trees meanwhile
//!   (a lost object, a refused tree list) and its next tree is not to be written checked.
//! - **The Execute's vouch** ([`kbf_front::prepare_run`]'s held check takes it) is not taken by
//!   a run that takes the write: the Execute's finding stands in for it. It is still believed
//!   for its time to live, as it was before, by a later preparation of the action that collects
//!   its own closure (another attempt, or a mint after a write that named no tree). A write for an Execute that ended MISSING may leave
//!   objects in the store, which are content-addressed and written create-only, and nothing
//!   else: no operation, no row, no memo.
//! - **Kept** until it ends, whether a mint took it or not, so a second Execute of the action
//!   starts no second write, and a drop of the action's trees finds it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::SystemTime;

use kbf_front::action_tree::{BaseTree, ListingWrite, MEMO_TTL, TreeWriter};
use kbf_front::{DecodedAction, HeldFinding};
use kbf_proto::reapi::Digest;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{Front, RunPrepare, Trees, cas_digest, lock_checked, lock_trees, write_named};
use crate::executor::front_cache::Bounded;
use crate::executor::config::InputTree;
use crate::executor::mac::System;
use crate::executor::metrics::Metrics;
use crate::executor::steps::Step;
use crate::executor::traits::MintError;

/// The writes started ahead of their runs, by action and form.
pub(super) type Earlies = Arc<Mutex<BTreeMap<(kbf_types::Digest, InputTree), Early>>>;

/// A write started ahead of its run, kept until it ends, taken by a mint or not.
pub(super) struct Early {
    /// Tells the entry from a later one of the same action and form.
    id: u64,
    /// What a mint takes, once: the tree's name, sent as soon as the writer has it (dropped by a
    /// write that ended first), and the task that writes the tree, which ends with the writes'
    /// answer.
    take: Option<Taken>,
    /// Cleared when the action's trees are dropped while it is written: it is not remembered.
    trusted: Arc<AtomicBool>,
}

/// What a mint takes of a write started ahead of its run.
type Taken = (oneshot::Receiver<String>, JoinHandle<Result<(), MintError>>);

/// What the task writing a tree ahead of its run uses.
struct Writing {
    prepare: Arc<dyn RunPrepare>,
    writer: Arc<TreeWriter>,
    base: Arc<OnceLock<BaseTree>>,
    trees: Arc<Mutex<Trees>>,
    checked: Arc<Mutex<Bounded<kbf_types::Digest, ()>>>,
    earlies: Earlies,
    metrics: Arc<Metrics>,
    how: ListingWrite,
}

/// The entries behind `earlies`' lock.
fn lock_earlies(
    earlies: &Mutex<BTreeMap<(kbf_types::Digest, InputTree), Early>>,
) -> MutexGuard<'_, BTreeMap<(kbf_types::Digest, InputTree), Early>> {
    earlies.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Front {
    /// [`Ingest::start_tree`](crate::executor::Ingest::start_tree): the module docs.
    pub(super) fn start_early(
        &self,
        action: &Digest,
        decoded: Arc<DecodedAction>,
        configured: InputTree,
        finding: HeldFinding,
    ) {
        let Ok(action) = cas_digest(action) else {
            return;
        };
        if self.base.get().is_none() {
            return;
        }
        // An action its facts refuse has no run to take the tree.
        let Ok(facts) = self.facts_of(&decoded.action, &decoded.command) else {
            return;
        };
        let form = match facts.system {
            System::Macos { .. } => InputTree::Provider,
            System::Linux => self.form_of(&action, configured),
        };
        let now = (self.now)();
        let checked = self.checked().get(&action).is_some();
        if checked || self.remembered_tree(&action, form, now).is_some() {
            return;
        }
        let mut earlies = lock_earlies(&self.earlies);
        if earlies.contains_key(&(action, form)) {
            return;
        }
        let id = self.early_ids.fetch_add(1, Ordering::Relaxed);
        let trusted = Arc::new(AtomicBool::new(true));
        let (named, name) = oneshot::channel();
        let writing = Writing {
            prepare: Arc::clone(&self.prepare),
            writer: Arc::clone(&self.writer),
            base: Arc::clone(&self.base),
            trees: Arc::clone(&self.trees),
            checked: Arc::clone(&self.checked),
            earlies: Arc::clone(&self.earlies),
            metrics: Arc::clone(&self.metrics),
            how: self.listing_write(&action, form, false),
        };
        let started = Started {
            key: (action, form),
            id,
            trusted: Arc::clone(&trusted),
            now,
        };
        let writes = tokio::spawn(write_early(writing, started, decoded, finding, named));
        let early = Early {
            id,
            take: Some((name, writes)),
            trusted,
        };
        earlies.insert((action, form), early);
    }

    /// The write started ahead of the run for `action` in `form`, taken: the tree's name and the
    /// task that writes it. `None` when there is none, when another mint took it, or when it
    /// ended before it named the tree.
    pub(super) async fn take_early(
        &self,
        action: &kbf_types::Digest,
        form: InputTree,
    ) -> Option<(String, JoinHandle<Result<(), MintError>>)> {
        let taken = lock_earlies(&self.earlies)
            .get_mut(&(*action, form))
            .and_then(|early| early.take.take());
        let (name, writes) = taken?;
        let tree = name.await.ok()?;
        Some((tree, writes))
    }

    /// Drops the writes of `action` started ahead of their runs: none is taken, and none is
    /// remembered once it ends. Called before the action's trees are dropped, so that a write
    /// remembered before it is cleared is dropped with them.
    pub(super) fn forget_early(&self, action: &kbf_types::Digest) {
        let mut earlies = lock_earlies(&self.earlies);
        for form in [InputTree::Manifests, InputTree::Provider] {
            if let Some(early) = earlies.remove(&(*action, form)) {
                early.trusted.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// Which write a task makes, and when it was started.
struct Started {
    key: (kbf_types::Digest, InputTree),
    id: u64,
    trusted: Arc<AtomicBool>,
    now: SystemTime,
}

/// Collects the closure of the action of `started` with no held check and writes its tree,
/// sending the tree's name to `named` as soon as the writer has it; then remembers the tree if
/// every write succeeded, `finding` says the Execute found every input held and the write is
/// still trusted; then forgets the entry, and answers with the writes' answer.
async fn write_early(
    writing: Writing,
    started: Started,
    decoded: Arc<DecodedAction>,
    finding: HeldFinding,
    named: oneshot::Sender<String>,
) -> Result<(), MintError> {
    let (action, form) = started.key;
    let written = async {
        let collected = writing.prepare.collect_unchecked(&action, &decoded.action);
        let (kept, took) = collected.await.map_err(MintError::from)?;
        for (step, took) in [(Step::Action, took.action), (Step::Walk, took.walk)] {
            writing.metrics.step_took(step, form, took);
        }
        let timed = |timed| writing.metrics.tree_write(form, timed);
        let (writer, base) = (&writing.writer, &writing.base);
        write_named(writer, base, &kept, (form, writing.how), named, timed).await
    };
    let written = written.await;
    let held = finding.wait().await;
    if let Ok(tree) = &written
        && held
    {
        // Read under the trees' lock: a drop of the action's trees clears it first (and a lost
        // object marks the action checked first), then takes the lock to drop them, so a tree
        // put here is either not put or dropped with them.
        let mut trees = lock_trees(&writing.trees);
        let checked = lock_checked(&writing.checked).get(&action).is_some();
        if started.trusted.load(Ordering::SeqCst) && !checked {
            let expires = started.now.checked_add(MEMO_TTL).unwrap_or(started.now);
            trees.put((action, form), (tree.clone(), expires));
        }
    }
    // Remembered before it is forgotten: a mint that finds no entry finds the tree.
    let mut earlies = lock_earlies(&writing.earlies);
    if earlies
        .get(&started.key)
        .is_some_and(|early| early.id == started.id)
    {
        earlies.remove(&started.key);
    }
    drop(earlies);
    written.map(drop)
}
