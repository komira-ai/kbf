//! The peak CPU window a runtime says a lease used, put in the usage that lease reports.

use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, ResourceUsage};
use kbf_types::LeaseId;
use prost::Message;

use crate::runtime::RuntimeError;
use crate::runtimes::AnyRuntime;

/// The usage lease `id` reports once its run has ended: none for a run that succeeded (its
/// result carries it), else what the runtime says a failed run used. The runtime is then
/// asked for the lease's busiest window, which goes in the usage the result carries or in the
/// failed run's usage. A run with no window to name changes nothing.
pub(crate) fn usage_with_peak(
    run: &dyn AnyRuntime,
    id: LeaseId,
    outcome: &mut Result<ActionResult, RuntimeError>,
) -> Option<ResourceUsage> {
    let mut usage = outcome.as_ref().err().and_then(|_| run.failed_usage(id));
    if let Some(peak) = run.take_peak_window_millicores(id) {
        match outcome {
            Ok(result) => peak_into_result(result, peak),
            Err(_) => {
                if let Some(usage) = usage.as_mut() {
                    set_peak(usage, peak);
                }
            }
        }
    }
    usage
}

/// `usage` saying its lease's busiest window used `peak` millicores.
fn set_peak(usage: &mut ResourceUsage, peak: u64) {
    usage
        .cgroup
        .get_or_insert_with(worker::CgroupUsage::default)
        .peak_window_millicores = Some(peak);
}

/// The usage `result` carries, saying its lease's busiest window used `peak` millicores.
/// A result that carries none is left as it is.
fn peak_into_result(result: &mut ActionResult, peak: u64) {
    let Some(any) = result.execution_metadata.as_mut().and_then(|metadata| {
        (metadata.auxiliary_metadata.iter_mut()).find(|any| any.type_url == ResourceUsage::TYPE_URL)
    }) else {
        return;
    };
    let Ok(mut usage) = ResourceUsage::decode(any.value.as_slice()) else {
        return;
    };
    set_peak(&mut usage, peak);
    *any = usage.to_any();
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::ExecutedActionMetadata;

    use super::*;

    /// Catches a peak window put in a result whose usage is not there (a result with no
    /// metadata, or with other metadata), a usage that does not decode being replaced by
    /// one holding only the peak, and the peak not reaching a usage that decodes.
    #[test]
    fn a_peak_goes_into_the_usage_a_result_carries_and_nowhere_else() {
        let mut bare = ActionResult::default();
        peak_into_result(&mut bare, 5);
        assert_eq!(bare, ActionResult::default());

        let other = prost_types::Any {
            type_url: "type.googleapis.com/other".to_owned(),
            value: vec![1],
        };
        let mut unrelated = ActionResult {
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![other],
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        };
        let before = unrelated.clone();
        peak_into_result(&mut unrelated, 5);
        assert_eq!(unrelated, before);

        let garbled = prost_types::Any {
            type_url: ResourceUsage::TYPE_URL.to_owned(),
            value: vec![0xff, 0xff],
        };
        let mut broken = ActionResult {
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![garbled],
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        };
        let before = broken.clone();
        peak_into_result(&mut broken, 5);
        assert_eq!(broken, before);

        let mut usage = ResourceUsage {
            wall_micros: 8,
            ..ResourceUsage::default()
        };
        let mut result = ActionResult::default();
        carry_usage_into(&mut result, &usage);
        peak_into_result(&mut result, 5);
        set_peak(&mut usage, 5);
        assert_eq!(ResourceUsage::in_result(&result), Some(usage));
    }

    fn carry_usage_into(result: &mut ActionResult, usage: &ResourceUsage) {
        result
            .execution_metadata
            .get_or_insert_with(ExecutedActionMetadata::default)
            .auxiliary_metadata
            .push(usage.to_any());
    }
}
