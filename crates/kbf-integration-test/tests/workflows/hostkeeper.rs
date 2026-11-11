//! The one workflow that tells hostkeeper which release of kbf the farm's nodes run:
//! `hostkeeper-desired.yml`.
//!
//! It is the one workflow that
//! - follows the release (`workflow_run` of the workflow `release`, `completed`, `branches:
//!   [main]`; [`workflow_run`](super::workflow_run) holds the trigger's shape),
//! - calls a reusable workflow of another repository, [`SET_DESIRED`], pinned by a full commit SHA
//!   and a version comment, in its job `set-desired`, and
//! - holds `id-token: write` under the exact `if:` of that job ([`SET_IF`], which needs the repository variable
//!   `KBF_SETS_DESIRED` to be `true`); its other job, `check`, holds no token that writes.
//!
//! The lint cannot see a file's path, so the workflow is known by its `name:` ([`NAME`]); the
//! repository test `only_the_hostkeeper_workflow_is_named_so_and_follows_the_release` holds the
//! name to `hostkeeper-desired.yml` and that file to the trigger. Any other workflow that calls a
//! reusable workflow outside this repository, or calls this one from another job or unpinned, is
//! refused by [`job_uses`].

use super::yaml::{Node, Value};
use super::*;

/// The `name:` of the workflow.
pub const NAME: &str = "hostkeeper-desired";

/// The job that calls hostkeeper's workflow, and the only one of it that holds `id-token: write`.
pub const SET_JOB: &str = "set-desired";

/// The reusable workflow `set-desired` calls, up to the `@`.
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The `if:` of `set-desired`, the one spelling under which it holds `id-token: write`: a run of
/// `main`, the switch on, and the `check` job found a release to set.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.KBF_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// Whether the workflow is named [`NAME`].
pub(super) fn is_named(root: &Node) -> bool {
    let Value::Map(top) = &root.value else {
        return false;
    };
    get(top, "name").and_then(Node::as_str) == Some(NAME)
}

/// Whether `v` is [`SET_DESIRED`] at a full commit SHA.
fn is_the_pinned_call(v: &str) -> bool {
    v.strip_prefix(SET_DESIRED)
        .and_then(|rest| rest.strip_prefix('@'))
        .is_some_and(|sha| is_lower_hex(sha, 40))
}

/// Whether `comment` (the text after the `#`) is a version, `v<x>.<y>.<z>`.
fn is_version_comment(comment: &str) -> bool {
    let Some(version) = comment.trim().strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}

impl Lint<'_> {
    /// A job-level `uses:` that is not one of this repository's workflow files: allowed only for
    /// the job [`SET_JOB`] of the workflow [`NAME`], at [`SET_DESIRED`], pinned by a full commit SHA
    /// with a version comment.
    pub(super) fn outside_call(&mut self, key: &Node, called: &Node) {
        let Some(v) = called.as_str() else {
            self.refuse(called, "a reusable workflow's `uses` must be a string");
            return;
        };
        if !self.hostkeeper || key.as_str() != Some(SET_JOB) {
            self.refuse(
                called,
                "a reusable workflow outside this repository runs where this lint cannot see",
            );
        } else if !is_the_pinned_call(v) {
            self.refuse(
                called,
                format_args!(
                    "`{v}` is not `{SET_DESIRED}` pinned to a full commit SHA: the one workflow outside this repository that may be called"
                ),
            );
        } else if !matches!(called.value, Value::Scalar { plain: true, .. })
            || trailing_comment(self.text, called).is_none_or(|c| !is_version_comment(c))
        {
            self.refuse(
                called,
                format_args!(
                    "`{v}` needs a plain scalar and a version comment, `# v<x>.<y>.<z>`, so the release that holds the pin can be read"
                ),
            );
        }
    }

    /// Whether `id-token: write` is allowed in the job `key` with the `if:` `condition`: under the
    /// release's own conditions (the gamma stage, the apt read and write jobs), or in the job
    /// [`SET_JOB`] of the workflow [`NAME`] under [`SET_IF`].
    pub(super) fn deploy_scope_ok(&self, key: &Node, condition: Option<&str>) -> bool {
        let Some(condition) = condition else {
            return false;
        };
        condition == RELEASE_PUSH_ONLY
            || release_ifs::id_token_conditions()
                .iter()
                .any(|c| c == condition)
            || (self.hostkeeper && key.as_str() == Some(SET_JOB) && condition == SET_IF)
    }
}
