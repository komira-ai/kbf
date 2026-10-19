//! The fail-closed gate library of the CI/CD stages (`docs/design/kbf-cicd.md`, sections 5.2,
//! 7 and 10).
//!
//! Every stage (beta, the gamma deploy and its 24 hour bake, prod) writes one
//! [`ResultDoc`], in one shape, and decides each gate the same way:
//!
//! - [`Metrics`] reads Prometheus text. A series that is not there is [`Sample::Absent`],
//!   never zero, and [`judge`] turns `Absent` into [`Verdict::Red`].
//! - [`Run`] writes the document `RUNNING` first and `FINISHED` (green, red or extended)
//!   on every exit it can see: a normal finish, an early return, a panic
//!   (a drop guard) and [`Run::fail_and_exit`]. A process killed by a signal, or one that
//!   calls `std::process::exit` itself, leaves the `RUNNING` file; [`ResultDoc::passed`] is
//!   false for it, so no reader counts it as a pass.
//! - Every write goes through a temporary file and a rename, so a reader sees the old
//!   document or the new one, never half of one.
//! - [`SetHash`] is the one definition of a release set's hash: the SHA-256 of the
//!   `SHA256SUMS` bytes. `cargo xtask` and the evaluator both call it.
//!
//! Time comes in as a [`Clock`] (unix seconds), so a test pins it and a golden file is stable.
//! The crate does no I/O except the document's atomic write, and links no async runtime
//! and no other kbf crate.

mod doc;
mod gate;
mod metrics;
mod run;

pub use doc::{
    CommitId, DocError, Floor, Header, MinLoad, ResultDoc, SCHEMA, SetHash, Stage, Status, Waiver,
    Window,
};
pub use gate::{GateResult, Limit, Verdict, judge};
pub use metrics::{Metrics, MetricsError, Sample};
pub use run::{Clock, GateError, Run, system_clock};
