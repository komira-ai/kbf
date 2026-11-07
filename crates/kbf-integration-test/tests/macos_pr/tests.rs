//! [`problems`] on the real files with one change made, so each rule refuses it.

use super::*;

/// The problems of the real workflows with the first `from` in the workflow `file` replaced by
/// `to`. The change must alter the text.
fn changed_to(file: &str, from: &str, to: &str) -> Vec<String> {
    let mut workflows = real_files();
    let text = &mut workflows
        .iter_mut()
        .find(|(n, _)| n == file)
        .unwrap_or_else(|| panic!("no {file}"))
        .1;
    let changed = text.replacen(from, to, 1);
    assert_ne!(&changed, text, "{file}: `{from}` is not in the file");
    *text = changed;
    problems(&workflows)
}

/// `found` holds exactly `want` among its lines.
fn has(found: &[String], want: &str) {
    assert!(
        found.iter().any(|f| f == want),
        "want {want:?} in {found:#?}"
    );
}

/// Some line of `found` holds `part`.
fn has_part(found: &[String], part: &str) {
    assert!(
        found.iter().any(|f| f.contains(part)),
        "want a line with {part:?} in {found:#?}"
    );
}

const TASK_MACOS_IF: &str =
    "    if: ${{ github.event_name != 'pull_request' }}\n    runs-on: macos-latest";

#[test]
fn a_macos_job_a_pull_request_runs_outside_the_list_is_refused() {
    // Catches: ci.yml's `task-macos` back on every pull request (its after-merge `if:` dropped),
    // and a macOS job added to a pull-request workflow that is not on the list.
    let found = changed_to("ci.yml", TASK_MACOS_IF, "    runs-on: macos-latest");
    has(
        &found,
        "ci.yml#task-macos runs on a Mac for a pull request and is not one of the pull request's \
         macOS jobs (PR_MACOS_JOBS): run it in macos-pr.yml, or give it \
         `if: ${{ github.event_name != 'pull_request' }}`",
    );
    let found = changed_to(
        "host-install-linux.yml",
        "runs-on: ubuntu",
        "runs-on: macos-15\n    # was: ubuntu",
    );
    has_part(
        &found,
        "host-install-linux.yml#real-linux runs on a Mac for a pull request and is not one of",
    );
    // A condition that only looks like the after-merge one is no exemption.
    let found = changed_to(
        "ci.yml",
        TASK_MACOS_IF,
        "    if: ${{ github.event_name != 'push' }}\n    runs-on: macos-latest",
    );
    has_part(&found, "ci.yml#task-macos runs on a Mac for a pull request");
    // A label spelt in another case is the same runner to GitHub.
    let found = changed_to(
        "host-install-linux.yml",
        "runs-on: ubuntu",
        "runs-on: MacOS-15\n    # was: ubuntu",
    );
    has_part(
        &found,
        "host-install-linux.yml#real-linux runs on a Mac for a pull request and is not one of",
    );
    // A pull-request workflow that calls a workflow with a macOS job runs that job unexamined.
    let found = changed_to(
        "host-install-linux.yml",
        "jobs:\n",
        "jobs:\n  beta:\n    uses: ./.github/workflows/beta.yml\n",
    );
    has(
        &found,
        "host-install-linux.yml#beta calls beta.yml, which has a macOS job, on a pull request: this lint \
         cannot gate a called job, so run it from macos-pr.yml",
    );
}

#[test]
fn the_secrets_context_is_found_in_every_spelling_and_nothing_else() {
    // Catches: a reader that sees only `secrets.`, and one that refuses a word that merely
    // contains `secrets` (a path, a flag) or the `secrets:` key the other rule reads.
    for named in [
        "${{ secrets.X }}",
        "${{ SECRETS.X }}",
        "${{ secrets['X'] }}",
        "${{ toJSON(secrets) }}",
        "${{ secrets }}",
        "secrets",
    ] {
        assert!(names_secrets(named), "{named}");
    }
    for not in [
        "kbf-secrets/a.rs",
        "my_secrets.txt",
        "secretsfile",
        "--no-secrets",
        "secrets: inherit",
        "secret",
    ] {
        assert!(!names_secrets(not), "{not}");
    }
}

#[test]
fn a_listed_job_that_lost_its_gate_is_refused() {
    // Catches: macos-pr.yml's jobs given a condition (a job it skips reports success, so a
    // required check would pass without a Mac), artifacts.yml's darwin build without its label,
    // and a workflow's own `pull_request.paths` dropped.
    for job in ["macos-test", "macos-device"] {
        let found = changed_to(
            MACOS_PR,
            &format!("  {job}:\n"),
            &format!("  {job}:\n    if: ${{{{ github.actor != 'x' }}}}\n"),
        );
        has(
            &found,
            &format!(
                "macos-pr.yml#{job} has `if: ${{{{ github.actor != 'x' }}}}`; it runs on every pull request, \
                 and a job its condition skips reports success without having run on a Mac"
            ),
        );
    }
    let found = changed_to(
        "artifacts.yml",
        "    if: ${{ (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}\n    runs-on: macos-15",
        "    if: ${{ inputs.stage != 'sign' }}\n    runs-on: macos-15",
    );
    has(
        &found,
        "artifacts.yml#darwin-arm64 would run on a Mac for every pull request: its gate (Label) is gone",
    );
    // A condition that still names the label but no longer requires it.
    let found = changed_to(
        "artifacts.yml",
        "contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}\n    runs-on: macos-15",
        "contains(github.event.pull_request.labels.*.name, 'needs-artifacts') || true) && inputs.stage != 'sign' }}\n    runs-on: macos-15",
    );
    has(
        &found,
        "artifacts.yml#darwin-arm64 would run on a Mac for every pull request: its gate (Label) is gone",
    );
    // A paths filter that matches every path.
    let found = changed_to(
        "vmm-probe.yml",
        "  pull_request:\n    paths:\n      - crates/kbf-vmm/**\n",
        "  pull_request:\n    paths:\n      - '**'\n      - crates/kbf-vmm/**\n",
    );
    has(
        &found,
        "vmm-probe.yml#darwin-arm64 would run on a Mac for every pull request: its gate (TriggerPaths) is gone",
    );
    let found = changed_to(
        "host-install-mac.yml",
        "  pull_request:\n    paths:\n      - crates/kbf-host-install/**\n      - .github/workflows/host-install-mac.yml\n",
        "  pull_request:\n",
    );
    has(
        &found,
        "host-install-mac.yml#real-mac would run on a Mac for every pull request: its gate (TriggerPaths) is gone",
    );
}

#[test]
fn a_listed_job_that_is_gone_or_off_a_mac_is_stale() {
    // Catches: a list that keeps a row for a job moved to Linux or renamed, so the next macOS job
    // given its name would pass unexamined.
    let found = changed_to(
        "vmm-probe.yml",
        "    runs-on: macos-15",
        "    runs-on: ubuntu-latest",
    );
    has(
        &found,
        "vmm-probe.yml#darwin-arm64 is listed as a pull request's macOS job, and is no job on a Mac: drop it from the list",
    );
    let found = changed_to(
        "vmm-probe.yml",
        "  darwin-arm64:\n",
        "  darwin-arm64-renamed:\n",
    );
    has(
        &found,
        "vmm-probe.yml#darwin-arm64 is listed as a pull request's macOS job, and is no job on a Mac: drop it from the list",
    );
    has_part(
        &found,
        "vmm-probe.yml#darwin-arm64-renamed runs on a Mac for a pull request",
    );
}

#[test]
fn a_pull_request_macos_job_with_a_secret_is_refused() {
    // Catches: a secret handed to code a pull request controls on a Mac, through the job's own
    // `env:`, a step's `env:`, the GitHub token spelt as a secret, the workflow's top-level `env:`
    // or `secrets: inherit`.
    let found = changed_to(
        MACOS_PR,
        "    env:\n      # The darwin-arm64 build",
        "    env:\n      TOKEN: ${{ secrets.DEPLOY_TOKEN }}\n      # The darwin-arm64 build",
    );
    has(
        &found,
        "macos-pr.yml#macos-device names a secret (the `secrets` context); a pull request's macOS job holds none",
    );
    let found = changed_to(
        MACOS_PR,
        "      - run: cargo test -p kbf-task --locked --no-fail-fast\n",
        "      - run: cargo test -p kbf-task --locked --no-fail-fast\n        env:\n          KEY: ${{ Secrets.SIGNING_KEY }}\n",
    );
    has(
        &found,
        "macos-pr.yml#macos-test names a secret (the `secrets` context); a pull request's macOS job holds none",
    );
    for form in [
        "${{ secrets['DEPLOY_TOKEN'] }}",
        "${{ toJSON(secrets) }}",
        "${{ secrets }}",
    ] {
        let found = changed_to(
            MACOS_PR,
            "    env:\n      # The darwin-arm64 build",
            &format!("    env:\n      ALL: {form}\n      # The darwin-arm64 build"),
        );
        has(
            &found,
            "macos-pr.yml#macos-device names a secret (the `secrets` context); a pull request's macOS job holds none",
        );
    }
    let found = changed_to(
        "host-install-mac.yml",
        "GH_TOKEN: ${{ github.token }}",
        "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
    );
    has(
        &found,
        "host-install-mac.yml#real-mac names a secret (the `secrets` context); a pull request's macOS job holds none",
    );
    let found = changed_to(
        MACOS_PR,
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  TOKEN: ${{ secrets.DEPLOY_TOKEN }}\n",
    );
    has(
        &found,
        "macos-pr.yml's top-level `env:` names a secret, which macos-pr.yml#macos-test would hold",
    );
    let found = changed_to(
        MACOS_PR,
        "    timeout-minutes: 75\n",
        "    timeout-minutes: 75\n    secrets: inherit\n",
    );
    has(
        &found,
        "macos-pr.yml#macos-test passes `secrets:`; a pull request's macOS job holds none",
    );
}

#[test]
fn a_pull_request_macos_job_with_an_environment_oidc_or_a_write_scope_is_refused() {
    // Catches: an environment (whose secrets the job would hold), the OIDC token at any level
    // but none, and a write scope, on a pull request's macOS job.
    let found = changed_to(
        MACOS_PR,
        "    timeout-minutes: 90\n",
        "    timeout-minutes: 90\n    environment: mac\n",
    );
    has(
        &found,
        "macos-pr.yml#macos-device runs in an environment, whose secrets it would hold",
    );
    for (level, oidc) in [("write", true), ("read", true)] {
        let found = changed_to(
            MACOS_PR,
            "    timeout-minutes: 90\n    permissions:\n      contents: read\n",
            &format!(
                "    timeout-minutes: 90\n    permissions:\n      contents: read\n      id-token: {level}\n"
            ),
        );
        has(
            &found,
            &format!(
                "macos-pr.yml#macos-device sets `id-token: {level}`; a pull request's macOS job may only read{}",
                if oidc { ", and has no OIDC token" } else { "" }
            ),
        );
    }
    let found = changed_to(
        "mac-provision.yml",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: write\n",
    );
    has(
        &found,
        "mac-provision.yml#darwin-arm64 sets `contents: write`; a pull request's macOS job may only read",
    );
}

#[test]
fn the_workflow_runs_on_every_pull_request_and_queue_entry_and_nothing_else() {
    // Catches: a push trigger (the after-merge Mac jobs are beta.yml's), and a filter on the
    // trigger, which leaves the macOS checks unreported on a pull request it filters out.
    let found = changed_to(
        MACOS_PR,
        "  merge_group:\n",
        "  merge_group:\n  push:\n    branches: [main]\n",
    );
    has(
        &found,
        "macos-pr.yml: runs on [\"pull_request\", \"merge_group\", \"push\"]; it runs on `pull_request` and `merge_group` alone",
    );
    let found = changed_to(
        MACOS_PR,
        "on:\n  pull_request:\n",
        "on:\n  pull_request:\n    paths: ['crates/**']\n",
    );
    has_part(&found, "macos-pr.yml: filters its `pull_request` trigger");
    // Without the merge queue trigger, a required macOS check never reports for a queue entry.
    let found = changed_to(MACOS_PR, "  merge_group:\n", "");
    has(
        &found,
        "macos-pr.yml: runs on [\"pull_request\"]; it runs on `pull_request` and `merge_group` alone",
    );
    let found = changed_to(
        MACOS_PR,
        "  merge_group:\n",
        "  merge_group:\n    types: [checks_requested]\n",
    );
    has_part(&found, "macos-pr.yml: filters its `merge_group` trigger");
}

#[test]
fn a_burst_of_pushes_cancels_the_older_runs() {
    // Catches: a group per run (nothing is cancelled), and a group that does not cancel (a burst
    // of pushes queues a set of macOS runners for each).
    for (from, to) in [
        (
            "  cancel-in-progress: ${{ github.event_name == 'pull_request' }}\n",
            "  cancel-in-progress: false\n",
        ),
        (
            "  group: macos-pr-${{ github.event.pull_request.number || github.ref }}\n",
            "  group: macos-pr-${{ github.run_id }}\n",
        ),
    ] {
        has_part(
            &changed_to(MACOS_PR, from, to),
            "macos-pr.yml: its concurrency is",
        );
    }
}

#[test]
fn the_jobs_keep_their_runner_and_their_independence() {
    // Catches: a job moved off macos-15, made to need another job (a failed or skipped need
    // skips it, and a skipped required check passes), allowed to fail, or a job added, dropped
    // or renamed (the required check names it).
    for (from, to, want) in [
        (
            "  macos-test:\n    runs-on: macos-15\n",
            "  macos-test:\n    runs-on: macos-26\n",
            "macos-pr.yml: `macos-test` runs on `macos-26`, not `macos-15`",
        ),
        (
            "  macos-device:\n",
            "  macos-device:\n    needs: macos-test\n",
            "macos-pr.yml: `macos-device` needs [\"macos-test\"]: a failed or skipped need skips it, and a skipped required check passes",
        ),
        (
            "    timeout-minutes: 90\n",
            "    timeout-minutes: 90\n    continue-on-error: true\n",
            "macos-pr.yml: `macos-device` has a `continue-on-error`, which lets it pass on a failure",
        ),
        (
            "  macos-device:\n",
            "  macos-devices:\n",
            "macos-pr.yml: its jobs are [\"macos-test\", \"macos-devices\"], not [\"macos-test\", \"macos-device\"]",
        ),
    ] {
        has(&changed_to(MACOS_PR, from, to), want);
    }
}

#[test]
fn a_step_of_the_after_merge_mac_jobs_that_no_pull_request_runs_is_refused() {
    // Catches: a step added to beta.yml's native-macos (or changed there: its flags, its `env:`,
    // its `if:`) and not here, a step dropped here, and a job env the device steps need.
    let found = changed_to(
        "beta.yml",
        "      - run: cargo test -p kbf-guest --locked\n",
        "      - run: cargo test -p kbf-guest --locked\n      - run: cargo test -p kbf-new --locked\n",
    );
    has(
        &found,
        "beta.yml#native-macos runs `cargo test -p kbf-new --locked` and no step of macos-pr.yml's macOS jobs is that step",
    );
    let found = changed_to(
        MACOS_PR,
        "      - run: cargo xtask sudo-test -p kbf-driver-native --test slot_gpu -- --test-threads=1 --nocapture\n",
        "",
    );
    has(
        &found,
        "beta.yml#native-macos runs `cargo xtask sudo-test -p kbf-driver-native --test slot_gpu -- --test-threads=1 --nocapture` and no step of macos-pr.yml's macOS jobs is that step",
    );
    let found = changed_to(
        MACOS_PR,
        "      - run: cargo xtask sudo-test -p kbf-mac-provision --test as_root -- --test-threads=1 --nocapture\n        if: ${{ !cancelled() }}\n",
        "      - run: cargo xtask sudo-test -p kbf-mac-provision --test as_root -- --test-threads=1 --nocapture\n",
    );
    has_part(
        &found,
        "beta.yml#native-macos runs `cargo xtask sudo-test -p kbf-mac-provision --test as_root",
    );
    let found = changed_to(
        "beta.yml",
        "MACOSX_DEPLOYMENT_TARGET: \"14.0\"\n      - run: cargo test -p xtask",
        "MACOSX_DEPLOYMENT_TARGET: \"15.0\"\n      - run: cargo test -p xtask",
    );
    has_part(
        &found,
        "beta.yml#native-macos runs `cargo xtask sudo-test -p xtask --test pkg_macos",
    );
    let found = changed_to(
        "beta.yml",
        "a Mac pins (see test-x86).\n      GH_VERSION: 2.102.0\n",
        "a Mac pins (see test-x86).\n      GH_VERSION: 2.103.0\n",
    );
    has(
        &found,
        "beta.yml#native-macos sets `GH_VERSION: 2.103.0` and macos-pr.yml's `macos-device` does not",
    );
    let found = changed_to(
        "ci.yml",
        "      - run: cargo test -p kbf-task --locked --no-fail-fast\n",
        "      - run: cargo test -p kbf-task --locked --no-fail-fast --release\n",
    );
    has(
        &found,
        "ci.yml#task-macos runs `cargo test -p kbf-task --locked --no-fail-fast --release` and no step of macos-pr.yml's macOS jobs is that step",
    );
}

#[test]
fn a_crate_the_mac_tests_whole_and_the_linux_check_misses_is_refused() {
    // Catches: a crate added to `macos-test` and not to ci.yml's darwin `--tests` check, so a
    // macOS compile error in it reaches main from a pull request that runs no Mac.
    let found = changed_to("ci.yml", " -p kbf-updater -p kbf-update", " -p kbf-update");
    has(
        &found,
        "macos-pr.yml#macos-test tests kbf-updater whole, and ci.yml's `lint` job does not check its tests for aarch64-apple-darwin: add `-p kbf-updater` there",
    );
}

#[test]
fn a_cargo_line_selects_its_packages_and_targets() {
    // Catches: a reader that takes a test filter after `--` for a package, misses `--package`,
    // or reads a line that selects one target as testing the crate whole.
    let s = |run: &str| selection(run);
    assert_eq!(
        s("cargo test -p a --package b --locked -- -p not-a-package --test nor"),
        Some(Selection {
            packages: vec!["a".into(), "b".into()],
            whole: true,
        })
    );
    assert_eq!(
        s("cargo xtask sudo-test -p a --test t1 --test t2 -- --nocapture"),
        Some(Selection {
            packages: vec!["a".into()],
            whole: false,
        })
    );
    for partial in ["cargo build -p a --bin b", "cargo check -p a --lib"] {
        assert_eq!(
            s(partial),
            Some(Selection {
                packages: vec!["a".into()],
                whole: false,
            }),
            "{partial}"
        );
    }
    assert_eq!(s("cargo xtask fetch --url x"), None);
    assert_eq!(s("sw_vers"), None);
}
