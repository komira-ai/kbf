//! Step 4: the frontend update, through the update API the proxy serves
//! (`docs/design/frontend-proxy.md`, section 4).
//!
//! `POST /v1/update/frontend {commit, request_id, nodes: true, rollback: false}` starts it, and
//! `GET /v1/update/frontend` is read until the update the POST named is `done` (the step
//! succeeds), `failed` or `rolled_back` (it fails, with the step and the reason the API gave).
//! The deploy does not set the desired release: with `nodes: true` the new frontend does, once
//! it has baked, and its controller rolls the nodes (step 5 watches).
//!
//! - **One request id per run.** Every retry of the POST (a connection that failed, a `429`
//!   or a `5xx`, an answer that was lost) carries the id in [`super::Options::request_id`],
//!   which the API answers with the update it started the first time. A retry therefore
//!   never starts a second update.
//! - **Only our update counts.** The status shows one update at a time, the last one after
//!   it ended. A status of another update (the one before, or one a person started) is not
//!   this deploy's verdict, whatever its phase.
//! - **Only `done` is success.** An earlier phase is a frontend update still going on; the
//!   deploy reads on until the deadline ([`super::Options::handover_timeout`], which covers
//!   the bake).

use serde::{Deserialize, Serialize};

use super::api::{Reply, Request};
use super::summary::{Exit, Frontend};
use super::{Fail, Run, fail};
use crate::proxy::api::model::{StatusBody, UpdateId, UpdateRequest};
use crate::proxy::model::{Failure, Phase};

const UPDATE: &str = "/v1/update/frontend";

/// The `202`: the update, and the phase it starts in.
#[derive(Deserialize)]
struct Accepted {
    update_id: UpdateId,
}

/// The body of a refusal: `{"error": code, "message": text}`.
#[derive(Default, Deserialize)]
struct Refusal {
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
}

/// What the answer to the POST means for the deploy.
enum Posted {
    /// The update to follow.
    Accepted(UpdateId),
    /// The frontend already runs the commit: there is nothing to update.
    Running,
    /// Not an answer yet: ask again, with the same request id.
    Again(String),
    /// The end of the deploy.
    Stop(Fail),
}

fn classify(reply: Result<Reply, String>) -> Posted {
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Posted::Again(e),
    };
    match reply.status {
        200 | 202 => match serde_json::from_slice::<Accepted>(&reply.body) {
            Ok(accepted) => Posted::Accepted(accepted.update_id),
            Err(e) => Posted::Stop(fail(
                Exit::Failed,
                "api_error",
                format!("POST {UPDATE}: the answer names no update: {e}"),
            )),
        },
        429 | 500..=599 => Posted::Again(format!("POST {UPDATE}: status {}", reply.status)),
        status => refused(status, &reply.body),
    }
}

/// An answer that is neither an update nor a reason to ask again.
fn refused(status: u16, body: &[u8]) -> Posted {
    let why: Refusal = serde_json::from_slice(body).unwrap_or_default();
    let said = format!(
        "POST {UPDATE}: status {status} {}: {}",
        why.error, why.message
    );
    match (status, why.error.as_str()) {
        (409, "same_commit") => Posted::Running,
        (409, "incompatible") => Posted::Stop(fail(Exit::Refused, "incompatible", said)),
        (409, "state_lost" | "request_id_reused" | "request_id_retired") => {
            Posted::Stop(fail(Exit::Failed, "api_error", said))
        }
        // `update_open`, `old_child_draining`, `installer_busy`: not now.
        (409, _) => Posted::Stop(fail(Exit::Busy, "frontend_busy", said)),
        (422, _) => Posted::Stop(fail(Exit::Refused, "release_incomplete", said)),
        _ => Posted::Stop(fail(Exit::Failed, "api_error", said)),
    }
}

/// Why the status was not read.
pub(super) enum Unseen {
    /// No usable answer: the call failed, or the status was not 200. Asking again may work.
    Unanswered(String),
    /// A `200` that is not an update status. Asking again will not help.
    Unreadable(String),
}

impl Unseen {
    pub(super) fn into_text(self) -> String {
        match self {
            Unseen::Unanswered(text) | Unseen::Unreadable(text) => text,
        }
    }
}

/// The word the API uses for a phase or a step.
fn word(value: &impl Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
}

/// What a status says about the update `id`.
enum Verdict {
    /// The status is of another update, or of none.
    NotOurs,
    Going(Phase),
    Done,
    Ended(Phase, Option<Failure>),
}

fn verdict(status: &StatusBody, id: &UpdateId) -> Verdict {
    match (&status.update_id, status.phase) {
        (Some(seen), Some(phase)) if seen == id => match phase {
            Phase::Done => Verdict::Done,
            Phase::Failed | Phase::RolledBack => Verdict::Ended(phase, status.failure.clone()),
            _ => Verdict::Going(phase),
        },
        _ => Verdict::NotOurs,
    }
}

/// The failure of an update that ended `failed` or `rolled_back`, with the step and the
/// reason as the API gave them.
fn ended(id: &UpdateId, phase: Phase, failure: Option<&Failure>) -> Fail {
    let reason = if phase == Phase::Failed {
        "frontend_failed"
    } else {
        "frontend_rolled_back"
    };
    let what = match failure {
        Some(f) => format!("at step {}: {}", word(&f.step), f.reason),
        None => "(the API named no step)".to_owned(),
    };
    fail(
        Exit::Failed,
        reason,
        format!("update {} ended {} {what}", id.0, word(&phase)),
    )
}

impl Run<'_> {
    /// `GET /v1/update/frontend`.
    pub(super) fn frontend_status(&self) -> Result<StatusBody, Unseen> {
        let reply = self
            .send(&Request::get(UPDATE))
            .map_err(Unseen::Unanswered)?;
        if reply.status != 200 {
            return Err(Unseen::Unanswered(format!(
                "GET {UPDATE}: status {}",
                reply.status
            )));
        }
        serde_json::from_slice(&reply.body)
            .map_err(|e| Unseen::Unreadable(format!("GET {UPDATE}: not an update status: {e}")))
    }

    /// Whether the step has used its time.
    fn out_of_time(&self, start: u64) -> bool {
        self.env.clock.now().saturating_sub(start) >= self.opts.handover_timeout.as_secs()
    }

    pub(super) fn update_frontend(&mut self) -> Result<(), Fail> {
        let start = self.env.clock.now();
        let request = Request::post_json(
            UPDATE,
            &UpdateRequest {
                commit: self.opts.commit.clone(),
                request_id: self.opts.request_id.clone(),
                nodes: true,
                rollback: false,
            },
        );
        let id = loop {
            match classify(self.send(&request)) {
                Posted::Accepted(id) => break id,
                Posted::Running => {
                    self.sum.frontend = Some(Frontend {
                        already_running: true,
                        ..Frontend::default()
                    });
                    return Ok(());
                }
                Posted::Stop(stop) => return Err(stop),
                Posted::Again(why) => {
                    if self.out_of_time(start) {
                        return Err(fail(
                            Exit::Failed,
                            "api_error",
                            format!("no update was accepted: {why}"),
                        ));
                    }
                }
            }
            self.env.clock.sleep(self.opts.poll);
        };
        self.follow(start, &id)
    }

    /// Read the status until the update `id` ends. A status that cannot be had right now is
    /// asked for again: the children behind the proxy swap during the update.
    fn follow(&mut self, start: u64, id: &UpdateId) -> Result<(), Fail> {
        let mut seen = None;
        let mut last = "the status has not been read".to_owned();
        loop {
            match self.frontend_status() {
                Err(Unseen::Unreadable(text)) => {
                    return Err(fail(Exit::Failed, "api_error", text));
                }
                Err(Unseen::Unanswered(text)) => last = text,
                Ok(status) => match verdict(&status, id) {
                    Verdict::NotOurs => {
                        last = format!("the API shows another update, not {}", id.0);
                    }
                    Verdict::Going(phase) => {
                        seen = Some(phase);
                        self.record(id, phase, None);
                    }
                    Verdict::Done => {
                        self.record(id, Phase::Done, None);
                        return Ok(());
                    }
                    Verdict::Ended(phase, failure) => {
                        self.record(id, phase, failure.clone());
                        return Err(ended(id, phase, failure.as_ref()));
                    }
                },
            }
            if self.out_of_time(start) {
                let state = match seen {
                    Some(phase) => format!("was in phase {} when", word(&phase)),
                    None => format!("was not shown ({last}) when"),
                };
                return Err(fail(
                    Exit::Failed,
                    "frontend_timeout",
                    format!(
                        "update {} {state} the {} s ran out",
                        id.0,
                        self.opts.handover_timeout.as_secs()
                    ),
                ));
            }
            self.env.clock.sleep(self.opts.poll);
        }
    }

    fn record(&mut self, id: &UpdateId, phase: Phase, failure: Option<Failure>) {
        self.sum.frontend = Some(Frontend {
            update_id: Some(id.0.clone()),
            phase: Some(phase),
            failure,
            already_running: false,
        });
    }
}
