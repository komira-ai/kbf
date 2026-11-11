//! [`problems`] on the real file, and on copies of it with one defect each: every rule of the
//! module's docs refuses its defect and names it.

use super::*;

/// The real file with the first `from` replaced by `to`; the change must alter the text.
fn changed(from: &str, to: &str) -> String {
    let text = real_file();
    let spoiled = text.replacen(from, to, 1);
    assert_ne!(spoiled, text, "`{from}` is not in the file");
    spoiled
}

/// `text` has a problem line that starts with or contains `says`.
fn says(text: &str, says: &str) {
    let found = problems(text);
    assert!(
        found.iter().any(|p| p.contains(says)),
        "want {says:?} in {found:#?}"
    );
}

#[test]
fn hostkeeper_desired_yml_checks_the_release_then_sets_it_by_its_targets() {
    // Catches, on the real file: a trigger added or dropped, the gate dropped from a job, the
    // check skipped or not handed on, hostkeeper's workflow called with another release, pin,
    // coordinator or tailnet.
    assert_eq!(problems(&real_file()), Vec::<String>::new());
}

#[test]
fn a_file_that_is_not_one_yaml_document_is_refused() {
    assert!(
        problems("a: [")
            .iter()
            .any(|p| p.contains("not valid YAML"))
    );
}

#[test]
fn the_triggers_are_the_completion_of_release_on_main_and_a_dispatch_with_one_input() {
    says(
        &changed("on:\n", "on:\n  push:\n    branches: [main]\n"),
        "triggers are",
    );
    says(
        &changed(
            "  workflow_dispatch:\n",
            "  schedule:\n    - cron: '0 * * * *'\n  workflow_dispatch:\n",
        ),
        "triggers are",
    );
    says(
        &changed("workflows: [release]", "workflows: [pipeline]"),
        "workflow_run.workflows",
    );
    says(
        &changed("types: [completed]", "types: [requested]"),
        "workflow_run.types",
    );
    says(
        &changed("branches: [main]", "branches: [main, dev]"),
        "workflow_run.branches",
    );
    says(
        &changed(
            "    branches: [main]\n",
            "    branches: [main]\n    paths: ['**']\n",
        ),
        "workflow_run.paths is a filter it does not have",
    );
    says(
        &changed(
            "        required: true\n",
            "        required: true\n      extra:\n        type: string\n",
        ),
        "inputs are",
    );
    says(
        &changed("        type: string\n", "        type: boolean\n"),
        "input version: type",
    );
    says(
        &changed("        required: true\n", "        required: false\n"),
        "input version: required",
    );
}

#[test]
fn the_concurrency_never_cancels_and_splits_hand_runs_from_release_runs() {
    says(
        &changed("cancel-in-progress: false", "cancel-in-progress: true"),
        "concurrency is",
    );
    says(
        &changed("'dispatch' || 'release'", "'release' || 'release'"),
        "concurrency is",
    );
    let text = real_file();
    let at = text.find("concurrency:").expect("concurrency");
    let end = text[at..].find("\njobs:").expect("jobs") + at;
    let without = format!("{}{}", &text[..at], &text[end..]);
    says(&without, "concurrency is None");
}

#[test]
fn the_top_level_grants_nothing() {
    says(
        &changed("permissions: {}\n", "permissions:\n  contents: read\n"),
        "permissions: {}",
    );
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_both_jobs() {
    // Catches: a gate on a step (the job would start, and the token exist, with the deploy off),
    // a gate on another variable or on a value other than `true`, a job that runs from a branch
    // other than main, and a release run acted on although it failed, ran on another branch or
    // came from a fork.
    for condition in [CHECK_IF, SET_IF] {
        assert!(condition.contains("github.ref == 'refs/heads/main'"));
        assert!(condition.contains("vars.KBF_SETS_DESIRED == 'true'"));
    }
    for (part, with) in [
        (
            "vars.KBF_SETS_DESIRED == 'true'",
            "vars.KBF_SETS_DESIRED != ''",
        ),
        ("github.ref == 'refs/heads/main' && ", ""),
        ("github.event.workflow_run.conclusion == 'success' && ", ""),
        ("github.event.workflow_run.head_branch == 'main' && ", ""),
        (
            " && github.event.workflow_run.head_repository.full_name == github.repository",
            "",
        ),
    ] {
        assert!(CHECK_IF.contains(part), "{part}");
        says(
            &changed(CHECK_IF, &CHECK_IF.replacen(part, with, 1)),
            "check: `if` is",
        );
    }
    says(
        &changed(
            SET_IF,
            &SET_IF.replacen(" && needs.check.outputs.set == 'true'", "", 1),
        ),
        "set-desired: `if` is",
    );
    for (job, condition) in [("check", CHECK_IF), ("set-desired", SET_IF)] {
        says(
            &changed(&format!("    if: {condition}\n"), ""),
            &format!("{job}: `if` is None"),
        );
    }
    // The gate moved from the job to its step.
    says(
        &changed(
            "        run: cargo xtask release desired",
            "        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n        run: cargo xtask release desired",
        ),
        "check: a step has an `if`",
    );
}

#[test]
fn the_jobs_are_check_and_set_desired() {
    says(
        &changed(
            "  set-desired:\n",
            "  other:\n    runs-on: ubuntu-24.04\n    steps: []\n\n  set-desired:\n",
        ),
        "jobs are",
    );
}

/// Each `(from, to, says)`: the real file with that one change has a problem containing `says`.
const DEFECTS: &[(&str, &str, &str)] = &[
    (
        "    runs-on: ubuntu-24.04\n",
        "    needs: [set-desired]\n    runs-on: ubuntu-24.04\n",
        "check: needs [\"set-desired\"]; it needs no job",
    ),
    (
        "      actions: read\n",
        "      actions: write\n",
        "check: permissions are",
    ),
    // Without `actions: read` the jobs of the release run cannot be read.
    ("      actions: read\n", "", "check: permissions are"),
    // The check holds no token that signs.
    (
        "      contents: read\n      actions: read\n",
        "      contents: read\n      actions: read\n      id-token: write\n",
        "check: permissions are",
    ),
    (
        "--dir \"$RUNNER_TEMP/desired\"",
        "--dir dist",
        "check: the step `desired` does not run",
    ),
    // A run after a release that ignores which run it was: every run would set a release.
    (
        " --after-run \"$RUN_ID\"",
        "",
        "check: the step `desired` does not run",
    ),
    (
        "        id: desired\n",
        "        id: index\n",
        "check: the step `desired` does not run",
    ),
    (
        "      - run: cargo build -p xtask --locked\n",
        "",
        "is not a step before the step `desired`",
    ),
    (
        "VERSION: ${{ inputs.version }}",
        "VERSION: 0.1.0",
        "check: the step `desired` does not read VERSION from ${{ inputs.version }}",
    ),
    (
        "RUN_ID: ${{ github.event.workflow_run.id }}",
        "RUN_ID: ${{ github.run_id }}",
        "check: the step `desired` does not read RUN_ID",
    ),
    (
        "GH_TOKEN: ${{ github.token }}",
        "GH_TOKEN: ''",
        "check: the step `desired` does not read GH_TOKEN",
    ),
    (
        "      set: ${{ steps.desired.outputs.set }}\n",
        "      set: 'true'\n",
        "check: output set is",
    ),
    (
        "      targets: ${{ steps.desired.outputs.targets }}\n",
        "      targets: ${{ steps.desired.outputs.targets }}\n      extra: ${{ steps.desired.outputs.set }}\n",
        "check: it hands on 4 outputs",
    ),
    (
        "    needs: [check]\n",
        "    needs: [check, other]\n",
        "set-desired: needs are",
    ),
    (
        "      id-token: write\n",
        "      id-token: write\n      actions: write\n",
        "set-desired: permissions are",
    ),
    (
        "      id-token: write\n",
        "",
        "set-desired: permissions are",
    ),
    (
        "set-desired.yml@318bcc93c9bcd009d40597c45227278742acc622 # v0.1.242",
        "set-desired.yml@main # v0.1.242",
        "set-desired: `uses` is not",
    ),
    (
        "    uses: komira-ai/hostkeeper/",
        "    uses: komira-ai/other/",
        "set-desired: `uses` is not",
    ),
    (
        "318bcc93c9bcd009d40597c45227278742acc622 # v0.1.242",
        "318bcc93c9bcd009d40597c45227278742acc622 # v0.1.241",
        "the pin's comment names",
    ),
    (
        "318bcc93c9bcd009d40597c45227278742acc622 # v0.1.242",
        "318bcc93c9bcd009d40597c45227278742acc622",
        "the pin's comment names None",
    ),
    (
        "      service: kbf-storage-and-daemon\n",
        "      service: js-agent\n",
        "set-desired: with.service is",
    ),
    (
        "      version: ${{ needs.check.outputs.package-version }}\n",
        "      version: 0.1.1\n",
        "set-desired: with.version is",
    ),
    (
        "      targets: ${{ needs.check.outputs.targets }}\n",
        "      targets: '{}'\n",
        "set-desired: with.targets is",
    ),
    (
        "      hostkeeper_version: 0.1.242\n",
        "      hostkeeper_version: 0.1.241\n",
        "set-desired: with.hostkeeper_version is",
    ),
    (
        "      hostkeeper_commit: 318bcc93c9bcd009d40597c45227278742acc622\n",
        "      hostkeeper_commit: 0000000000000000000000000000000000000000\n",
        "set-desired: with.hostkeeper_commit is",
    ),
    (
        "      coordinator_url: ${{ vars.HOSTKEEPER_COORDINATOR_URL }}\n",
        "      coordinator_url: ${{ vars.KBF_COORDINATOR_URL }}\n",
        "set-desired: with.coordinator_url is",
    ),
    (
        "      coordinator_ca: ${{ vars.HOSTKEEPER_COORDINATOR_CA }}\n",
        "",
        "set-desired: with.coordinator_ca is None",
    ),
    (
        "      audience: kbf-hostkeeper-desired\n",
        "      audience: jobscheduler-hostkeeper-desired\n",
        "set-desired: with.audience is",
    ),
    // The call returns once the coordinator stored the version.
    (
        "      wait: false\n",
        "      wait: true\n",
        "set-desired: with.wait is",
    ),
    (
        "      tailnet_tag: ${{ vars.HOSTKEEPER_TAILNET_TAG }}\n",
        "      tailnet_tag: tag:ci\n",
        "set-desired: with.tailnet_tag is",
    ),
    (
        "      wait: false\n",
        "      wait: false\n      host_group: farm\n",
        "set-desired: with.host_group is not an input it gives",
    ),
    (
        "    with:\n      service:",
        "    secrets: inherit\n    with:\n      service:",
        "set-desired: the job has a `secrets:` key",
    ),
    (
        "          GH_TOKEN: ${{ github.token }}\n",
        "          GH_TOKEN: ${{ secrets.TOKEN }}\n",
        "the file names a secret",
    ),
];

#[test]
fn each_defect_of_the_table_is_refused_and_named() {
    for (from, to, wanted) in DEFECTS {
        says(&changed(from, to), wanted);
    }
}
