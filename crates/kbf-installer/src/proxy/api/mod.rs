//! The update API of the frontend proxy (`docs/design/frontend-proxy.md` section 4, unit
//! U8a): the routes, the bearer-token check, the single actor that owns the state of the
//! update, and the idempotent request ids.
//!
//! Nothing here binds a socket, starts a process or touches the filesystem beyond the
//! proxy's state directory. Everything U8a needs from the rest of the system is a trait
//! in [`traits`]: [`traits::OrderWriter`]
//! (the `frontend_update`/`revert` records), [`traits::StatusReader`] (`status.json`),
//! [`traits::Supervisor`] (the U7 swap), [`traits::CompatGate`] (R1/R4, a stub until U9)
//! and [`traits::Forwarder`] (the operator port). U7b and U8b supply the real ones.
//!
//! The vocabulary is the swap's, in [`super::model`]: [`Phase`](super::model::Phase) (the
//! API owns `accepted`, `fetching`, `verifying` and reports them; the swap owns `staged`
//! on), [`Step`](super::model::Step), [`Failure`](super::model::Failure),
//! [`Locator`](super::model::Locator), [`Verb`](super::model::Verb),
//! [`StagedFacts`](super::model::StagedFacts) and [`Slot`](super::model::Slot). [`model`]
//! here holds only what is the API's own: the request and response bodies and the rollback
//! rule.
//!
//! # How an update moves
//!
//! `POST` checks the request (409 and 422 cases), saves its `request_id`, writes a
//! `frontend_update` order (a locator; the root half fetches, verifies and stages without
//! flipping, FP section 2) and answers 202 with phase `accepted`. From then on the API
//! reads `status.json` on every call that matters to it and on the actor's own tick
//! ([`actor::ActorTiming::tick`]):
//!
//! - `state: fetching` is phase `fetching`;
//! - `state: refused` with `refused_order_id` naming our order is phase `failed` with the
//!   refusal word as `failure.reason` ([`refusal`]); no child is started;
//! - `staged_commit` equal to ours with `staged{}` facts runs the compatibility gate, and
//!   a compatible release is handed to the [`traits::Supervisor`], which owns every later
//!   phase; an incompatible one fails the update and a `revert` order drops the staged
//!   release;
//! - none of these within [`core::StagingDeadline`] of the order (default 15 minutes)
//!   fails the update with `failure{step: stage, reason: timeout}` and a `revert` order
//!   drops whatever the installer may still stage. This is the end of an update whose
//!   order was lost, whose refusal `status.json` does not name, whose installer is dead,
//!   or whose `status.json` cannot be read (the deadline does not wait for the file);
//!   without it such an update would stay `accepted` and block every later one.
//!
//! The phase `verifying` is never reported by the API: `status.json` has no state between
//! `fetching` and `staged`.
//!
//! A staging update survives a restart of the process: its deadline is written with its
//! `request_id` ([`idempotency::Waiting`]), and a new [`core::Core`] takes the update up
//! again with the deadline the first one set. An update that was handed over or has ended
//! is not taken up again (the supervisor has the first; the second is `request_id_retired`
//! to a retry).
//!
//! # Assumptions on open questions (each lives in one place)
//!
//! - **Order ids (Q1).** `order_id`, `epoch` and `generation` come from the
//!   [`traits::OrderWriter`]; the API does not know where their counter lives.
//! - **Which refusal is ours (Q1).** `status.json` has one `refusal` string for the last
//!   thing the installer refused, with no name for the order. `acked_order_id` is the id of
//!   the last node order acted on: a `frontend_update` has a floor of its own, which a
//!   refusal does not move, and another writer's ids on the host may be any size, so it
//!   cannot attribute a refusal. The API reads `refused_order_id`
//!   ([`traits::InstallerStatus`]) and a refusal is an update's own only when that names
//!   its order ([`core::Core::advance`]); a stale refusal names an older order, or none.
//!   The root half does not write the field yet, so today a refusal ends its update at
//!   the staging deadline, with reason `timeout`, not with its own word.
//! - **Compatibility before the fetch (Q1, U9).** `POST` asks the gate with no facts, so
//!   a 409 `incompatible` or 422 at `POST` time is possible only for what U9 can know
//!   without the release; the verdict on the staged facts arrives as `failed` /
//!   `step: compat` ([`traits::CompatGate`]).
//! - **Request map (Q5).** `requests.json` in the proxy state directory is this module's
//!   own file ([`idempotency`]); `update.json` (the swap's intent) is U7's.
//! - **Restart pending (FP section 10).** `restart_pending` is asked of the caller's
//!   [`traits::RestartPending`]; the proxy passes its `RunningBinary`. No marker file.
//! - **The confirm window** the orders ask for is [`core::Config::window_s`].
//! - **Operator port forwards.** `/v1/update/pause` and `/v1/update/resume` are the paths
//!   of the two forwarded verbs (FP names `/pause`, `/resume` under `/v1/update`).
//! - **Who runs the forward Postgres migration, and with which credential, is still open
//!   and is not touched here:** it belongs behind the `Migrator` trait of the update
//!   driver (U7b), before a candidate child starts, and the API only sees its effect as a
//!   `failed` phase from the supervisor.
//!
//! # What U7b and U8b wire
//!
//! [`traits::Supervisor`] is not implemented by [`super::Swap`] itself; the adapter (U7b)
//! must bridge these gaps:
//! - `Swap::run` blocks until a terminal phase and borrows its ports, while `start` must
//!   return: the adapter runs the swap on its own thread and builds the [`super::Plan`]
//!   from the [`traits::UpdateSpec`] plus what it knows (mode, `old_slot`, timing, and
//!   `from`, which the spec carries as an `Option`).
//! - `abort` and `rollback` become [`super::model::Request`]s on the swap's
//!   [`super::ports::Requests`] queue and answer with the phase read from
//!   [`super::Swap::intent`]; the swap decides later whether it can honor them.
//! - `status` is built from the intent record (`phase`, `phase_started`, slots) plus
//!   [`super::ports::Gate::open_streams`] and [`super::ports::ChildAdmin::open_counts`]
//!   (`open_streams` and `open_sessions` of the child's role view).
//!   The intent keeps `nodes_posted` as a bool, the API wants a count: the adapter maps
//!   it to 0 or 1, which is all [`model::rollback_gate`] reads.
//! - `force_retire` has no counterpart in the swap, which leaves a draining old child
//!   until the operator force-retires it: the adapter does it through the child and gate
//!   ports.
//!
//! U7b implements [`traits::Supervisor`] over the real children and the `confirm`
//! order; U8b implements [`traits::OrderWriter`] over `orders/` and the counter and the
//! OIDC [`auth::Authorize`] (#1088).
//!
//! # How a listener hosts the API
//!
//! [`handler::Handler`] is the API behind plain values: a listener that has authenticated
//! a caller calls [`handler::Handler::handle`] with `{method, path, body}` and gets
//! `{status, headers, body}`. The router's own token check, rate limit and operator-port
//! forwarding are not in that path (the listener authenticates, and sends the forwarded
//! paths on itself). Two promises come with it:
//!
//! - **The actor never runs on the listener's runtime** ([`actor`]): it has a thread of
//!   its own, ticks on it, and a caller waits for it at most [`actor::ActorTiming::deadline`]
//!   (then 503), so a stalled `status.json` read or fleet read stalls no one else.
//! - **The actor's death is not silent**: `on_death` of [`handler::Handler::start`] runs
//!   once on the actor's thread when the actor dies of a crash or a panic, and the
//!   listener passes what stops its process. Every request after that is a 503.

pub mod actor;
pub mod auth;
pub mod core;
pub mod handler;
pub mod idempotency;
pub mod model;
pub mod refusal;
pub mod router;
pub mod traits;

#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod handler_tests;
#[cfg(test)]
mod idempotency_tests;
#[cfg(test)]
mod router_tests;
#[cfg(test)]
mod staging_tests;
#[cfg(test)]
mod tests;
