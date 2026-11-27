//! Tests of the rules for the steps that run buck2 against the farm and the bound of every step
//! of their workflows ([`super`]): the committed workflows pass, and each rule refuses its defect
//! put into a committed workflow, read by its exact problem.

use super::{farm_steps, is_komira_checkout, problems};
use crate::workflows::{decode, job_names, job_steps};

const FARM: &str = ".github/workflows/farm.yml";
const RELEASE: &str = ".github/workflows/release.yml";
/// The farm step of `farm.yml`, as a problem names it.
const LANE: &str = "build/buck2 build //:ci on the build farm, local 0";
/// The farm step of `release.yml`, as a problem names it.
const PROOF: &str = "gamma/Prove the farm with a remote-only Buck2 build";
/// The flag that hands a farm verb the tool.
const FLAG: &str = " --assert-remote \"$RUNNER_TEMP/assert_remote\"";

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// The committed workflow `path` with its one occurrence of `from` replaced by `to`.
fn with(path: &str, from: &str, to: &str) -> String {
    let text = read(path);
    assert_eq!(text.matches(from).count(), 1, "{from:?} once in {path}");
    text.replacen(from, to, 1)
}

/// The problem of a farm step without the flag.
fn no_flag(step: &str, verb: &str) -> String {
    format!(
        "{step}: `{verb}` is not given `--assert-remote \"$RUNNER_TEMP/assert_remote\"`: its buck2 would run with no watchdog on its connection to the farm"
    )
}

/// The problem of a step without a bound under the job's.
fn unbounded(step: &str, own: &str, job: u64) -> String {
    format!(
        "{step}: its `timeout-minutes` ({own}) is not a whole number from 1 under the job's {job}: a hung step would run to the job's timeout"
    )
}

#[test]
fn every_workflow_passes_and_the_farm_steps_are_the_lanes_build_and_the_gamma_proof() {
    // Catches: rules that refuse the files they were written for, and a reading of farm steps
    // that finds none (every other rule would then hold nothing) or misses one of the two.
    let root = crate::repo_root();
    let mut found = Vec::new();
    for f in crate::listed_files(&root)
        .iter()
        .filter(|f| crate::is_workflow(f))
    {
        let Some(bytes) = crate::read_bytes(&root.join(f)) else {
            continue;
        };
        let text = decode(&bytes).expect("a workflow is UTF-8");
        assert_eq!(problems(text), Vec::<String>::new(), "{f}");
        for (job, step) in farm_steps(text).expect("parses") {
            found.push(format!("{f}#{job}/{step}"));
        }
    }
    assert_eq!(
        found,
        [format!("{FARM}#{LANE}"), format!("{RELEASE}#{PROOF}")]
    );
}

#[test]
fn both_workflows_check_out_komira_once_at_one_commit() {
    // Catches: the lane and the release proof running assert_remote from two commits of komira,
    // which would let one of them run a watchdog the other was never tested with.
    let mut refs = Vec::new();
    for path in [FARM, RELEASE] {
        let text = read(path);
        for job in job_names(&text).expect("jobs") {
            for step in job_steps(&text, &job).unwrap_or_default() {
                if is_komira_checkout(&step) {
                    let r = step.with.iter().find(|(k, _)| k == "ref").expect("a ref");
                    refs.push(r.1.clone());
                }
            }
        }
    }
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert_eq!(refs[0], refs[1]);
}

#[test]
fn a_farm_step_without_the_tool_is_refused() {
    // Catches: the lane's build or the release proof run with no watchdog, and one handed a path
    // where no step builds the tool.
    let text = with(FARM, FLAG, "");
    assert_eq!(problems(&text), [no_flag(LANE, "cargo xtask farm-build")]);
    let text = with(
        FARM,
        FLAG,
        " --assert-remote \"$RUNNER_TEMP/bin/assert_remote\"",
    );
    assert_eq!(problems(&text), [no_flag(LANE, "cargo xtask farm-build")]);
    let text = with(RELEASE, FLAG, "");
    assert_eq!(problems(&text), [no_flag(PROOF, "cargo xtask prove-farm")]);
}

#[test]
fn a_step_that_runs_buck2_itself_is_refused_whatever_its_path() {
    // Catches: a buck2 command against the farm added outside the two verbs (no watchdog), by
    // its pinned path, quoted or not, a checkout's launcher, or a bare name.
    let upload = "      - name: Upload the regenerated build table as a patch\n";
    for run in [
        "'\"$RUNNER_TEMP/bin/buck2\" build //:ci'",
        "$RUNNER_TEMP/bin/buck2 build //:ci",
        "./buck2 test //:ci",
        "buck2 build //:ci",
    ] {
        let text = with(
            FARM,
            upload,
            &format!("      - name: Raw\n        timeout-minutes: 5\n        run: {run}\n{upload}"),
        );
        assert_eq!(
            problems(&text),
            [
                "build/Raw: runs buck2 itself; buck2 reaches the farm only through `cargo xtask farm-build` or `cargo xtask prove-farm`, under assert_remote's watchdog"
            ],
            "{run}"
        );
    }
}

#[test]
fn a_komira_checkout_that_is_not_pinned_or_not_exactly_the_sources_is_refused() {
    // Catches: the tool fetched at a branch or a short commit (its bytes could change under the
    // lane), a checkout that keeps the token or takes more than the tool's sources, and none.
    let pinned = "          ref: b2080fe989248adf9b47d4bd14c0ecefa0802f79\n";
    let wrong = "build/actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1: the checkout of komira-ai/komira is not given exactly [(\"repository\", \"komira-ai/komira\"), (\"path\", \"buck-out/komira\"), (\"persist-credentials\", \"false\"), (\"sparse-checkout\", \"/tools/build/ci/assert_remote/\"), (\"sparse-checkout-cone-mode\", \"false\")] and a `ref:` of a full 40-digit lowercase commit";
    for to in [
        "          ref: main\n",
        "          ref: b2080fe\n",
        "          ref: B2080FE989248ADF9B47D4BD14C0ECEFA0802F79\n",
        "",
        "          ref: b2080fe989248adf9b47d4bd14c0ecefa0802f79\n          token: ${{ github.token }}\n",
    ] {
        assert_eq!(problems(&with(FARM, pinned, to)), [wrong], "{to:?}");
    }
    for (from, to) in [
        (
            "          persist-credentials: false\n          sparse",
            "          persist-credentials: true\n          sparse",
        ),
        ("          sparse-checkout-cone-mode: false\n", ""),
        (
            "          path: buck-out/komira\n",
            "          path: komira\n",
        ),
    ] {
        assert_eq!(problems(&with(FARM, from, to)), [wrong], "{to:?}");
    }
    let start = "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        timeout-minutes: 5\n        with:\n          repository: komira-ai/komira\n";
    let block = format!(
        "{start}{pinned}          path: buck-out/komira\n          persist-credentials: false\n          sparse-checkout: /tools/build/ci/assert_remote/\n          sparse-checkout-cone-mode: false\n"
    );
    assert_eq!(
        problems(&with(FARM, &block, "")),
        [format!(
            "{LANE}: no earlier step checks out komira-ai/komira, the pinned sources of assert_remote"
        )]
    );
}

#[test]
fn a_farm_step_with_no_build_of_the_tool_before_it_is_refused() {
    // Catches: the tool written elsewhere than the path the verbs are given, and a build that
    // comes after the step that runs it.
    let emit = "-femit-bin=\"$RUNNER_TEMP/assert_remote\"";
    let missing = format!(
        "{LANE}: no earlier step builds assert_remote with `build-exe` and `-femit-bin=\"$RUNNER_TEMP/assert_remote\"`"
    );
    let text = with(FARM, emit, "-femit-bin=\"$RUNNER_TEMP/bin/assert_remote\"");
    assert_eq!(problems(&text), [missing.clone()]);
    let real = read(FARM);
    let start = real
        .find("      - name: Build assert_remote\n")
        .expect("the build");
    let end = real
        .find("      - name: Join the farm's Tailscale network\n")
        .expect("the join");
    let late = format!("{}{}{}", &real[..start], &real[end..], &real[start..end]);
    assert_eq!(problems(&late), [missing]);
}

#[test]
fn every_step_of_a_workflow_with_a_farm_step_has_a_bound_under_its_jobs() {
    // Catches: a step with no bound (it would hang to the job's timeout), one whose bound is the
    // job's or more, a zero, an expression the lint cannot read as minutes, and a job with no
    // bound of its own.
    let rustup = "      - run: rustup toolchain install\n        timeout-minutes: 5\n";
    let step = "build/rustup toolchain install";
    for (to, own) in [
        ("", "none"),
        ("        timeout-minutes: 150\n", "150"),
        ("        timeout-minutes: 151\n", "151"),
        ("        timeout-minutes: 0\n", "0"),
        ("        timeout-minutes: ${{ 5 }}\n", "${{ 5 }}"),
    ] {
        let text = with(
            FARM,
            rustup,
            &format!("      - run: rustup toolchain install\n{to}"),
        );
        assert_eq!(problems(&text), [unbounded(step, own, 150)], "{to:?}");
    }
    // One minute under the job's is a bound.
    let text = with(
        FARM,
        "        timeout-minutes: 120\n",
        "        timeout-minutes: 149\n",
    );
    assert_eq!(problems(&text), Vec::<String>::new());
    let text = with(RELEASE, "        timeout-minutes: 40\n", "");
    assert_eq!(problems(&text), [unbounded(PROOF, "none", 135)]);
    let text = with(FARM, "    timeout-minutes: 150\n", "");
    assert_eq!(
        problems(&text),
        [
            "build: has no whole-number `timeout-minutes`: the workflow runs buck2 against the farm, and every job of it is bounded"
        ]
    );
}

#[test]
fn a_workflow_without_a_farm_step_needs_no_bounds_and_one_with_a_bare_farm_step_needs_everything() {
    // Catches: the bound rule applied to every workflow (it is the farm's), and a farm verb in a
    // new workflow with none of the tool, its sources, its build or the bounds.
    let plain = "on: push\npermissions: {}\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo test --locked\n";
    assert_eq!(problems(plain), Vec::<String>::new());
    let farm = plain.replace("cargo test --locked", "cargo xtask prove-farm --address x");
    let step = "a/cargo xtask prove-farm --address x";
    assert_eq!(
        problems(&farm),
        [
            no_flag(step, "cargo xtask prove-farm"),
            format!(
                "{step}: no earlier step checks out komira-ai/komira, the pinned sources of assert_remote"
            ),
            format!(
                "{step}: no earlier step builds assert_remote with `build-exe` and `-femit-bin=\"$RUNNER_TEMP/assert_remote\"`"
            ),
            "a: has no whole-number `timeout-minutes`: the workflow runs buck2 against the farm, and every job of it is bounded".to_owned(),
        ]
    );
}
