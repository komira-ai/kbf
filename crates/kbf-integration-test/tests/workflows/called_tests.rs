//! Tests of the calls of other repositories' reusable workflows and of the deploy job's token.
//! Each says which defect it catches.

use super::super::{Repo, scan};
use super::{CALLABLE, DEPLOY_ONLY};

/// A workflow whose one job is `body`.
fn workflow(body: &str) -> String {
    format!("name: t\non: push\npermissions: {{}}\njobs:\n  a:\n{body}")
}

fn found(body: &str) -> Vec<String> {
    scan(&workflow(body), &Repo::default())
}

/// The deploy job: the call `uses` (the rest of its line included), under `if:` `condition`,
/// with `permissions`.
fn call(uses: &str, condition: &str, permissions: &str) -> String {
    format!("    if: {condition}\n    permissions: {permissions}\n    uses: {uses}\n")
}

fn refused(body: &str, want: &str) {
    let found = found(body);
    assert!(
        found.iter().any(|m| m.contains(want)),
        "want `{want}`:\n{body}\n-> {found:?}"
    );
}

#[test]
fn the_pinned_deploy_call_with_its_token_passes() {
    // Catches: the release's frontend deploy refused by the lint that should let exactly it
    // through (the call at its pinned commit with its version comment, holding `id-token:
    // write` under the deploy condition, with no `runs-on`).
    let body = call(
        &format!("{} # v0.1.3", CALLABLE[0]),
        DEPLOY_ONLY,
        "{id-token: write}",
    );
    assert_eq!(found(&body), Vec::<String>::new(), "{body}");
}

#[test]
fn a_call_of_anything_but_the_pinned_workflow_at_its_commit_is_refused() {
    // Catches: the allowance read as a prefix, an owner, a repository or a path: another
    // commit, a tag, a branch, a short or upper-case SHA, another workflow of the same
    // repository, the same path under another owner, and a suffix after the SHA are each
    // refused as a call outside this repository.
    let pinned = CALLABLE[0];
    let (path, sha) = pinned.split_once('@').expect("pinned");
    let others = [
        format!("{path}@{}", "0".repeat(40)),
        format!("{path}@v0.1.3"),
        format!("{path}@main"),
        format!("{path}@{}", &sha[..12]),
        format!("{path}@{}", sha.to_ascii_uppercase()),
        format!("komira-ai/docker-swarm-updater/.github/workflows/ci.yml@{sha}"),
        format!("someone/docker-swarm-updater/.github/workflows/deploy.yml@{sha}"),
        format!("{pinned}x"),
    ];
    for other in others {
        refused(
            &call(
                &format!("{other} # v0.1.3"),
                DEPLOY_ONLY,
                "{id-token: write}",
            ),
            &format!(
                "reusable workflow outside this repository runs where this lint cannot see: `{other}`"
            ),
        );
    }
}

#[test]
fn the_pinned_call_without_a_readable_version_comment_or_as_no_string_is_refused() {
    // Catches: a pinned call whose version no reader can see (no comment, an empty one, or a
    // quoted scalar, whose comment's place the parser does not keep), and a `uses:` that is
    // not one string.
    let pinned = CALLABLE[0];
    for (uses, want) in [
        (
            pinned.to_owned(),
            format!("`{pinned}` needs a version comment"),
        ),
        (
            format!("{pinned} #"),
            format!("`{pinned}` needs a version comment"),
        ),
        (
            format!("{pinned} #   "),
            format!("`{pinned}` needs a version comment"),
        ),
        (
            format!("\"{pinned}\" # v0.1.3"),
            format!("`{pinned}` must be a plain scalar"),
        ),
        (
            format!("'{pinned}' # v0.1.3"),
            format!("`{pinned}` must be a plain scalar"),
        ),
        (format!("[{pinned}]"), "`uses` must be a string".to_owned()),
    ] {
        refused(&call(&uses, DEPLOY_ONLY, "{id-token: write}"), &want);
    }
}

#[test]
fn the_deploy_condition_allows_the_oidc_token_and_nothing_else() {
    // Catches: the deploy condition taken as a gate for more than the one scope (`contents`,
    // `attestations` or `actions` for writing), and a loosened or altered deploy condition
    // (the endpoint test turned round, another variable, the tailnet tags test dropped or
    // turned round, the release part dropped) still granting the token.
    let uses = format!("{} # v0.1.3", CALLABLE[0]);
    for scope in ["contents", "attestations", "actions"] {
        refused(
            &call(&uses, DEPLOY_ONLY, &format!("{{{scope}: write}}")),
            &format!("`{scope}: write` is allowed only in a job whose `if:` is exactly"),
        );
    }
    let loosened = [
        DEPLOY_ONLY.replace("CP_DEPLOY_ENDPOINT != ''", "CP_DEPLOY_ENDPOINT == ''"),
        DEPLOY_ONLY.replace("CP_DEPLOY_ENDPOINT", "CP_DEPLOY_AUDIENCE"),
        DEPLOY_ONLY.replace(" && vars.CP_DEPLOY_TAILSCALE_TAGS != ''", ""),
        DEPLOY_ONLY.replace(
            "CP_DEPLOY_TAILSCALE_TAGS != ''",
            "CP_DEPLOY_TAILSCALE_TAGS == ''",
        ),
        DEPLOY_ONLY.replace(" && needs.plan.outputs.pipeline != 'true'", ""),
        "${{ vars.CP_DEPLOY_ENDPOINT != '' }}".to_owned(),
    ];
    for condition in loosened {
        refused(
            &call(&uses, &condition, "{id-token: write}"),
            "`id-token: write` is allowed only in a job whose `if:` is exactly",
        );
    }
}
