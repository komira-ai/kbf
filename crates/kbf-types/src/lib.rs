//! Shared value types for kbf: the content digest (function, hash, size), an image named
//! by manifest digest, platform properties, QoS levels, lease identifiers and kinds, node
//! phases, farm time, the vocabulary of scheduled work (operations, workers, request
//! vectors, control-log records), the `StateMachine` trait and `Effect` enum that the
//! pure cores implement and emit, and the enrollment vocabulary (tokens, node ids, roles,
//! the reasons a frontend refuses a node).
//!
//! The crate has two parts. The `sched` module is generic: scheduling vocabulary with no
//! REAPI and no content `Digest`. It is `js_types::sched` of the `komira-ai/jobscheduler`
//! repository, at the revision pinned in the workspace `Cargo.toml`, re-exported here.
//! Work is keyed there by a `WorkKey` and a stored result is a `ResultRef`; the build's
//! `ActionKey` and `Digest` convert to and from them (`action_key.rs`, `result_ref.rs`).
//! Everything else is specific to building. The generic items are re-exported at the
//! root, so existing paths are unchanged. The root's `LeaseId` and `Resources` are
//! `sched`'s, not the different types of the same names at the root of `js_types`.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-integration-test` and the lists in `clippy.toml` enforce it.
//! Every value here is plain data: two processes that build a value from the same
//! inputs get equal values that order and format identically.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod action_key;
mod digest;
mod enroll;
pub mod enroll_api;
mod host_role;
mod image;
pub mod object_store_api;
pub mod operator_api;
mod platform;
mod release;
mod result_ref;

/// The scheduler's vocabulary, from `js_types::sched`.
pub mod sched {
    pub use js_types::sched::*;
}

pub use action_key::{ActionKey, NotAnActionKey, work_key};
pub use digest::{Digest, DigestFunction, ParseDigestError};
pub use enroll::{
    CaPin, NodeId, NodeIdError, PartFault, RefusalCode, RefusalReason, Role, RoleError, RoleSet,
    RoleSetError, Token, TokenError, TokenId, TokenSecret,
};
pub use host_role::{HostRole, HostRoleError};
pub use image::{ImageError, ImageRef};
pub use platform::{Platform, PlatformError};
pub use release::ReleasePlatform;
pub use sched::{
    Answer, ControlRecord, CustomQos, Effect, Failure, FarmFault, FarmRun, FarmTime, FencePolicy,
    LeaseGrant, LeaseId, LeaseKind, LostWhy, MemoryKill, MemoryRun, NodePhase, OperationId,
    Outcome, Qos, QosError, Refusal, RefusalRecord, Resources, ResultRecord, ResultRef, StartLease,
    StateMachine, Supported, TooNew, VERSION_CAP, Version, VersionFault, WaiterId, Waiting,
    WorkKey, WorkerId, understood,
};
