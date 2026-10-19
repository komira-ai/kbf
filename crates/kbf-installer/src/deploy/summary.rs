//! The JSON a deploy prints, and the exit codes (docs/design/kbf-installer.md, section 2).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::CommitId;
use crate::proxy::model::{Failure, Phase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok,
    Failed,
    Usage,
    Refused,
    Busy,
    NeedsOperator,
    /// Not applied: the plan.
    Plan,
}

impl Exit {
    pub fn code(self) -> i32 {
        match self {
            Exit::Ok | Exit::Plan => 0,
            Exit::Failed => 1,
            Exit::Usage => 2,
            Exit::Refused => 3,
            Exit::Busy => 4,
            Exit::NeedsOperator => 5,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Exit::Ok => "ok",
            Exit::Failed => "failed",
            Exit::Usage => "usage",
            Exit::Refused => "refused",
            Exit::Busy => "busy",
            Exit::NeedsOperator => "needs_operator",
            Exit::Plan => "plan",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Previous {
    pub commit: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub name: String,
    pub ok: bool,
    pub ms: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Step {
    pub fn ok(name: &str, ms: u64) -> Step {
        Step {
            name: name.to_owned(),
            ok: true,
            ms,
            reason: String::new(),
        }
    }

    pub fn failed(name: &str, ms: u64, reason: &str) -> Step {
        Step {
            ok: false,
            reason: reason.to_owned(),
            ..Step::ok(name, ms)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeResult {
    pub id: String,
    pub platform: String,
    /// `confirmed`, `reverted`, `refused`, `unsupported`, `timed_out`, `offline` or
    /// `pending` (the deploy stopped before this node finished).
    pub state: String,
    pub running_commit: String,
    pub secs: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub refusal: String,
}

impl NodeResult {
    pub fn new(id: &str, state: &str) -> NodeResult {
        NodeResult {
            id: id.to_owned(),
            platform: String::new(),
            state: state.to_owned(),
            running_commit: String::new(),
            secs: 0,
            refusal: String::new(),
        }
    }
}

/// The frontend update the deploy asked the update API for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Frontend {
    /// The API's id of the update; none when no update was made.
    pub update_id: Option<String>,
    /// The last phase the API reported for it.
    pub phase: Option<Phase>,
    /// What the API said went wrong, when the update ended `failed` or `rolled_back`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// The frontend already ran the commit, so the API made no update.
    #[serde(skip_serializing_if = "is_false")]
    pub already_running: bool,
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

/// The post-deploy checks; `None` is a check that did not run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PostResult {
    pub members_active: Option<bool>,
    pub nodes_on_commit: Option<bool>,
    pub readyz: Option<bool>,
    /// `pass`, `fail` or `skipped`.
    pub cas_smoke: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub schema: u32,
    pub commit: String,
    pub previous_desired: Option<Previous>,
    pub generation: Option<u64>,
    pub result: String,
    pub exit: i32,
    pub reason: String,
    pub started: String,
    pub ended: String,
    pub steps: Vec<Step>,
    pub platforms: BTreeMap<String, String>,
    pub nodes: Vec<NodeResult>,
    pub frontend: Option<Frontend>,
    pub post: PostResult,
    /// The installer's own check was not available, so it did not run.
    pub check_skipped: bool,
    /// What went wrong, for a person; never a host name or a token.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Summary {
    pub fn new(commit: &CommitId, now: u64) -> Summary {
        Summary {
            schema: 2,
            commit: commit.to_string(),
            previous_desired: None,
            generation: None,
            result: Exit::Failed.word().to_owned(),
            exit: Exit::Failed.code(),
            reason: String::new(),
            started: utc(now),
            ended: String::new(),
            steps: Vec::new(),
            platforms: BTreeMap::new(),
            nodes: Vec::new(),
            frontend: None,
            post: PostResult::default(),
            check_skipped: false,
            detail: String::new(),
        }
    }

    pub fn end(&mut self, exit: Exit, reason: &str, detail: &str) {
        self.result = exit.word().to_owned();
        self.exit = exit.code();
        reason.clone_into(&mut self.reason);
        detail.clone_into(&mut self.detail);
    }

    pub fn line(&self) -> String {
        serde_json::to_string(self).expect("a summary serializes")
    }
}

/// `2026-10-18T01:02:03Z` for seconds since the epoch.
pub fn utc(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
