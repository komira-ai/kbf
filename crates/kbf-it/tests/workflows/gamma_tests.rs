//! Tests of `gamma.yml` and of the one shape of `workflow_run` the lint allows
//! ([`workflow_run`]). The control is the real file; each plant changes one thing in it and
//! must go red.

use super::*;

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
    // The API reports a job by its display name, which is its id only while the job has no
    // `name:`, is not a matrix and does not call a reusable workflow: any of the three would
    // make `gamma evidence` fail (it refuses a run with no job of that name).
    for key in ["name", "strategy", "uses"] {
        let has = job_has_key(&release, xtask::gamma::PUBLISH_JOB, key);
        assert_eq!(has, Ok(false), "the publish job has `{key}`");
    }
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
    refused(&leaked, "`secrets` appears outside");
    // Every other way to reach the repository's secrets has the word too.
    for other in ["secrets['TS_OAUTH_SECRET']", "toJSON(secrets)"] {
        let leak = format!("      LEAK: ${{{{ {other} }}}}\n");
        let leaked = plant(run_id, &format!("{run_id}{leak}"));
        refused(&leaked, "`secrets` appears outside");
    }
    let inherit = plant("  offer:\n", "  offer:\n    secrets: inherit\n");
    refused(&inherit, "`secrets` appears outside");
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

/// Catches: an upload without `overwrite: true`. `upload-artifact` refuses a name the run
/// already has, so "Re-run all jobs" would fail in `offer`, and "Re-run failed jobs" after a
/// failed deploy would end red in the summary upload even when the redeploy succeeded.
#[test]
fn every_upload_overwrites_so_a_rerun_does_not_fail_on_the_name() {
    let without = "uploads without `overwrite: true`";
    let offer = "          if-no-files-found: error\n          overwrite: true\n";
    let planted = plant(offer, "          if-no-files-found: error\n");
    refused(&planted, without);
    let summary = "          retention-days: 30\n          overwrite: true\n";
    let planted = plant(summary, "          retention-days: 30\n");
    refused(&planted, without);
    let off = "          overwrite: false\n";
    let planted = plant_all("          overwrite: true\n", off);
    refused(&planted, without);
}

/// Catches: the host or the user in a step that does not run the deploy, in a job's `env:`,
/// or in a `with:` (GitHub prints a variable in the log of every step whose environment has
/// it, and these logs are public).
#[test]
fn the_host_and_the_user_are_named_by_the_deploy_step_alone() {
    let named = "names the host or the user";
    let table = "        if: ${{ always() }}\n        run: cargo xtask gamma summary";
    let env = "        if: ${{ always() }}\n        env:\n          GAMMA_HOST: ${{ vars.GAMMA_HOST }}\n        run: cargo xtask gamma summary";
    refused(&plant(table, env), named);
    let last = "          use-cache: true\n";
    let with = format!("{last}          note: ${{{{ vars.GAMMA_USER }}}}\n");
    refused(&plant(last, &with), named);
    let script = "run: echo $GAMMA_HOST && cargo xtask gamma evidence";
    refused(&plant("run: cargo xtask gamma evidence", script), named);
    let run_id = "      RUN_ID: ${{ github.event.workflow_run.id }}\n";
    let with_host = format!("{run_id}      GAMMA_HOST: ${{{{ vars.GAMMA_HOST }}}}\n");
    let planted = plant(run_id, &with_host);
    refused(&planted, "the `env` of `offer` has the host");
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
fn the_table_and_the_summary_upload_run_whatever_the_deploy_did() {
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

/// Proves one deploy at a time with none cancelled mid-roll, as the `deploy` job's own
/// group, and catches the group moved to the workflow or put on `offer`. GitHub cancels a
/// pending run when a newer one queues; at workflow level a docs-only push (whose `deploy` is
/// skipped) would cancel a pending release and nothing would go red.
#[test]
fn the_deploys_queue_in_the_deploy_job_and_not_in_the_workflow() {
    let text = real_gamma();
    let group = "    concurrency:\n      group: gamma\n      cancel-in-progress: false\n";
    assert!(text.contains(group));
    let top = "concurrency:\n  group: gamma\n  cancel-in-progress: false\n\nenv:\n";
    let new = format!("{top}  CARGO_TERM_COLOR");
    let moved = plant("env:\n  CARGO_TERM_COLOR", &new);
    refused(&moved, "a workflow-level `concurrency`");
    let timeout = "    timeout-minutes: 30\n";
    let planted = plant(timeout, &format!("{timeout}    concurrency: g\n"));
    refused(&planted, "a `concurrency` in `offer`");
    refused(&plant(group, ""), "a `concurrency` in `deploy`");
}

/// Proves nothing in the file runs on a pull request.
#[test]
fn nothing_in_the_file_runs_on_a_pull_request() {
    assert!(!real_gamma().contains("pull_request"));
}

/// The environment references of the workflow's `cargo xtask` lines.
const REFERENCES: [&str; 8] = [
    "$GITHUB_REPOSITORY",
    "$GITHUB_OUTPUT",
    "$GITHUB_STEP_SUMMARY",
    "$RUNNER_TEMP",
    "$RUN_ID",
    "$SHA",
    "$GAMMA_HOST",
    "$GAMMA_USER",
];

/// A word of a `run:` line without its quotes, each environment reference replaced by `1`.
fn word(raw: &str) -> String {
    let plain = raw.trim_matches('"').to_owned();
    REFERENCES.iter().fold(plain, |w, var| w.replace(var, "1"))
}

fn split(line: &str) -> Vec<String> {
    line.split(' ').map(word).collect()
}

/// The words of every `cargo xtask` line of the job `job`, `cargo xtask` cut off.
fn xtask_words(text: &str, job: &str) -> Vec<Vec<String>> {
    let steps = job_steps(text, job).expect("steps");
    let lines = steps.iter().filter_map(|s| s.run.as_deref());
    let lines = lines.filter_map(|line| line.strip_prefix("cargo xtask "));
    lines.map(split).collect()
}

/// Catches: a flag of the workflow's `run:` lines that xtask does not take (`--run-id` spelled
/// `--run_id`, a required flag left out, a verb renamed). `offer` runs on every release whether
/// or not the repository variables are set, so without this the first real release would be the
/// first parse of its lines. Each line is given to xtask's own argument parser.
#[test]
fn every_xtask_line_of_the_workflow_is_a_command_line_xtask_accepts() {
    let text = real_gamma();
    let mut seen = 0;
    for job in ["offer", "deploy"] {
        for words in xtask_words(&text, job) {
            let args: Vec<&str> = words.iter().map(String::as_str).collect();
            assert!(!words.iter().any(|w| w.contains('$')), "{args:?}");
            assert_eq!(xtask::check_args(&args), Ok(()), "{args:?}");
            seen += 1;
        }
    }
    assert_eq!(seen, 4);
    // The check is not blind: a misspelt flag and a missing one are refused.
    let check = |line: &str| xtask::check_args(&line.split(' ').collect::<Vec<_>>());
    assert!(check("gamma sudoers-line --user x").is_ok());
    assert!(check("gamma evidence --repo 1 --sha 1 --run_id 1").is_err());
    assert!(check("gamma evidence --repo 1").is_err());
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
