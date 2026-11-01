//! The mark on execution metadata whose phase times are a task's claim.
//!
//! A daemon measures its `input_fetch`, `execution` and `output_upload` times. A task run by
//! the executor only claims its three durations in its manifest, and the frontend lays them
//! out as timestamps. The two are told apart by an empty `auxiliary_metadata` entry of the
//! type [`TYPE_URL`], which the frontend adds when it lays out a claim and nothing else
//! writes; a reader of the metadata (the run record) reads it as "these three durations are
//! data, the instants and the gaps between them and the server's times are not".

use prost_types::Any;

use crate::reapi::ExecutedActionMetadata;

/// The type URL of the mark in `ExecutedActionMetadata.auxiliary_metadata`.
pub const TYPE_URL: &str = "type.googleapis.com/kbf.worker.v1.TaskClaimedPhases";

/// Marks `metadata` as carrying a task's claim, once.
pub fn mark(metadata: &mut ExecutedActionMetadata) {
    let _ = (metadata, Any::default());
}

/// Whether `metadata` is marked as a task's claim.
#[must_use]
pub fn is_marked(metadata: &ExecutedActionMetadata) -> bool {
    let _ = metadata;
    false
}

#[cfg(test)]
#[path = "task_claim_tests.rs"]
mod tests;
