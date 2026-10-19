//! The automatic rollback of a bake that ended red (`docs/design/kbf-cicd.md`, section 8.2):
//! decided here, and off unless the operator turned it on.
//!
//! [`plan`] is a pure decision. The rollback is offered only when all of these hold: it was
//! asked for, the bake ended **red** (a soft gate never ends a bake, so a soft red cannot
//! reach here; extended and green never roll back), the red gate had a **measurement** over
//! its limit (a gate red for want of a series or a baseline is no evidence the release is
//! bad), the hand-off says the release is **reversible**, and the hand-off names the commit
//! it replaced and the generation the deploy set. Otherwise the answer says why nothing is
//! offered, and the run pages. [`offer`] is the call: `POST /v1/update/desired` of the older
//! commit with `expect_generation`, so a desired commit someone else has moved since is a
//! `409` and nothing is written.

use std::time::Duration;

use kbf_gate::{CommitId, Status};

/// What the evaluator knows when a bake ends.
pub struct Inputs<'a> {
    /// `--auto-rollback`.
    pub enabled: bool,
    /// How the bake ended.
    pub status: Status,
    /// Whether the red gate rests on a measurement ([`crate::drive::Outcome::measured`]).
    pub measured: bool,
    /// The hand-off's `reversible`; `None` when there was no hand-off.
    pub reversible: Option<bool>,
    /// The commit the deploy replaced.
    pub previous: Option<&'a CommitId>,
    /// The generation the deploy set.
    pub generation: Option<u64>,
}

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing: not asked for, or the bake did not end red.
    Off,
    /// A red bake that is not rolled back, and why. The run pages.
    Page(String),
    /// Offer `commit` as the desired release, if the desired generation is still `expect`.
    Offer {
        /// The older commit.
        commit: CommitId,
        /// The generation the deploy set.
        expect: u64,
    },
}

/// Decide.
pub fn plan(i: &Inputs<'_>) -> Plan {
    if !i.enabled || i.status != Status::Red {
        return Plan::Off;
    }
    if !i.measured {
        return Plan::Page(
            "the red gate had no measurement (a missing series or baseline), which is no evidence the release is bad: nothing offered"
                .to_owned(),
        );
    }
    match i.reversible {
        Some(true) => {}
        Some(false) => {
            return Plan::Page(
                "the release is not reversible (section 8.3): nothing offered".to_owned(),
            );
        }
        None => {
            return Plan::Page(
                "the release's reversibility is unknown (no hand-off): nothing offered".to_owned(),
            );
        }
    }
    match (i.previous, i.generation) {
        (Some(commit), Some(expect)) => Plan::Offer {
            commit: commit.clone(),
            expect,
        },
        (None, _) => {
            Plan::Page("the hand-off names no previous commit: nothing offered".to_owned())
        }
        (_, None) => Plan::Page(
            "the deploy set no generation to compare against: nothing offered".to_owned(),
        ),
    }
}

/// Offer `commit` at `{api}/v1/update/desired` with the bearer `token`, as a compare-and-set
/// on generation `expect`. The answer is `Ok` only for a success status; a `409`, a refusal
/// or a connection error is an error, and the caller does not retry.
pub fn offer(
    api: &str,
    token: &str,
    commit: &CommitId,
    expect: u64,
    timeout: Duration,
) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let body = serde_json::json!({"commit": commit.as_str(), "expect_generation": expect});
    agent
        .post(&format!("{}/v1/update/desired", api.trim_end_matches('/')))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map(|_| ())
        .map_err(|e| e.to_string())
}
