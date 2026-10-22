//! `POST /v1/finalize` and `GET /v1/finalize`: the operator's one irreversible step of an
//! upgrade, raising every storage node's metadata log to the version the new build runs
//! (`kbf cluster finalize`; `docs/api.md`).
//!
//! Both pass the gates of every `/v1` write ([`super::gate`]), a `GET` included: the report
//! names the build and the log of each node.
//!
//! - `POST /v1/finalize` with `{"target": N}` (or no body, or `{}`) asks every node to raise
//!   its log to machine version `N`; `N` is at most [`RUNS`], the newest this frontend's build
//!   runs, and defaults to it (`400` otherwise). It reads every member of the map and every
//!   live row ([`Members::finalize_report`]); if any member's state blocks (it reports no
//!   build or log version, its build runs less than `N`, or it has no live row because it is
//!   down), the answer is `409` naming those members and why, and nothing is written. Else
//!   the farm's target is raised to `N` ([`Members::raise_finalize`]; the nodes learn it from
//!   their next heartbeat) and the answer is `200` with `{"target": N, "written": bool}`:
//!   `written` is false when the target was at `N` or higher already.
//! - `GET /v1/finalize` answers `{"target": N | null, "nodes": [...]}`: the farm's target
//!   and, for every member, what it reports and whether it is `done`, `pending`, `blocked`
//!   or `unknown` against it. With no target set the nodes are judged against [`RUNS`], what a
//!   finalize without a target would ask.
//!
//! There is no override for a member that cannot answer: an operator removes a member that
//! is down from the map before a finalize.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{ApiState, error, gate, json, member_failure, no_members};
use crate::members::{FinalizeNode, Members, MembershipError};
use crate::proxy_auth::Caller;
use crate::raft_log::RUNS;

/// The body of `POST /v1/finalize`: the log version to raise to; none is [`RUNS`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ask {
    target: Option<u32>,
}

/// The answer of a `POST /v1/finalize` that passed its checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finalized {
    /// The log version asked for.
    pub target: u32,
    /// Whether this call raised the farm's target; false when it was at `target` or higher.
    pub written: bool,
}

/// The answer of `GET /v1/finalize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeView {
    /// The farm's target; `None` before anyone asked.
    pub target: Option<u32>,
    /// Every member against the target (or [`RUNS`] when there is none), in member order.
    pub nodes: Vec<FinalizeNode>,
}

/// What a finalize to a target came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// These members keep it from being asked; nothing was written.
    Blocked(Vec<FinalizeNode>),
    /// The farm's target is at the one asked for or higher; whether this call wrote it.
    Set(bool),
}

/// The log version a body asks for, within 1 to `runs`.
///
/// # Errors
/// The body is not `{"target": N}`, or `N` is zero or past `runs`.
fn target_of(body: &[u8], runs: u32) -> Result<u32, String> {
    let ask = if body.is_empty() {
        Ask::default()
    } else {
        serde_json::from_slice::<Ask>(body)
            .map_err(|e| format!("a finalize body is {{\"target\": N}}, or none: {e}"))?
    };
    let target = ask.target.unwrap_or(runs);
    if !(1..=runs).contains(&target) {
        let why = format!("target {target} is not a version this build runs (1 to {runs})");
        return Err(why);
    }
    Ok(target)
}

/// Raises the farm's target to `target` unless a member blocks it.
fn ask(members: &Members, target: u32) -> Result<Outcome, MembershipError> {
    let blocked: Vec<FinalizeNode> = members
        .finalize_report(target)?
        .into_iter()
        .filter(|node| node.state.blocks())
        .collect();
    if blocked.is_empty() {
        return Ok(Outcome::Set(members.raise_finalize(target)?));
    }
    Ok(Outcome::Blocked(blocked))
}

/// The farm's target and every member against it.
fn view(members: &Members) -> Result<FinalizeView, MembershipError> {
    let target = members.finalize_target()?;
    let nodes = members.finalize_report(target.unwrap_or(RUNS.get()))?;
    Ok(FinalizeView { target, nodes })
}

/// The answer to a finalize to `target` that came to `outcome`.
fn outcome_answer(target: u32, outcome: Outcome) -> Response {
    match outcome {
        Outcome::Set(written) => {
            if written {
                tracing::info!(finalize_to = target, "finalize target raised");
            }
            json(StatusCode::OK, &Finalized { target, written })
        }
        Outcome::Blocked(blocked) => {
            let why = format!(
                "a finalize to {target} is blocked by {} member(s)",
                blocked.len()
            );
            let body = serde_json::json!({ "error": why, "target": target, "blocked": blocked });
            json(StatusCode::CONFLICT, &body)
        }
    }
}

/// The answer to a blocking membership call: `ok` over what it returned, or the failure.
fn answered<T>(
    done: Result<Result<T, MembershipError>, tokio::task::JoinError>,
    ok: impl FnOnce(T) -> Response,
) -> Response {
    match done {
        Ok(Ok(value)) => ok(value),
        Ok(Err(e)) => error(member_failure(&e), &e.to_string()),
        Err(e) => {
            let why = format!("membership task: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, &why)
        }
    }
}

/// `POST /v1/finalize`: the gates, the body, the checks, then the write.
pub(super) async fn start(
    State(api): State<Arc<ApiState>>,
    peer: Caller,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), &peer, &headers) {
        return error(code, &why);
    }
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    let target = match target_of(&body, RUNS.get()) {
        Ok(target) => target,
        Err(why) => return error(StatusCode::BAD_REQUEST, &why),
    };
    // The store's calls block (its trait says so).
    let done = tokio::task::spawn_blocking(move || ask(&members, target)).await;
    answered(done, |outcome| outcome_answer(target, outcome))
}

/// `GET /v1/finalize`: the gates, then the report.
pub(super) async fn status(
    State(api): State<Arc<ApiState>>,
    peer: Caller,
    headers: HeaderMap,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), &peer, &headers) {
        return error(code, &why);
    }
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    // The store's calls block (its trait says so).
    let done = tokio::task::spawn_blocking(move || view(&members)).await;
    answered(done, |view| json(StatusCode::OK, &view))
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
