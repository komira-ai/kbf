//! Tests of `gamma.yml` and of the one shape of `workflow_run` the lint allows. The
//! control is the real file; each plant changes one thing in it and must go red.

use super::*;

const SHA40: &str = "0123456789abcdef0123456789abcdef01234567";

fn real_gamma() -> String {
    std::fs::read_to_string(crate::repo_root().join(".github/workflows/gamma.yml"))
        .expect("gamma.yml")
}

fn repo() -> Repo {
    Repo {
        action_dirs: Vec::new(),
        workflows: Vec::new(),
    }
}

fn found(text: &str) -> Vec<String> {
    scan(text, &repo())
}

/// The real file with `old` replaced by `new`, which must be there.
fn plant(old: &str, new: &str) -> String {
    let text = real_gamma();
    assert!(text.contains(old), "gamma.yml has no `{old}`");
    text.replacen(old, new, 1)
}

fn refused(text: &str, want: &str) {
    let out = found(text);
    assert!(
        out.iter().any(|m| m.contains(want)),
        "want `{want}`: {out:?}"
    );
}

/// The control: the real file passes the whole workflow lint.
#[test]
fn gamma_yml_passes_the_workflow_lint() {
    assert_eq!(found(&real_gamma()), Vec::<String>::new());
}

/// Catches: a `workflow_run` of some other workflow (one that runs pull request code), of
/// other types, or on every branch.
#[test]
fn workflow_run_is_allowed_only_for_a_completed_artifacts_run_on_named_branches() {
    let other = "other than `[artifacts]`";
    refused(
        &plant("workflows: [artifacts]", "workflows: [artifacts, ci]"),
        other,
    );
    refused(&plant("workflows: [artifacts]", "workflows: [ci]"), other);
    let types = "other than `[completed]`";
    refused(&plant("types: [completed]", "types: [requested]"), types);
    refused(
        &plant("types: [completed]", "types: [completed, requested]"),
        types,
    );
    let branches = "without a `branches:` list";
    refused(
        &plant("    branches: [main, feature/arch-v2]\n", ""),
        branches,
    );
    refused(
        &plant(
            "    branches: [main, feature/arch-v2]\n",
            "    branches: []\n",
        ),
        branches,
    );
    refused(
        "on: workflow_run\npermissions: {}\njobs: {}\n",
        "without its `workflows`",
    );
}

/// Catches: a job that runs for a failed run, or for a pull request or merge-queue run
/// (which attest nothing and must never deploy).
#[test]
fn every_job_follows_a_successful_push_run() {
    let want = "every job's `if:`";
    refused(
        &plant("github.event.workflow_run.conclusion == 'success' && ", ""),
        want,
    );
    refused(
        &plant("github.event.workflow_run.event == 'push' && ", ""),
        want,
    );
    refused(
        &plant(
            "    if: ${{ github.event.workflow_run.conclusion",
            "    xif: ${{ github.event.workflow_run.conclusion",
        ),
        want,
    );
}

/// Catches: a checkout of the triggering ref or of another repository (code run with the
/// secrets), and the event's text interpolated into a script. The control is the real
/// file, which checks out the default branch.
#[test]
fn a_checkout_names_no_ref_and_no_event_field_is_in_a_script() {
    for pick in [
        "ref: ${{ github.event.workflow_run.head_sha }}",
        "repository: evil/fork",
    ] {
        let text = plant(
            "          persist-credentials: false\n",
            &format!("          persist-credentials: false\n          {pick}\n"),
        );
        refused(&text, "may name no `ref` or `repository`");
    }
    refused(
        &plant(
            "cargo build -p xtask --locked",
            "echo ${{ github.event.workflow_run.head_sha }}",
        ),
        "only through `env:`",
    );
}

/// Catches: the token scopes widening or a signing scope in a job that does not sign.
#[test]
fn the_job_holds_read_scopes_only() {
    refused(
        &plant("      attestations: read\n", "      attestations: write\n"),
        "write",
    );
    refused(
        &plant("      actions: read\n", "      id-token: write\n"),
        "id-token",
    );
    refused(
        &plant("      contents: read\n", "      contents: write\n"),
        "contents",
    );
}

/// Proves the job is inert until configured: its `if:` needs every repository variable to
/// be non-empty, so with none set the run is skipped (green). Catches an `if:` that drops
/// one of them, which would let a half-configured repository start a deploy that fails.
#[test]
fn the_job_runs_only_when_every_variable_is_set() {
    let condition = job_condition(&real_gamma(), "deploy")
        .unwrap()
        .expect("an if:");
    for var in ["GAMMA_HOST", "GAMMA_USER", "TS_TAG"] {
        let need = format!(" && vars.{var} != ''");
        assert!(condition.contains(&need), "{condition}");
        assert!(plant(&condition, &condition.replace(&need, "")).len() < real_gamma().len());
    }
    // The gate is the job's own `if:`: no other job exists to be skipped separately.
    assert_eq!(real_gamma().matches("\n  deploy:").count(), 1);
}

/// Proves the OAuth secrets reach one step, which runs after `cargo xtask` is built (so
/// no build script runs beside them), that the environment and the queue are as designed
/// and that nothing triggers on a pull request.
#[test]
fn the_oauth_secrets_reach_one_step_after_the_build() {
    let text = real_gamma();
    assert_eq!(text.matches("secrets.").count(), 2);
    let steps = job_steps(&text, "deploy").expect("steps");
    let at = |pred: &dyn Fn(&Step) -> bool| steps.iter().position(pred).expect("step");
    let build = at(&|s| s.run.as_deref() == Some("cargo build -p xtask --locked"));
    let tailscale = at(&|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("tailscale/github-action@"))
    });
    assert!(build < tailscale);
    let secret_steps = steps
        .iter()
        .filter(|s| s.with.iter().any(|(_, v)| v.contains("secrets.")))
        .count();
    assert_eq!(secret_steps, 1);
    // The table runs even when the deploy failed.
    let table = at(&|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.contains("gamma summary"))
    });
    assert!(steps[table].conditional);
    assert!(table > at(&|s| s.run.as_deref().is_some_and(|r| r.contains("gamma deploy"))));
    assert!(text.contains("environment: gamma"));
    assert!(text.contains("group: gamma") && text.contains("cancel-in-progress: false"));
    assert!(!text.contains("pull_request"));
    assert!(!text.contains(SHA40));
}

/// Catches: the stage file and the workflow drifting (a job with no row, or the stage no
/// longer allowing the trigger).
#[test]
fn the_stage_file_lists_gamma_as_an_actions_stage_with_its_job() {
    let root = crate::repo_root();
    let stages = std::fs::read_to_string(root.join(super::stages::STAGE_FILE)).expect("stages");
    let dir = root.join(super::stages::WORKFLOW_DIR);
    let mut workflows = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .into_string()
            .expect("name");
        if name.ends_with(".yml") {
            let text = std::fs::read_to_string(dir.join(&name)).expect("text");
            workflows.push((name, text));
        }
    }
    assert_eq!(
        super::stages::check(&stages, &workflows),
        Vec::<String>::new()
    );
    // A pull_request trigger added to gamma.yml is allowed by no stage of its jobs.
    let mut planted = workflows.clone();
    let g = planted
        .iter_mut()
        .find(|(n, _)| n == "gamma.yml")
        .expect("gamma.yml");
    g.1 = g.1.replacen("on:\n", "on:\n  pull_request:\n", 1);
    let out = super::stages::check(&stages, &planted);
    assert!(
        out.iter()
            .any(|m| m.contains("`pull_request` is allowed by no stage")),
        "{out:?}"
    );
    // A job of gamma.yml that no row lists.
    let mut planted = workflows;
    let g = planted
        .iter_mut()
        .find(|(n, _)| n == "gamma.yml")
        .expect("gamma.yml");
    g.1.push_str(
        "  extra:\n    if: ${{ false }}\n    runs-on: ubuntu-24.04\n    steps:\n      - run: 'true'\n",
    );
    let out = super::stages::check(&stages, &planted);
    assert!(
        out.iter().any(|m| m.contains("job `extra` is in no stage")),
        "{out:?}"
    );
}
