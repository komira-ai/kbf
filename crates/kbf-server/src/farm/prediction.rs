//! The farm's side of `--placement-predictions` ([`crate::placement_predictions`]): the
//! builder that sets the mode.

use super::Farm;

impl Farm {
    /// This farm, giving its scheduler the estimator's predictions as `mode` says
    /// (`--placement-predictions`). Set before the farm takes a state store over: an
    /// operation the store holds is predicted when it is read back.
    #[must_use]
    pub fn with_placement_predictions(mut self, mode: crate::PlacementPredictions) -> Self {
        self.settings.predictions = mode;
        self.driver.set_predictions(&self.settings);
        self
    }
}
