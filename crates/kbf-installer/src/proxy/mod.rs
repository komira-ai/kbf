//! The frontend swap orchestrator (U7a of `docs/design/frontend-proxy.md`, FP): the core
//! of the supervisor in `kbf-installer proxy`, written against traits so that it runs with
//! no listener, no child process and no filesystem beyond `update.json`.
//!
//! What is here: the [`Swap`] phase machine for a frontend host (two children side by
//! side) and for an all-in-one host (stop, hold, start); recovery after a proxy restart
//! ([`Swap::adopt`]); the operator's rollback; the retire rule; and [`FileRecorder`], the
//! fsynced `update.json`. The update API is in [`api`] (U8a).
//!
//! Real implementations of the ports (U7b), each behind the trait it implements:
//! - [`gate_real::ProxyGate`], the `Gate` on `kbf-proxy`'s hold gate (it keeps the
//!   `HoldGuard`, opens a slot at its first release, adopts running children out of the
//!   startup hold);
//! - [`child_real::ProcChild`] (Linux), the `Child` on processes watched by pidfd;
//! - [`runtime::SystemClock`] and [`runtime::RequestQueue`], the `Clock` and the `Requests`;
//! - [`supervisor`] (Linux), the proxy's start: adopt the children a previous proxy left.
//!
//! Still fakes, stated: `ChildAdmin` (the child's admin port is U2, `proxy/u2b-admin`,
//! not merged), `Migrator` (who migrates is open), `RootOrders` (the orders spool and
//! `status.json` reader belong to the update API's unit, U8b). Until `ChildAdmin` exists
//! the proxy verb cannot run a swap or resume one, so [`supervisor::adopt_children`] adopts
//! a child only when no update is in flight.
//! [`child_http`] is the blocking loopback client the future real `ChildAdmin`
//! adapter and the fleet read will call a child's ports with (U8b); nothing calls it yet.
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
//! - **abort** ends an update as `failed` with step `abort`, not `rolled_back`; so does an
//!   abort that arrives while the old child quiesces (it is `abort`, not `switch`).
//! - **Compatibility gate (R1, R4)** is not here: [`model::StagedFacts`] is carried and
//!   only its commit is read. The gate that reads `protocol`, `min_protocol` and
//!   `state_schema` is U9.
//! - **Order locator (Q1)**: [`model::Locator`] carries `verb` with the other five fields.
//! - **Rollback** uses a hold, like the swap, so streams move atomically. It records
//!   `rolling_back` in `update.json` before it touches either child (it has no phase of
//!   its own), so a restart finishes it, or puts the new child back if it never landed.
//!   The new child served: it is never killed, only drained and sent SIGTERM.
//! - **Confirm** is sent up to `Timing::confirm_attempts` times with a doubling wait. If
//!   it never lands, the new child keeps serving, the staged release is kept, the nodes
//!   are not sent, the old child is drained and retired, and the update ends `failed`
//!   with `failure.step == confirm`. After the switch no failure drops the staged release.
//! - **All-in-one stop**: after `quiesce`, the proxy sends SIGTERM to the old process
//!   ([`aio`]).
//! - A dead new child during the bake (the proxy running) leaves the update parked in
//!   `baking` (red) until the operator rolls back; the proxy-supervised respawn with an
//!   epoch bump is U7b.
//! - **Restart reconcile** ([`recover`]): retire never signals the recorded-active slot,
//!   so a restart past the switch first checks that slot. If the new child is dead (or
//!   not ours any more: a recycled pid) or does not hold the generation while the old
//!   child lives, the switch failed: `rolling_back` and `failure{step: switch}` are
//!   recorded before either child is touched, the old child resumes, and retire runs on
//!   the other one (a dead child needs no signal; a live one gets SIGTERM once idle,
//!   never SIGKILL). The update ends `failed` at `switch`, not `rolled_back`, which is the
//!   operator's answer. If both children are dead the recorded-active release is spawned
//!   again, takes the generation (no live holder, so the child bumps the epoch) and is
//!   recorded. A live child that does not answer `role` is not reconciled.

mod aio;
pub mod api;
#[cfg(target_os = "linux")]
pub mod child_real;
pub mod gate_real;
pub mod child_http;
pub mod model;
pub mod ports;
pub mod record;
mod recover;
mod retire;
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod supervisor;
mod swap;

#[cfg(test)]
mod aio_tests;
#[cfg(all(test, target_os = "linux"))]
mod child_rig;
#[cfg(test)]
mod child_http_tests;
#[cfg(test)]
mod confirm_boundary_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod fault_tests;
#[cfg(test)]
mod gate_real_tests;
#[cfg(test)]
mod gate_tests;
#[cfg(test)]
mod reconcile_tests;
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
