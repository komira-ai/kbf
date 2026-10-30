//! Predictions from the estimator (`--placement-predictions`): what a new operation is
//! predicted to cost, and in shadow how the finished run fell against it.
//!
//! One function makes the prediction for every arm of the farm ([`Learned::predict`]): the
//! estimator's learned row for the operation's key, converted as the scheduler converts an
//! estimate ([`Prediction::from_estimate`]) with the cores clamped to what the largest
//! node that could run it has registered. It is made from the learned row alone: every
//! fallback of the estimator (no row, a stale or cold one, a size that would take a whole
//! host) and every operation the farm does not ask it about gives none, counted by why, and
//! a prediction is never the scheduler's cold one.
//!
//! The comparison with a run is made in `shadow`, once, for the first run of the operation
//! that the estimator would take as a sample (it succeeded and ran a second or more), on the
//! three figures the prediction carries. The fit band is the same for each figure: the run
//! is **under** when it used more than predicted, **over** when it used less than half of
//! what was predicted, and **fit** in between (`predicted / 2 <= used <= predicted`). The
//! figures are the cores the run used on average (CPU time over wall time, in millicores),
//! its peak memory in bytes, and its life in whole minutes (the minute it reached, counted
//! from 1, at most the estimator's horizon). A run that measured no peak or no CPU time has
//! no figure on that axis and is not counted on it.

use std::collections::BTreeMap;

use kbf_estimator::{
    Bounds, Estimate, Fallback, Hint, Key, LIFE_HORIZON, MIN_WALL_US, Outcome as RunOutcome, Run,
};
use kbf_front::metrics::{PREDICTION_UNSET, PredictionTotals};
use kbf_sched::Prediction;
use kbf_types::{LeaseId, OperationId};

use super::{Learned, STARTED_CAP, Skip};
use crate::PlacementPredictions;

/// Microseconds in a minute.
const MINUTE_US: u64 = 60_000_000;

/// Why a new operation has no prediction. The order is that of
/// [`PREDICTION_UNSET`], which spells each as its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unset {
    /// The estimator has no row for the key.
    Missing,
    /// The key's row was last seen too long ago.
    Stale,
    /// The key's row holds too few samples.
    Cold,
    /// The key's stored row did not decode.
    Corrupt,
    /// The size would reach the memory of the largest matching node.
    OverCap,
    /// Not an action, or it asked for a GPU.
    Kind,
    /// The request's `needs` do not pin the OS.
    NoOs,
    /// The operation's subject is unknown.
    NoSubject,
}

impl Unset {
    /// The `reason` label of `kbf_prediction_unset_total` for this reason.
    #[must_use]
    pub const fn label(self) -> &'static str {
        PREDICTION_UNSET[self as usize]
    }

    /// The reason for the error [`crate::learned::key_of`] gave: it refuses only an OS that
    /// is not pinned and a subject that is not known.
    #[must_use]
    pub fn of_key_error(why: Skip) -> Self {
        if why == Skip::NoSubject {
            Self::NoSubject
        } else {
            Self::NoOs
        }
    }
}

impl From<Fallback> for Unset {
    fn from(fallback: Fallback) -> Self {
        match fallback {
            Fallback::Missing => Self::Missing,
            Fallback::Stale => Self::Stale,
            Fallback::Cold => Self::Cold,
            Fallback::Corrupt => Self::Corrupt,
            Fallback::OverCap => Self::OverCap,
        }
    }
}

/// What [`Learned::forecast`] says of a new operation: the prediction, or why there is none.
pub type Forecast = Result<Prediction, Unset>;

/// The figure of a run a prediction is compared with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// The cores the run used on average, in millicores.
    Cpu,
    /// The peak memory of the run, in bytes.
    Mem,
    /// The life of the run, in whole minutes.
    Life,
}

/// How a run fell against its prediction on one axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The run used more than predicted.
    Under,
    /// The run used at most the prediction and at least half of it.
    Fit,
    /// The run used less than half of the prediction.
    Over,
}

impl Fit {
    /// Where `used` falls against `predicted`.
    #[must_use]
    pub const fn of(used: u64, predicted: u64) -> Self {
        if used > predicted {
            Self::Under
        } else if used.saturating_mul(2) < predicted {
            Self::Over
        } else {
            Self::Fit
        }
    }
}

/// The predictions of a farm and what became of them.
#[derive(Debug, Default)]
pub(super) struct Predictions {
    pub(super) mode: PlacementPredictions,
    made: u64,
    unset: [u64; 8],
    vs_run: [[u64; 3]; 3],
    /// In shadow: the prediction of each operation that has not yet had a run compared.
    pending: BTreeMap<OperationId, Prediction>,
    /// The prediction of the operation whose report is staged for each lease.
    staged: BTreeMap<LeaseId, (OperationId, Prediction)>,
}

/// `estimate` as the scheduler is given it: the cores never above what `bounds` says the
/// largest node has (a client's hint may be above it), the memory below the host's by the
/// estimator's own rule.
fn prediction_of(estimate: &Estimate, bounds: &Bounds) -> Prediction {
    let prediction = Prediction::from_estimate(estimate);
    Prediction {
        cpu_millis: prediction.cpu_millis.min(u64::from(bounds.node_cpu_millis)),
        ..prediction
    }
}

impl Learned {
    /// This state, with predictions as `mode` says.
    #[must_use]
    pub fn with_predictions(mut self, mode: PlacementPredictions) -> Self {
        self.predictions.mode = mode;
        self
    }

    /// Sets what predictions do (`--placement-predictions`).
    pub fn set_predictions(&mut self, mode: PlacementPredictions) {
        self.predictions.mode = mode;
    }

    /// What predictions do.
    #[must_use]
    pub const fn predictions(&self) -> PlacementPredictions {
        self.predictions.mode
    }

    /// What the estimator would predict for an operation of `key` whose client named `hint`,
    /// asked about on nodes as `bounds` says: the learned row, with the cores clamped to the
    /// largest node's. Counts nothing and changes nothing, so the farm may ask before it
    /// knows the operation is a new one ([`Learned::note_forecast`] counts it after).
    ///
    /// # Errors
    /// The reason the estimator has no usable row.
    pub fn forecast(&self, key: &Key, hint: &Hint, bounds: &Bounds) -> Forecast {
        self.summaries
            .estimate(key, self.day(), bounds, hint)
            .map(|estimate| prediction_of(&estimate, bounds))
            .map_err(Unset::from)
    }

    /// Counts a [`Learned::forecast`] of the new operation `operation` as a prediction made,
    /// or unset by its reason, and returns what to give the scheduler for it
    /// ([`Learned::to_set`]). Nothing while predictions are off.
    pub fn note_forecast(
        &mut self,
        operation: OperationId,
        forecast: Forecast,
    ) -> Option<Prediction> {
        if !self.predictions.mode.predicts() {
            return None;
        }
        self.calls += 1;
        match forecast {
            Ok(prediction) => {
                self.predictions.made += 1;
                self.to_set(operation, Some(prediction))
            }
            Err(why) => {
                self.predictions.unset[why as usize] += 1;
                None
            }
        }
    }

    /// The prediction for an operation of `key` whose client named `hint`, asked about on
    /// nodes as `bounds` says: the estimator's learned row, with the cores clamped to the
    /// largest node's. `None` when the estimator has no usable row (counted by its reason),
    /// and always while predictions are off. Never the scheduler's cold prediction.
    pub fn predict(&mut self, key: &Key, hint: &Hint, bounds: &Bounds) -> Option<Prediction> {
        if !self.predictions.mode.predicts() {
            return None;
        }
        self.calls += 1;
        match self.forecast(key, hint, bounds) {
            Ok(prediction) => {
                self.predictions.made += 1;
                Some(prediction)
            }
            Err(why) => {
                self.predictions.unset[why as usize] += 1;
                None
            }
        }
    }

    /// Counts an operation the farm did not ask the estimator about, for reason `why`.
    /// Does nothing while predictions are off.
    pub fn unpredicted(&mut self, why: Unset) {
        if self.predictions.mode.predicts() {
            self.predictions.unset[why as usize] += 1;
        }
    }

    /// What to give the scheduler for `operation`, for which [`Learned::predict`] returned
    /// `prediction`: it in `on`, nothing in `shadow` (which remembers it to compare with the
    /// run) and in `off`.
    pub fn to_set(
        &mut self,
        operation: OperationId,
        prediction: Option<Prediction>,
    ) -> Option<Prediction> {
        let prediction = prediction?;
        if self.predictions.mode == PlacementPredictions::Shadow {
            if self.predictions.pending.len() >= STARTED_CAP {
                self.predictions.pending.pop_first();
            }
            self.predictions.pending.insert(operation, prediction);
        }
        Some(prediction).filter(|_| self.predictions.mode == PlacementPredictions::On)
    }

    /// The report of `lease` of `operation` is staged: the prediction remembered for the
    /// operation is compared with the run if the scheduler takes the report
    /// ([`Learned::finish`]).
    pub fn stage_prediction(&mut self, lease: LeaseId, operation: OperationId) {
        if let Some(prediction) = self.predictions.pending.get(&operation).copied() {
            self.predictions
                .staged
                .insert(lease, (operation, prediction));
        }
    }

    /// Forgets the prediction staged for `lease`, returning it.
    pub(super) fn unstage_prediction(
        &mut self,
        lease: LeaseId,
    ) -> Option<(OperationId, Prediction)> {
        self.predictions.staged.remove(&lease)
    }

    /// `run`, a report of `operation` the scheduler took, ends the operation (a requeue is
    /// another path): its prediction is forgotten, so the map holds what is in flight, and
    /// compared with the run if the estimator takes it as a sample.
    pub(super) fn end_prediction(
        &mut self,
        operation: OperationId,
        prediction: &Prediction,
        run: Option<&Run>,
    ) {
        self.predictions.pending.remove(&operation);
        if let Some(run) = run {
            self.compare_run(prediction, run);
        }
    }

    /// Compares `run` with `prediction`, if the estimator takes the run as a sample.
    fn compare_run(&mut self, prediction: &Prediction, run: &Run) {
        if run.outcome != RunOutcome::Success || run.wall_us < MIN_WALL_US {
            return;
        }
        let life = (run.wall_us / MINUTE_US)
            .saturating_add(1)
            .clamp(1, u64::from(LIFE_HORIZON));
        self.tally(Axis::Life, life, u64::from(prediction.life_minutes));
        if let Some(peak) = run.peak_memory_bytes.filter(|peak| *peak > 0) {
            self.tally(Axis::Mem, peak, prediction.memory_bytes);
        }
        if run.cpu_usage_us > 0 {
            let cores = run.cpu_usage_us.saturating_mul(1000) / run.wall_us;
            self.tally(Axis::Cpu, cores, prediction.cpu_millis);
        }
    }

    fn tally(&mut self, axis: Axis, used: u64, predicted: u64) {
        let bucket = Fit::of(used, predicted);
        self.predictions.vs_run[axis as usize][bucket as usize] += 1;
    }

    /// How many operations were given a prediction.
    #[must_use]
    pub fn predictions_made(&self) -> u64 {
        self.predictions.made
    }

    /// How many operations got no prediction for reason `why`.
    #[must_use]
    pub fn predictions_unset(&self, why: Unset) -> u64 {
        self.predictions.unset[why as usize]
    }

    /// How many finished runs fell in `fit` against their prediction on `axis`.
    #[must_use]
    pub fn predictions_vs_run(&self, axis: Axis, fit: Fit) -> u64 {
        self.predictions.vs_run[axis as usize][fit as usize]
    }

    /// The operations whose prediction waits for a run to be compared with, for a test.
    #[must_use]
    pub fn predictions_pending(&self) -> usize {
        self.predictions.pending.len()
    }

    /// Copies the predictions' counts into the scrape.
    pub fn publish_predictions(&self, totals: &PredictionTotals) {
        totals.set_made(self.predictions.made);
        for (reason, count) in self.predictions.unset.iter().enumerate() {
            totals.set_unset(reason, *count);
        }
        for (axis, row) in self.predictions.vs_run.iter().enumerate() {
            for (fit, count) in row.iter().enumerate() {
                totals.set_vs_run(axis, fit, *count);
            }
        }
    }
}

#[cfg(test)]
mod tests;
