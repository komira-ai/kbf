//! The farm's side of `--placement-predictions` ([`crate::placement_predictions`]): the
//! prediction of an operation read back from the state store, and the builder that sets the
//! mode. The prediction of a new operation is made where the estimator is asked about it
//! (`State::learn_submit`), and given to the scheduler by the submit that numbered the
//! operation, before the submit is fed.

use kbf_estimator::Hint;
use kbf_sched::{Prediction, Scheduler};
use kbf_types::LeaseKind;

use super::Farm;
use super::durable::StoredOperation;
use super::side::Side;
use crate::learned::{Skip, Unset, basis_of_os, key_of};
use crate::machine::Platform;

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

impl Side {
    /// The prediction to give the scheduler for `op`, a queued operation read back from the
    /// state store at a start. Its client's evidence is not stored, so every figure of its
    /// request is taken as the client's: the prediction is never below what the request
    /// books. The estimate is a lookup in the summaries held in memory: no store is read
    /// here, so the caller may hold the farm's lock.
    pub(super) fn predict_stored(
        &mut self,
        sched: &Scheduler,
        op: &StoredOperation,
    ) -> Option<Prediction> {
        if !self.learned.predictions().predicts() {
            return None;
        }
        let request = &op.request;
        if request.kind != LeaseKind::Action || request.resources.gpus != 0 {
            self.learned.unpredicted(Unset::Kind);
            return None;
        }
        let Some(subject) = &op.subject else {
            self.learned.unpredicted(Unset::NoSubject);
            return None;
        };
        let platform = Platform::of(&request.needs);
        let key = basis_of_os(platform.os_family)
            .ok_or(Skip::NoOs)
            .and_then(|basis| key_of(&op.action.instance, &platform, basis, subject));
        let key = match key {
            Ok(key) => key,
            Err(why) => {
                self.learned.unpredicted(Unset::of_key_error(why));
                return None;
            }
        };
        let bounds = self.bounds_for_os(sched, platform.os_family);
        let hint = Hint {
            memory_bytes: Some(request.resources.memory_bytes),
            cpu_millis: Some(u32::try_from(request.resources.cpu_millis).unwrap_or(u32::MAX)),
        };
        let predicted = self.learned.predict(&key, &hint, &bounds);
        self.learned.to_set(op.operation, predicted)
    }
}
