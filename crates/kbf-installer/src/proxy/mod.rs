//! The frontend swap orchestrator (U7a of `docs/design/frontend-proxy.md`, FP): the core
//! of the supervisor in `kbf-installer proxy`, written against traits so that it runs with
//! no listener, no child process and no filesystem beyond `update.json`.
//!
//! What is here: the [`Swap`] phase machine for a frontend host (two children side by
//! side) and for an all-in-one host (stop, hold, start); recovery after a proxy restart
//! ([`Swap::adopt`]); the operator's rollback; the retire rule; and [`FileRecorder`], the
//! fsynced `update.json`. What is not: the proxy itself, the real child control and the
//! orders spool (U4b, U7b), the update API (U8).
//!
//! Assumptions, each in one place:
//! - **Q3** `resume` is an admin call that reloads fully, then re-takes the generation:
//!   [`ports::ChildAdmin::resume`].
//! - **Q8** open streams come from the proxy ([`ports::Gate::open_streams`]); open
//!   executions and queued operations from the child ([`ports::ChildAdmin::open_counts`]).
//! - **Migration** (open): who runs the forward Postgres migration, and with which
//!   credential, is undecided; the swap only calls [`ports::Migrator::migrate_forward`]
//!   before the candidate starts. For an all-in-one host it runs after the old process
//!   exits.
//! - **update.json** keeps the recorded process identity of both children
//!   ([`model::Intent::old_proc`], `new_proc`), which FP section 4 does not list: adoption
//!   needs pid, start time and executable. A candidate spawned and not yet recorded when
//!   the proxy dies is not found again (U7b should write a pid file at spawn).
//! - **confirm** is sent once the bake passes and is idempotent at the root half, because
//!   a restart between the confirm and the `nodes` record sends it again.
//! - **abort** ends an update as `failed` with step `abort`, not `rolled_back`.
//! - **Rollback** uses a hold, like the swap, so streams move atomically.
//! - **All-in-one stop**: after `quiesce`, the proxy sends SIGTERM to the old process
//!   ([`aio`]).
//! - A dead new child during the bake leaves the update parked in `baking` (red) until the
//!   operator rolls back; the proxy-supervised respawn with an epoch bump is U7b.

mod aio;
pub mod model;
pub mod ports;
pub mod record;
mod recover;
mod retire;
mod swap;

#[cfg(test)]
mod aio_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod recover_tests;
#[cfg(test)]
mod swap_tests;

pub use model::{Halt, Intent, Phase, Plan, Timing};
pub use ports::Ports;
pub use record::FileRecorder;
pub use swap::Swap;
