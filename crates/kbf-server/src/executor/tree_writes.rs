//! Where the time of an action tree's writes goes, apart from the store's request, and how many
//! manifests a tree writes: `kbf_executor_tree_write_seconds{stage=...,part=...}` and
//! `kbf_executor_tree_manifests_written`, beside the stages of
//! [`crate::executor::steps`].
//!
//! - **`kbf_executor_tree_write_seconds`** has one observation per write a tree made, once it
//!   ended, stored or failed, of each of its parts ([`WriteWaits`]): `manifest_slot` (the wait
//!   for one of the writer's manifest slots, shared by every tree), `store_slot` (the waits for
//!   one of the pack store's slots, shared with every pack upload, added up over the attempts),
//!   `store_503` (the waits between attempts for a store that answered 503) and `request` (the
//!   rest: the attempts at the store). `stage` is the stage the write was made in, as the step
//!   family names it: `tree_manifests` (one observation per manifest, `.kbf` among them),
//!   `tree_root` (the root) and `tree_list` (a tree list's one write, which takes no manifest
//!   slot, so it has no `manifest_slot`). The stage names the form, so there is no `form`
//!   label. A manifest another tree had in flight is observed once, for the tree that wrote it.
//! - **`kbf_executor_tree_manifests_written`** has one observation per tree written as
//!   manifests whose manifests stage stored them all: how many manifests that stage wrote by its
//!   own requests, `.kbf` among them and the root not; not those the memo had, nor those another
//!   tree had in flight. Its buckets count manifests, from none to 5,000.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_front::action_tree::{WriteStage, WriteWaits};

use crate::executor::histogram::{Histograms, Series};

/// A stage whose writes are timed one by one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The input root's manifests and `.kbf`.
    Manifests,
    /// The root manifest.
    Root,
    /// The tree list's one write.
    List,
}

impl Stage {
    /// The stage a write of `stage` was made in, if its writes are timed: the blobs' and the
    /// sidecar's are not.
    pub(crate) const fn of(stage: WriteStage) -> Option<Self> {
        match stage {
            WriteStage::Manifests => Some(Self::Manifests),
            WriteStage::Root => Some(Self::Root),
            WriteStage::List => Some(Self::List),
            WriteStage::Blobs | WriteStage::Sidecar => None,
        }
    }

    /// The `stage` label: the step it is timed as.
    const fn label(self) -> &'static str {
        match self {
            Self::Manifests => "tree_manifests",
            Self::Root => "tree_root",
            Self::List => "tree_list",
        }
    }
}

/// A part of one write's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    /// The wait for a manifest slot.
    ManifestSlot,
    /// The waits for a slot of the pack store.
    StoreSlot,
    /// The waits between attempts for a store that answered 503.
    Store503,
    /// The attempts at the store.
    Request,
}

impl Part {
    /// The `part` label.
    const fn label(self) -> &'static str {
        match self {
            Self::ManifestSlot => "manifest_slot",
            Self::StoreSlot => "store_slot",
            Self::Store503 => "store_503",
            Self::Request => "request",
        }
    }
}

/// One series of `kbf_executor_tree_write_seconds`: made only by [`observe`], which never makes
/// a tree list's `manifest_slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WritePart {
    stage: Stage,
    part: Part,
}

impl WritePart {
    const fn of(stage: Stage, part: Part) -> Self {
        Self { stage, part }
    }

    /// Every series: each part of a manifest's write, then the root's, then a tree list's,
    /// which has no manifest slot.
    const ALL: [Self; 11] = [
        Self::of(Stage::Manifests, Part::ManifestSlot),
        Self::of(Stage::Manifests, Part::StoreSlot),
        Self::of(Stage::Manifests, Part::Store503),
        Self::of(Stage::Manifests, Part::Request),
        Self::of(Stage::Root, Part::ManifestSlot),
        Self::of(Stage::Root, Part::StoreSlot),
        Self::of(Stage::Root, Part::Store503),
        Self::of(Stage::Root, Part::Request),
        Self::of(Stage::List, Part::StoreSlot),
        Self::of(Stage::List, Part::Store503),
        Self::of(Stage::List, Part::Request),
    ];
}

impl Series for WritePart {
    const NAME: &'static str = "kbf_executor_tree_write_seconds";
    const HELP: &'static str = "Where the time of each write an action tree made went, by the \
                                stage it was made in and by part: the wait for a manifest slot, \
                                the waits for a slot of the pack store, the waits between \
                                attempts for a store that answered 503, and the attempts";
    const ALL: &'static [Self] = &Self::ALL;

    fn labels(self) -> String {
        format!(
            "stage=\"{}\",part=\"{}\"",
            self.stage.label(),
            self.part.label()
        )
    }

    fn slot(self) -> usize {
        let part = self.part as usize;
        match self.stage {
            Stage::Manifests => part,
            Stage::Root => 4 + part,
            // A tree list's write has no `manifest_slot`, the first part; it is never observed.
            Stage::List => 8 + part.saturating_sub(1),
        }
    }
}

/// `kbf_executor_tree_write_seconds`.
pub(crate) type WriteHistograms = Histograms<WritePart>;

/// Counts one write of `stage` into `histograms`, each of its parts.
pub(crate) fn observe(histograms: &WriteHistograms, stage: Stage, waits: WriteWaits) {
    let parts = [
        (Part::ManifestSlot, waits.manifest_slot),
        (Part::StoreSlot, waits.store_slot),
        (Part::Store503, waits.store_503),
        (Part::Request, waits.request),
    ];
    for (part, took) in parts {
        if stage == Stage::List && part == Part::ManifestSlot {
            continue;
        }
        histograms.observe(WritePart { stage, part }, took);
    }
}

/// The upper bounds of the buckets of `kbf_executor_tree_manifests_written`, in manifests.
const MANIFEST_BOUNDS: [u64; 13] = [0, 1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000];

/// `kbf_executor_tree_manifests_written`: a histogram of the manifests each tree wrote.
#[derive(Debug, Default)]
pub(crate) struct ManifestsWritten {
    /// The trees that fell in each bucket (the last is over every bound).
    buckets: [AtomicU64; MANIFEST_BOUNDS.len() + 1],
    sum: AtomicU64,
}

impl ManifestsWritten {
    /// Counts one tree that wrote `written` manifests.
    pub(crate) fn observe(&self, written: u64) {
        let bucket = MANIFEST_BOUNDS
            .iter()
            .position(|bound| written <= *bound)
            .unwrap_or(MANIFEST_BOUNDS.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(written, Ordering::Relaxed);
    }

    /// Appends the family to `out`, in the Prometheus text format.
    pub(crate) fn render(&self, out: &mut String) {
        let name = "kbf_executor_tree_manifests_written";
        let _ = writeln!(
            out,
            "# HELP {name} How many manifests the manifests stage of each action tree wrote by \
             its own requests, .kbf among them and the root not\n# TYPE {name} histogram"
        );
        let mut seen = 0;
        let bounds = MANIFEST_BOUNDS.iter().map(u64::to_string);
        for (le, count) in bounds.chain(["+Inf".to_owned()]).zip(&self.buckets) {
            seen += count.load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {seen}");
        }
        let sum = self.sum.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_sum {sum}\n{name}_count {seen}");
    }
}

#[cfg(test)]
#[path = "tree_writes_tests.rs"]
mod tests;
