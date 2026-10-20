//! Tests of `gamma.yml` and of the one shape of `workflow_run` the lint allows
//! ([`workflow_run`]). The control is the real file; each plant changes one thing in it and
//! must go red.

use super::*;

const SHA40: &str = "0123456789abcdef0123456789abcdef01234567";

/// The lines of `gamma.yml` the plants work on.
const BUILD: &str = "      - run: cargo build -p xtask --locked\n";
const TOOLCHAIN: &str = "      - run: rustup toolchain install\n";
const TAILSCALE: &str =
    "      - uses: tailscale/github-action@a392da0a182bba0e9613b6243ebd69529b1878aa # v4.1.0\n";
const PINNED: &str = "          persist-credentials: false\n";
const OFFER_READ: &str = "      attestations: read\n      contents: read\n";
const DEPLOY_READ: &str = "    permissions:\n      contents: read\n    env:\n";

/// The text of the repository's file `rel`.
fn file(rel: &str) -> String {
    let path = crate::repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn real_gamma() -> String {
    file(".github/workflows/gamma.yml")
}

fn found(text: &str) -> Vec<String> {
    scan(text, &Repo::default())
}

/// The real file with `old` replaced by `new` (the first one), which must be there.
fn plant(old: &str, new: &str) -> String {
    let text = real_gamma();
    assert!(text.contains(old), "gamma.yml has no `{old}`");
    text.replacen(old, new, 1)
}

/// The real file with every `old` replaced by `new`; `old` must be there.
fn plant_all(old: &str, new: &str) -> String {
    let text = real_gamma();
    assert!(text.contains(old), "gamma.yml has no `{old}`");
    text.replace(old, new)
}

fn refused(text: &str, want: &str) {
    let out = found(text);
    assert!(out.iter().any(|m| m.contains(want)), "{want}: {out:?}");
}

/// The control: the real file passes the whole workflow lint, with no message from the
/// `workflow_run` rules either.
#[test]
fn gamma_yml_passes_the_workflow_lint() {
    assert_eq!(found(&real_gamma()), Vec::<String>::new());
}

/// Catches: the chain disabled by a rename on either side. `workflows: [release]` matches by
/// name, so a renamed release workflow silently stops the offer; the name and the publish job
/// the xtask verbs look for are checked against release.yml itself.
#[test]
fn the_names_gamma_follows_are_the_names_release_yml_has() {
    let release = file(".github/workflows/release.yml");
    let name = xtask::gamma::RELEASE_WORKFLOW;
    assert_eq!(workflow_name(&release), Ok(Some(name.to_owned())));
    let filters = workflow_run_filters(&real_gamma()).expect("filters");
    assert!(filters.contains(&("workflows".to_owned(), vec![name.to_owned()])));
    let jobs = job_names(&release).expect("jobs");
    let publish = xtask::gamma::PUBLISH_JOB.to_owned();
    assert!(jobs.contains(&publish), "{jobs:?}");
    let path = crate::repo_root().join(xtask::gamma::RELEASE_PATH);
    assert!(path.is_file(), "{}", path.display());
}

/// Catches: a `workflow_run` in a workflow that is not `gamma`, which would run with the
/// repository's token and an environment's credentials on any run that completes.
#[test]
fn workflow_run_is_allowed_only_for_the_workflow_named_gamma() {
    let only = "only the workflow named `gamma`";
    refused(&plant("name: gamma\n", "name: deploy\n"), only);
    refused(&plant("name: gamma\n", ""), only);
    refused(&plant("name: gamma\n", "name: [gamma]\n"), only);
}

/// Catches: a trigger that is not one `completed` run of `release` on `main`: another
/// workflow (one that runs pull request code), another type, another branch, or a second
/// trigger (a dispatch by a writer, a schedule, a pull request, a push).
#[test]
fn the_trigger_is_a_completed_release_run_on_main_alone() {
    let wrong = [
        ("workflows: [release]", "workflows: [release, ci]"),
        ("workflows: [release]", "workflows: [artifacts]"),
        ("workflows: [release]", "workflows: [Release]"),
        ("types: [completed]", "types: [completed, requested]"),
        ("types: [completed]", "types: [requested]"),
        ("branches: [main]", "branches: [main, feature/arch-v2]"),
        ("branches: [main]", "branches: []"),
        ("branches: [main]\n", ""),
    ];
    for (old, new) in wrong {
        refused(&plant(old, new), "the workflow_run filters");
    }
    let more = [
        "workflow_dispatch:",
        "schedule:\n    - cron: '0 0 * * *'",
        "pull_request:",
        "push:",
    ];
    for extra in more {
        let text = plant("on:\n", &format!("on:\n  {extra}\n"));
        refused(&text, "the triggers");
    }
}

/// Catches: a job that runs for a run that failed, one that is not a push, one that is not
/// on main or is a fork's, and a deploy that runs without the staged offer or with a
/// repository variable unset. Each piece is dropped from the first job's `if:`, then from
/// every job's.
#[test]
fn every_job_follows_a_successful_push_to_main_of_this_repository() {
    let offer = "the `if:` of `offer`";
    let deploy = "the `if:` of `deploy`";
    let shared = [
        "github.event.workflow_run.conclusion == 'success' && ",
        "github.event.workflow_run.event == 'push' && ",
        "github.event.workflow_run.head_branch == 'main' && ",
        " && github.event.workflow_run.head_repository.full_name == github.repository",
    ];
    for piece in shared {
        refused(&plant(piece, ""), offer);
        let all = plant_all(piece, "");
        refused(&all, offer);
        refused(&all, deploy);
    }
    let only_deploy = [
        " && needs.offer.outputs.staged == 'true'",
        " && vars.GAMMA_HOST != ''",
        " && vars.GAMMA_USER != ''",
        " && vars.TS_TAG != ''",
    ];
    for piece in only_deploy {
        refused(&plant(piece, ""), deploy);
    }
    let bypass = plant("${{ github.event", "${{ true || github.event");
    refused(&bypass, offer);
}

/// Proves the permissions are exact: `contents: write` on the offer job, `id-token` anywhere,
/// a scope dropped from a job and one added to it.
#[test]
fn the_permissions_are_exact() {
    let offer = "the permissions of `offer`";
    let write = "      attestations: read\n      contents: write\n";
    refused(&plant(OFFER_READ, write), "contents");
    refused(&plant(OFFER_READ, write), offer);
    let id_token = "    permissions:\n      contents: read\n      id-token: write\n    env:\n";
    refused(&plant(DEPLOY_READ, id_token), "id-token");
    let more = "    permissions:\n      actions: read\n      contents: read\n    env:\n";
    refused(&plant(DEPLOY_READ, more), "the permissions of `deploy`");
    let dropped = plant("      attestations: read\n", "");
    refused(&dropped, offer);
    let comment = plant("# Gamma:", "# id-token\n# Gamma:");
    refused(&comment, "`id-token` appears");
    let wide = plant("permissions: {}\n", "permissions: write-all\n");
    refused(&wide, "permissions");
}

/// Catches: a credential in a job that must not hold one (the offer job, which runs the
/// release's own text through verification), in a job-level `env:`, in a script, or reaching
/// the Tailscale step before `cargo xtask` is built (a build script beside the credential);
/// and the Tailscale identity joined by the offer job.
#[test]
fn the_oauth_client_reaches_one_step_after_the_build() {
    let text = real_gamma();
    assert_eq!(text.matches("secrets.").count(), 2);
    let steps = job_steps(&text, "deploy").expect("steps");
    let at = |pred: &dyn Fn(&Step) -> bool| steps.iter().position(pred).expect("step");
    let build = at(&|s| s.run.as_deref() == Some("cargo build -p xtask --locked"));
    let tailscale = at(&|s| s.uses.as_deref().is_some_and(|u| u.contains("tailscale/")));
    assert!(build < tailscale);
    let holds = |s: &&Step| s.with.iter().any(|(_, v)| v.contains("secrets."));
    assert_eq!(steps.iter().filter(holds).count(), 1);
    let run_id = "      RUN_ID: ${{ github.event.workflow_run.id }}\n";
    let leak = "      LEAK: ${{ secrets.TS_OAUTH_SECRET }}\n";
    let leaked = plant(run_id, &format!("{run_id}{leak}"));
    refused(&leaked, "`secrets.` appears outside");
    let token = "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}";
    let in_env = plant("GH_TOKEN: ${{ github.token }}", token);
    refused(&in_env, "holds a credential");
    let before = "must build cargo xtask before the Tailscale step";
    refused(&plant_all(BUILD, ""), before);
    let without = plant_all(BUILD, "");
    let last = "          use-cache: true\n";
    let after = without.replacen(last, &format!("{last}{BUILD}"), 1);
    refused(&after, before);
    let with = "        with:\n          tags: tag:x\n";
    let joined = plant(TOOLCHAIN, &format!("{TOOLCHAIN}{TAILSCALE}{with}"));
    refused(&joined, "`offer` joins the tailnet");
    let script = "run: echo ${{ secrets.X }} && cargo xtask gamma evidence";
    let in_script = plant("run: cargo xtask gamma evidence", script);
    refused(&in_script, "holds a credential");
}

/// Catches: a checkout of the triggering ref or of another repository (code run with the
/// credentials), and the event's text interpolated into a script.
#[test]
fn a_checkout_names_no_ref_and_no_event_field_is_in_a_script() {
    let picks = [
        "ref: ${{ github.event.workflow_run.head_sha }}",
        "repository: evil/fork",
    ];
    for pick in picks {
        let text = plant(PINNED, &format!("{PINNED}          {pick}\n"));
        refused(&text, "checks out a ref or a repository");
    }
    let echo = "echo ${{ github.event.workflow_run.head_sha }}";
    let text = plant("cargo build -p xtask --locked", echo);
    refused(&text, "puts an event field in a script");
}

/// Catches: an upload that is not one of the two pinned (the offer's tar, the redacted
/// summary), and above all the installer's own summary, which names nodes, uploaded.
#[test]
fn only_the_pinned_artifacts_are_uploaded_and_never_the_raw_summary() {
    let raw = "path: ${{ runner.temp }}/raw.json";
    refused(&plant("path: summary.json", raw), "uploads");
    refused(&plant("path: summary.json", "path: ."), "uploads");
    refused(&plant("name: gamma-summary", "name: other"), "uploads");
    refused(&plant("name: gamma-offer", "name: other"), "uploads");
    let old = "path: ${{ runner.temp }}/offer\n          retention";
    refused(&plant(old, "path: dist\n          retention"), "uploads");
}

/// Catches: a deploy that does not wait for the offer, runs outside the `gamma` environment
/// (whose branch policy and credentials it relies on), or an offer job in an environment.
#[test]
fn deploy_needs_the_offer_and_runs_in_the_gamma_environment() {
    let needs = "the needs of `deploy`";
    refused(&plant("    needs: [offer]\n", ""), needs);
    refused(&plant("    needs: [offer]\n", "    needs: []\n"), needs);
    let environment = "the environment of `deploy`";
    let prod = plant("    environment: gamma\n", "    environment: prod\n");
    refused(&plant("    environment: gamma\n", ""), environment);
    refused(&prod, environment);
    let on_offer = plant("  offer:\n", "  offer:\n    environment: gamma\n");
    refused(&on_offer, "the environment of `offer`");
}

/// Proves the offer job's output is the staged flag alone.
#[test]
fn the_offer_job_outputs_only_the_staged_flag() {
    let output = "    outputs:\n      staged: ${{ steps.stage.outputs.staged }}\n";
    refused(&plant(output, ""), "the outputs of `offer`");
    let always = "    outputs:\n      staged: ${{ true }}\n";
    refused(&plant(output, always), "the outputs of `offer`");
}

/// The names of the variables of a step's own `env:`.
fn env_names(step: &Step) -> Vec<String> {
    step.env.iter().map(|(k, _)| k.clone()).collect()
}

/// Proves what the steps promise: the table and the summary artifact are made even when the
/// deploy failed, the host and the user are in the environment of the deploy step alone (a
/// variable is printed in the log of a step that has it), and the artifact paths are those
/// the verb is told.
#[test]
fn the_table_and_the_summary_are_made_after_a_failed_deploy() {
    let text = real_gamma();
    let steps = job_steps(&text, "deploy").expect("steps");
    let ran = |word: &str| {
        let has = |s: &Step| s.run.as_deref().is_some_and(|r| r.contains(word));
        let at = steps.iter().position(has);
        at.unwrap_or_else(|| panic!("no step runs `{word}`"))
    };
    let (deploy, table) = (ran("gamma deploy"), ran("gamma summary"));
    let always = Some("${{ always() }}");
    let table_ok = steps[table].condition.as_deref() == always;
    assert!(table > deploy && table_ok);
    assert!(steps[steps.len() - 1].condition.as_deref() == always);
    let want = ["GH_TOKEN", "GAMMA_HOST", "GAMMA_USER"];
    assert_eq!(env_names(&steps[deploy]), want);
    let sha = (
        "SHA".to_owned(),
        "${{ github.event.workflow_run.head_sha }}".to_owned(),
    );
    assert_eq!(job_env(&text, "deploy").expect("env"), [sha]);
    let run = steps[deploy].run.as_deref().expect("run");
    let summary = " --summary summary.json";
    let raw = " --raw \"$RUNNER_TEMP/raw.json\"";
    let bundle = " --bundle \"$RUNNER_TEMP/offer/bundle.tar\"";
    for part in [summary, raw, bundle] {
        assert!(run.contains(part), "{run}");
    }
    let table_run = steps[table].run.as_deref().expect("run");
    assert!(table_run.contains(" --file summary.json"), "{table_run}");
}

/// Proves the file is arranged as designed: one deploy at a time and none cancelled
/// mid-roll, nothing on a pull request, and no commit named in it.
#[test]
fn the_deploys_queue_and_nothing_names_a_commit_or_a_pull_request() {
    let text = real_gamma();
    assert!(text.contains("group: gamma\n  cancel-in-progress: false"));
    assert!(!text.contains("pull_request"));
    assert!(!text.contains(SHA40));
}

/// Every workflow file of the repository, `(file name, text)`.
fn workflow_files() -> Vec<(String, String)> {
    let dir = crate::repo_root().join(super::stages::WORKFLOW_DIR);
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("workflow directory") {
        let entry = entry.expect("entry");
        let name = entry.file_name().into_string().expect("name");
        if name.ends_with(".yml") {
            let text = std::fs::read_to_string(dir.join(&name)).expect("text");
            files.push((name, text));
        }
    }
    files.sort();
    files
}

/// Whether the workflow triggers on `workflow_run`.
fn triggers_on_workflow_run(text: &str) -> bool {
    let triggers = events(text).expect("events");
    triggers.iter().any(|t| workflow_run::is_workflow_run(t))
}

/// Catches: another workflow triggering on `workflow_run`, or naming itself `gamma` to pass
/// for it. The scan must have read the real files, `gamma.yml` among them.
#[test]
fn only_gamma_yml_triggers_on_workflow_run() {
    let files = workflow_files();
    assert!(files.iter().any(|(n, _)| n == "gamma.yml"));
    assert!(files.len() >= 5, "{} workflows read", files.len());
    for (name, text) in &files {
        let follows = triggers_on_workflow_run(text);
        assert_eq!(follows, name == "gamma.yml", "{name}");
        let called = workflow_name(text).expect("name") == Some("gamma".to_owned());
        assert_eq!(called, name == "gamma.yml", "{name}");
    }
    let release = file(".github/workflows/release.yml");
    let planted = release.replacen("on:\n", "on:\n  workflow_run:\n    workflows: [ci]\n", 1);
    refused(&planted, "only the workflow named `gamma`");
}

/// Catches: the stage file and the workflow drifting (a job with no row, or the stage no
/// longer allowing the trigger).
#[test]
fn the_stage_file_lists_gamma_as_an_actions_stage_with_its_jobs() {
    let root = crate::repo_root();
    let stages = std::fs::read_to_string(root.join(super::stages::STAGE_FILE)).expect("stages");
    let workflows = workflow_files();
    assert_eq!(super::stages::check(&stages, &workflows), Vec::<String>::new());
    assert!(stages.contains("gamma.yml#offer"));
    assert!(stages.contains("gamma.yml#deploy"));
    let checked = |change: &dyn Fn(&str) -> String| {
        let mut planted = workflows.clone();
        for (name, text) in &mut planted {
            if name.as_str() == "gamma.yml" {
                *text = change(text);
            }
        }
        super::stages::check(&stages, &planted)
    };
    // A pull_request trigger added to gamma.yml is allowed by no stage of its jobs.
    let out = checked(&|t| t.replacen("on:\n", "on:\n  pull_request:\n", 1));
    let no_stage = "`pull_request` is allowed by no stage";
    assert!(out.iter().any(|m| m.contains(no_stage)), "{out:?}");
    // A job of gamma.yml that no row lists.
    let extra = "  extra:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: 'true'\n";
    let out = checked(&|t| format!("{t}{extra}"));
    let unlisted = "job `extra` is in no stage";
    assert!(out.iter().any(|m| m.contains(unlisted)), "{out:?}");
}
