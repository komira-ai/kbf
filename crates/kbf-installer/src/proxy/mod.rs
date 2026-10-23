//! The frontend swap orchestrator (U7a of `docs/design/frontend-proxy.md`, FP): the core
//! of the supervisor in `kbf-installer proxy`, written against traits so that it runs with
//! no listener, no child process and no filesystem beyond `update.json`.
//!
//! What is here: the [`Swap`] phase machine for a frontend host (two children side by
//! side) and for an all-in-one host (stop, hold, start); recovery after a proxy restart
//! ([`Swap::adopt`]); the operator's rollback; the retire rule; and [`FileRecorder`], the
//! fsynced `update.json`; and [`orders::OrderSpool`], the one writer of every order the
//! proxy puts in `orders/`. The update API is in [`api`] (U8a).
//!
//! Real implementations of the ports (U7b), each behind the trait it implements:
//! - [`gate_real::ProxyGate`], the `Gate` on `kbf-proxy`'s hold gate (it keeps the
//!   `HoldGuard`, opens a slot at its first release, adopts running children out of the
//!   startup hold);
//! - [`child_real::ProcChild`] (Linux), the `Child` on processes watched by pidfd;
//! - [`runtime::SystemClock`] and [`runtime::RequestQueue`], the `Clock` and the `Requests`;
//! - [`supervisor`] (Linux), the proxy's start: adopt the children a previous proxy left;
//! - [`supervisor_swap`], the update API's `Supervisor`: the swap on its own thread;
//! - [`crate::migrator::DbMigrator`], the `Migrator` over the state database;
//! - [`releases`], the proxy's own copies of the `kbf` it runs and the [`ports::RootOrders`] that
//!   goes with them (a status file the proxy writes itself): what `kbf-installer proxy
//!   --kbf-binary PATH` turns on (see [`handover`]); without the flag nothing uses it;
//! - [`binary_watch`], which polls the kbf at that path and starts the swap to a new one (the
//!   trigger of a package upgrade, in place of the update API).
//!
//! Child control over HTTP: [`child_http`] is the blocking loopback client that the real
//! child control and the fleet read call a child's ports with (U8b); [`admin`] is the
//! `ChildAdmin` built on it (`admin_protocol` 1, sending the proxy secret header on every
//! call). The proxy verb builds it in [`supervisor::child_admin`] and uses it for the start:
//! [`supervisor::adopt_children`] adopts a child a previous proxy left (an active one at
//! once, a candidate after it takes the generation) and, where none runs, starts the release
//! `status.json` calls installed ([`boot`]). [`supervisor_swap::SwapSupervisor`] is the update
//! API's `Supervisor` over all of it: it runs [`Swap`] on a thread of its own, resumes an
//! update that was in flight when the proxy stopped, and builds the status the API serves.
//! Its seams are in its module documentation: the compatibility gate is the API's.
//!
//! The update API's seams are bound in [`handover`]: the order spool, `status.json`, the
//! request map and the restart answer are real there, and so is the supervisor
//! ([`supervisor_swap::SwapSupervisor`]).
//!
//! Not built, stated: [`supervisor::adopt_children`] still adopts a child only when no update
//! is in flight (the startup hold stays otherwise); choosing between two live children is not
//! done. `Migrator` is real in the verb: the cold start's hook is [`supervisor::host_migrator`],
//! a [`crate::migrator::DbMigrator`] over the database `installer.toml` names. The swap's
//! migrator is [`handover_swap::StateDbMigrator`], the same [`crate::migrator::DbMigrator`]
//! built per migration (see [`handover_swap`]). `Admin::ready` reports `health` and
//! `capabilities` as not ready until their probes exist, so a swap on a real child stops at
//! `ready`.
//! [`root_orders::SpoolRootOrders`] is the swap's order port ([`handover_swap`] builds it), or,
//! with `--kbf-binary`, [`releases::LocalOrders`].
//!
//! Assumptions, each in one place:
//! - **Q3** `resume` is an admin call that reloads fully, then re-takes the generation:
//!   [`ports::ChildAdmin::resume`].
//! - **Cold start** ([`boot`]). With no live child the proxy starts `installed_commit` of
//!   `status.json`, running `releases/<sha>/kbf` (never `releases/current`) in slot A as a
//!   candidate, and makes it take the generation: no live holder, and the child keeps the
//!   epoch. It starts nothing unless the binary is executable and the release has
//!   its `VERIFIED` marker. A failed attempt is stopped (SIGKILL only if the process never
//!   answered ready, SIGTERM otherwise) and tried again after 2, 4, ... 30 s, ten attempts in
//!   all; then the streams stay held until the proxy restarts. "Ready" for this boot is the
//!   child's operator `/readyz` alone, because the health and capabilities probes are not built
//!   ([`cold::ReadyRule`]); the takeover is the final check. The reason of each failed attempt
//!   is a fixed word in `boot.json` (state directory) and the log.
//! - **Start.** A frontend host starts the new child as a candidate (`--await-takeover`,
//!   [`model::Start`]); an all-in-one host starts the new process, and the old release put
//!   back after a failed start, to serve at once, because no takeover is ever sent there.
//! - **Q8** open streams come from the proxy ([`ports::Gate::open_streams`]); open
//!   streams and open worker sessions from the child's admin role view
//!   ([`ports::ChildAdmin::open_counts`]); queued operations do not gate retire.
//! - **Migration** (decided, FP section 8): the swap calls [`ports::Migrator::migrate_forward`]
//!   before the candidate starts (for an all-in-one host, after the old process exits).
//!   [`crate::migrator::DbMigrator`] is the real one: it applies the installer's embedded,
//!   expand-only migrations up to the `state_schema` the staged release reports, as the
//!   role `installer.toml` names, and refuses with a stable word (`installer_too_old`,
//!   `not_additive`, `database_busy`, `migration_failed`, ...) that leads
//!   `failure.reason` of step `migrate`. **Wired into the cold start:** the verb's boot hook
//!   (`Boot.migrator`) is [`supervisor::host_migrator`], so before the installed release's
//!   child starts the database is brought to the `state_schema` that release declares (asked
//!   of its own `--version --json`, because `status.json` forgets the installed release's
//!   facts when the root half restarts; the staged release's come from `status.json`). A
//!   refusal (`schema_unknown` for a release that would not say, plus the words above) is an
//!   attempt that failed as `migration-failed` with nothing started, retried like any other,
//!   its word in the log. **No database configured:** the verb refuses to start (and
//!   `--check` fails, exit 2): a frontend or all-in-one host runs on Postgres, so starting
//!   without a migration would only move the failure into the child; tests use an
//!   explicit fake, never a missing key.
//! - **update.json** keeps the recorded process identity of both children
//!   ([`model::Intent::old_proc`], `new_proc`), which FP section 4 does not list: adoption
//!   needs pid, start time and executable. A candidate spawned and not yet recorded when
//!   the proxy dies is found again only by the start-up adoption: `spawn` writes
//!   `<slot>.json` (pid, start time, executable, commit) before it returns
//!   ([`child_real::ProcChild::recorded`]), and [`supervisor::adopt_children`] reads it;
//!   [`Swap::adopt`] reads `update.json` alone.
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
//! - **Nodes** (step 9): the order for the nodes is posted again, one `Timing::poll` apart,
//!   while the new child gives no answer, for `Timing::desired_within`. A child that answers
//!   and refuses it (a 4xx, or a 409 with a code such as `not_own`) and one that never answers
//!   end the update `failed` with `failure.step == nodes` (`failure.reason` is the child's code
//!   and words, or `timeout`). The frontend was swapped and keeps serving, and the old child is
//!   still drained and retired. The order names the update's source when it is not a fetch.
//! - **All-in-one stop**: after `quiesce`, the proxy sends SIGTERM to the old process
//!   ([`aio`]).
//! - A dead new child during the bake (the proxy running) leaves the update parked in
//!   `baking` (red) until the operator rolls back; nothing respawns a dead bake child in
//!   this code, and nothing but a proxy restart looks at a child that died outside an update.
//! - **Restart reconcile** ([`recover`]): retire never signals the recorded-active slot,
//!   so a restart past the switch first checks that slot. If the new child is dead (or
//!   not ours any more: a recycled pid) or does not hold the generation while the old
//!   child lives, the switch failed: `rolling_back` and `failure{step: switch}` are
//!   recorded before either child is touched, the old child resumes, and retire runs on
//!   the other one (a dead child needs no signal; a live one gets SIGTERM once idle,
//!   never SIGKILL). The update ends `failed` at `switch`, not `rolled_back`, which is the
//!   operator's answer. If both children are dead the recorded-active release is spawned
//!   again, takes the generation (no live holder, and the child keeps the epoch) and is
//!   recorded. A live child that does not answer `role` is not reconciled.

pub mod admin;
mod aio;
pub mod api;
pub mod binary_watch;
#[cfg(target_os = "linux")]
pub mod boot;
pub mod child_http;
#[cfg(target_os = "linux")]
pub mod child_real;
mod cold;
pub mod gate_real;
pub mod handover;
pub mod handover_swap;
pub mod model;
pub mod orders;
pub mod ports;
pub mod record;
mod recover;
pub mod releases;
mod retire;
pub mod root_orders;
pub mod running_binary;
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod supervisor;
pub mod supervisor_swap;
mod swap;

#[cfg(test)]
mod admin_tests;
#[cfg(test)]
mod aio_tests;
#[cfg(test)]
mod child_http_tests;
#[cfg(all(test, target_os = "linux"))]
mod child_rig;
#[cfg(test)]
mod confirm_boundary_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod fault_tests;
#[cfg(test)]
pub(crate) mod gate_real_tests;
#[cfg(test)]
mod gate_tests;
#[cfg(test)]
#[path = "../migrator_tests.rs"]
mod migrator_tests;
#[cfg(test)]
mod nodes_tests;
#[cfg(test)]
mod orders_tests;
#[cfg(test)]
mod reconcile_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod recover_tests;
#[cfg(test)]
mod releases_tests;
#[cfg(test)]
mod root_orders_tests;
#[cfg(test)]
mod swap_tests;

pub use model::{Halt, Intent, Phase, Plan, Timing};
pub use ports::Ports;
pub use record::FileRecorder;
pub use running_binary::RunningBinary;
pub use swap::Swap;
