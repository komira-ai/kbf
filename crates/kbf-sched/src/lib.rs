//! The scheduler core: operation states, leases and fencing, result acceptance, and
//! assignment of actions to workers, as a pure state machine.
//!
//! The code lives in the `js-sched` crate of the `komira-ai/jobscheduler` repository
//! (`crates/js-sched`), at the revision pinned in the workspace `Cargo.toml`, together
//! with its unit tests, its simulation tests and their golden files (one copy, there);
//! this crate re-exports it under its old name so that every path
//! (`kbf_sched::Scheduler`, `kbf_sched::fence::LEASE_GRACE`, ...) stays as it was. It is
//! still a pure crate (no async runtime, network, clock, randomness or hashed
//! collections): the layering test in `kbf-integration-test` and the lists in
//! `clippy.toml` enforce it.
//!
//! What stays in kbf is the bridge between the build's types and the scheduler's: the
//! `ActionKey` and `WorkKey` conversions, and the `Digest` and `ResultRef` ones, are in
//! `kbf-types`.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use js_sched::*;
/// The whole-node settings of a scheduler ([`Scheduler::with_whole_node`]) and the cap a
/// request can be stopped by ([`WholeWait::Cap`]), which `js-sched` takes from `js-whole`.
pub use js_whole::{WholeNodeConfig, caps::Blocked};

/// Every [`WholeReason`], in the order they are shown.
pub const WHOLE_REASONS: [WholeReason; 10] = [
    WholeReason::Early,
    WholeReason::Holding,
    WholeReason::Barred,
    WholeReason::Cap(Blocked::Ceiling),
    WholeReason::Cap(Blocked::KeepForOrdinary),
    WholeReason::Cap(Blocked::MaxDraining),
    WholeReason::Cap(Blocked::PerConsumer),
    WholeReason::LastCapable,
    WholeReason::Queued,
    WholeReason::Unservable,
];

/// The name an operator sees for `reason`: the `reason` label of the whole-node gauges and
/// the `reason` of a request in `GET /v1/queue`.
#[must_use]
pub const fn whole_reason_name(reason: WholeReason) -> &'static str {
    match reason {
        WholeReason::Early => "early",
        WholeReason::Holding => "holding",
        WholeReason::Barred => "barred",
        WholeReason::Cap(Blocked::Ceiling) => "cap_ceiling",
        WholeReason::Cap(Blocked::KeepForOrdinary) => "cap_keep_for_ordinary",
        WholeReason::Cap(Blocked::MaxDraining) => "cap_max_draining",
        WholeReason::Cap(Blocked::PerConsumer) => "cap_per_consumer",
        WholeReason::LastCapable => "last_capable",
        WholeReason::Queued => "queued",
        WholeReason::Unservable => "unservable",
    }
}
