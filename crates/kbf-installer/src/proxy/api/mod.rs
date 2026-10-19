//! The update API of the frontend proxy (`docs/design/frontend-proxy.md` section 4, unit
//! U8a): the routes, the bearer-token check, the single actor that owns the state of the
//! update, and the idempotent request ids.
//!
//! Nothing here binds a socket, starts a process or touches the filesystem beyond the
//! proxy's state directory. Everything U8a needs from the rest of the system is a trait
//! in [`traits`]: [`traits::OrderWriter`]
//! (the `frontend_update`/`revert` records), [`traits::StatusReader`] (`status.json`),
//! [`traits::Supervisor`] (the U7 swap), [`traits::CompatGate`] (rule R1, made by
//! [`compat::R1Gate`] over a [`fleet::FleetSnapshot`]), [`fleet::FleetReader`] (the
//! snapshot's three reads of the active child, made over HTTP by [`fleet_http::ChildFleet`])
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
//! - `state: refused` for an order at or after ours is phase `failed` with the refusal
//!   word as `failure.reason` ([`refusal`]); no child is started;
//! - `staged_commit` equal to ours with `staged{}` facts reads the fleet from the active
//!   child ([`fleet::FleetReader`], once) and runs the compatibility gate on the facts and
//!   the fleet, and a compatible release is handed to the [`traits::Supervisor`], which
//!   owns every later phase; an incompatible one fails the update (`step: compat`,
//!   `reason: R1: <what>`) and a `revert` order drops the staged release.
//!
//! The phase `verifying` is never reported by the API: `status.json` has no state between
//! `fetching` and `staged`.
//!
//! # Assumptions on open questions (each lives in one place)
//!
//! - **Order ids (Q1).** `order_id`, `epoch` and `generation` come from the
//!   [`traits::OrderWriter`]; the API does not know where their counter lives.
//! - **Which refusal is ours (Q1).** `status.json` has one `refusal` string and
//!   `acked_order_id`; a `refused` state whose `acked_order_id` is at least our order's id
//!   is ours ([`core::Core::advance`]). A stale refusal has a lower id.
//! - **Compatibility before the fetch (Q1, U9).** `POST` asks the gate with no facts and
//!   no fleet, so a 409 `incompatible` or 422 at `POST` time is possible only for what a
//!   gate can know without the release ([`compat::R1Gate`] knows nothing: R1 needs the
//!   release's `min_protocol`); the verdict on the staged facts arrives as `failed` /
//!   `step: compat` ([`traits::CompatGate`]).
//! - **The fleet is read by the actor, in the actor.** [`fleet::FleetReader::read`] runs
//!   once per update inside [`core::Core`], which blocks on it as it does on the other
//!   sync seams: every API call waits while it runs, so a real reader bounds the three
//!   requests by one deadline in total and names the active child on every call, not
//!   when it is built (the rules are on the trait, and [`fleet_http::ChildFleet`] keeps
//!   them). A failed read is a value in the snapshot and the gate refuses on it.
//! - **Request map (Q5).** `requests.json` in the proxy state directory is this module's
//!   own file ([`idempotency`]); `update.json` (the swap's intent) is U7's.
//! - **Restart marker (FP section 10).** The path of the postinst marker is not fixed by
//!   the design; [`traits::RestartMarker`] takes it from the caller.
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
//!   [`super::ports::Gate::open_streams`] and [`super::ports::ChildAdmin::open_counts`].
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
pub mod compat;
pub mod core;
pub mod fleet;
pub mod fleet_http;
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
mod compat_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod fleet_http_tests;
#[cfg(test)]
mod fleet_tests;
#[cfg(test)]
mod handler_tests;
#[cfg(test)]
mod idempotency_tests;
#[cfg(test)]
mod router_tests;
#[cfg(test)]
mod tests;
