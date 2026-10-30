//! `--placement-predictions`: whether the farm gives its scheduler the estimator's prediction
//! of what a new operation costs, so that placement charges the learned figures instead of
//! the cores the request asked for and no memory.
//!
//! The prediction comes from the estimator's learned row alone ([`crate::learned`]): an
//! operation whose key has no usable row, or that the farm does not ask the estimator about,
//! gets none and is charged as it is without the flag. The flat figures are the first stage
//! of using what the farm learned in placement; they are shipped and measured alone, before
//! any per-minute profile is read.

use crate::{Args, ConfigError, LearnedSizes, PlacementLoad};

/// What `--placement-predictions` does with the estimator's answer for a new operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PlacementPredictions {
    /// No prediction is computed, counted or given to the scheduler.
    #[default]
    Off,
    /// The prediction is computed and counted at submit and compared with the run when it
    /// ends; the scheduler is given nothing, so placement is what `off` does.
    Shadow,
    /// The prediction is computed at submit (on the legacy core, and again for an operation
    /// read back from the state store) and given to the scheduler as the cost of the
    /// operation's lease.
    On,
}

impl PlacementPredictions {
    /// The value the flag takes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::On => "on",
        }
    }

    /// Whether a prediction is computed at all (`shadow` and `on`).
    #[must_use]
    pub const fn predicts(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl Args {
    /// Refuses a `--placement-predictions` that would do nothing, or something other than
    /// it says.
    ///
    /// # Errors
    /// [`ConfigError::PredictionsNeedLearnedSizes`]: `shadow` or `on` with
    /// `--learned-sizes off`, where the estimator has no rows to predict from;
    /// [`ConfigError::PredictionsNeedPlacementLoad`]: `on` with `--placement-load off`,
    /// where the scheduler books by cores and ignores the prediction.
    pub(crate) fn placement_predictions_fit(&self) -> Result<(), ConfigError> {
        let mode = self.placement_predictions;
        if !mode.predicts() {
            return Ok(());
        }
        if self.learned_sizes == LearnedSizes::Off {
            return Err(ConfigError::PredictionsNeedLearnedSizes(mode.name()));
        }
        if mode == PlacementPredictions::On && self.placement_load == PlacementLoad::Off {
            return Err(ConfigError::PredictionsNeedPlacementLoad);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
