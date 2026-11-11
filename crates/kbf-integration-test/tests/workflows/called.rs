//! The reusable workflows of other repositories a job may call ([`CALLABLE`]), and the one
//! `if:` under which such a call may hold the OIDC token ([`DEPLOY_ONLY`]).
//!
//! A job-level `uses:` of another repository runs that repository's file, which this lint never
//! reads, so it is refused unless it is exactly one of [`CALLABLE`]: a repository, path and full
//! commit SHA compared as one string (another commit, a tag, a branch, another path or another
//! owner's copy is refused), written as a plain scalar followed on its line by a version
//! comment, as a step's action is. What the called file does at that commit was read by the
//! reviewer of the change that pinned it; the release workflow lint holds the calling job's
//! inputs and token.

use super::yaml::{Node, Value};

/// The reusable workflows of other repositories a job may call, each `owner/repository/path@sha`.
/// docker-swarm-updater's deploy workflow at v0.1.3: the release's deploy of the frontend to its
/// Docker Swarm service.
pub const CALLABLE: &[&str] = &[
    "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6",
];

/// The one `if:` besides `RELEASE_PUSH_ONLY` under which a job may hold `id-token: write` (and
/// no other scope for writing): the same successful pipeline run for a push to `main` that
/// releases code, while the repository variable `CP_DEPLOY_ENDPOINT` is not empty.
pub const DEPLOY_ONLY: &str = "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' && vars.CP_DEPLOY_ENDPOINT != '' }}";

/// Why the job-level `uses:` node `called` (in the workflow `text`), which does not start with
/// `./`, is refused; `None` when it is a call this lint allows.
pub fn refusal(text: &str, called: &Node) -> Option<String> {
    let Some(v) = called.as_str() else {
        return Some("`uses` must be a string".to_owned());
    };
    if !CALLABLE.contains(&v) {
        return Some(format!(
            "a reusable workflow outside this repository runs where this lint cannot see: `{v}` is not one of {CALLABLE:?}"
        ));
    }
    if !matches!(called.value, Value::Scalar { plain: true, .. }) {
        return Some(format!(
            "`{v}` must be a plain scalar, so its version comment can be read"
        ));
    }
    if super::trailing_comment(text, called).is_none_or(|c| c.trim().is_empty()) {
        return Some(format!("`{v}` needs a version comment"));
    }
    None
}

#[cfg(test)]
#[path = "called_tests.rs"]
mod tests;
