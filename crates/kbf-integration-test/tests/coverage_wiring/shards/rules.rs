//! The rules of [`super`], on planted workflows: each way the split stops being whole, and the
//! shape `ci.yml` has, which passes.

use super::*;

const LINT_JOB: &str = "  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n";

const LEGACY_ARGS: &str = "--workspace";
const JS_ARGS: &str = "-p kbf-server --features kbf-server/farm-core-js";

fn entry(core: &str, part: usize, parts: usize, args: &str) -> String {
    let gb = if core == "js" { 35 } else { 40 };
    entry_with_disk(core, part, parts, args, &gb.to_string())
}

fn entry_with_disk(core: &str, part: usize, parts: usize, args: &str, gb: &str) -> String {
    format!(
        "          - core: {core}\n            part: {part}\n            parts: {parts}\n            min_free_gb: {gb}\n            args: {args}\n"
    )
}

fn matrix(entries: &str) -> String {
    format!("    strategy:\n      matrix:\n        include:\n{entries}")
}

fn good_matrix() -> String {
    matrix(&format!(
        "{}{}{}",
        entry("legacy", 1, 2, LEGACY_ARGS),
        entry("legacy", 2, 2, LEGACY_ARGS),
        entry("js", 1, 1, JS_ARGS)
    ))
}

const SHARD_ENV: &str = "    env:\n      PART: ${{ matrix.part }}\n      PARTS: ${{ matrix.parts }}\n      ARGS: ${{ matrix.args }}\n      MIN_FREE_GB: ${{ matrix.min_free_gb }}\n      LLVM_PROFILE_FILE_NAME: kbf-${{ matrix.core }}-${{ matrix.part }}-%p-%4m.profraw\n";
const SHARD_STEPS: &str = "    steps:\n      - run: cargo xtask runner-disk --path . --min-free-gb \"$MIN_FREE_GB\"\n      - run: cargo llvm-cov nextest $ARGS --locked --branch --no-report --no-fail-fast --profile pr \"--partition=count:$PART/$PARTS\"\n      - if: ${{ always() }}\n        run: df -h /\n      - run: cargo xtask merge-profiles --dir target/llvm-cov-target --extension profraw --prefix kbf-${{ matrix.core }}-${{ matrix.part }}- --out target/llvm-cov-target/kbf-${{ matrix.core }}-${{ matrix.part }}-merged.profdata\n      - uses: actions/upload-artifact@abc # v7\n        with:\n          name: profdata-${{ matrix.core }}-${{ matrix.part }}\n          path: target/llvm-cov-target/kbf-${{ matrix.core }}-${{ matrix.part }}-merged.profdata\n          if-no-files-found: error\n          overwrite: true\n";

fn shard_job(matrix: &str) -> String {
    format!(
        "  test-shard:\n    name: test-shard (${{{{ matrix.core }}}}-${{{{ matrix.part }}}}-of-${{{{ matrix.parts }}}})\n    runs-on: ubuntu-latest\n{matrix}{SHARD_ENV}{SHARD_STEPS}"
    )
}

const GATE_HEAD: &str = "  test:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: read\n      actions: read\n    steps:\n";
const BUILD_LEGACY: &str = "      - run: cargo llvm-cov nextest --workspace --locked --branch --no-report --profile pr -E 'none()' --no-tests=pass\n";
const BUILD_JS: &str = "      - run: cargo llvm-cov nextest -p kbf-server --features kbf-server/farm-core-js --locked --branch --no-report --profile pr -E 'none()' --no-tests=pass\n";
const CLEAN: &str = "      - run: cargo llvm-cov clean --profraw-only\n";
const WAIT: &str = "      - run: cargo xtask wait-artifacts --repo r --run-id 1 --job-prefix test-shard --artifact \"profdata-legacy-1=(legacy-1-of-2)\" --artifact \"profdata-legacy-2=(legacy-2-of-2)\" --artifact \"profdata-js-1=(js-1-of-1)\"\n";
const DOWNLOAD: &str = "      - uses: actions/download-artifact@abc # v8\n        with:\n          pattern: profdata-*\n          path: target/shard-profdata\n          merge-multiple: true\n";
const COMPLETE: &str = "      - run: cargo xtask profiles-complete --dir target/shard-profdata --extension profdata --prefix kbf-legacy-1- --prefix kbf-legacy-2- --prefix kbf-js-1-\n";
const MERGE: &str = "      - run: cargo xtask merge-profiles --dir target/shard-profdata --extension profdata --workspace-profile-in target/llvm-cov-target\n";
const REPORT: &str = "      - run: cargo llvm-cov report --branch --lcov\n";
const GATE_STEP: &str = "      - run: cargo run -p kbf-coverage --locked -- --lcov l\n";

fn gate_job(steps: &[&str]) -> String {
    format!("{GATE_HEAD}{}", steps.concat())
}

fn good_steps() -> Vec<&'static str> {
    vec![
        BUILD_LEGACY,
        BUILD_JS,
        CLEAN,
        WAIT,
        DOWNLOAD,
        COMPLETE,
        MERGE,
        REPORT,
        GATE_STEP,
    ]
}

fn workflow(shard: &str, gate: &str) -> String {
    format!("on: push\npermissions: {{}}\njobs:\n{LINT_JOB}{shard}{gate}")
}

fn good() -> String {
    workflow(&shard_job(&good_matrix()), &gate_job(&good_steps()))
}

#[test]
fn the_shape_ci_uses_passes() {
    let w = good();
    assert_eq!(shard_wiring(&w), Ok(()));
    assert_eq!(gate_wiring(&w), Ok(()));
    assert_eq!(check_names(&w), Ok(()));
}

/// A workflow with `from` replaced by `to` in the good one (which must hold `from`).
fn changed(from: &str, to: &str) -> String {
    let g = good();
    assert!(g.contains(from), "{from}");
    g.replacen(from, to, 1)
}

#[test]
fn each_drift_of_the_partition_is_refused() {
    // Catches: each way the matrix stops running every test of every core exactly once.
    let legacy_only =
        |entries: &str| workflow(&shard_job(&matrix(entries)), &gate_job(&good_steps()));
    let js = entry("js", 1, 1, JS_ARGS);
    let l1 = entry("legacy", 1, 2, LEGACY_ARGS);
    let l2 = entry("legacy", 2, 2, LEGACY_ARGS);
    let cases = [
        (
            legacy_only(&format!("{l1}{l2}")),
            "the matrix of `test-shard` has no `js` entry, so that core's tests would not run",
        ),
        (
            legacy_only(&js),
            "the matrix of `test-shard` has no `legacy` entry, so that core's tests would not run",
        ),
        (
            legacy_only(&format!("{l1}{js}")),
            "the `legacy` entries are parts [1] of 2, not each of 1 to 2 once, so some tests would run twice or not at all",
        ),
        (
            legacy_only(&format!("{l1}{l1}{l2}{js}")),
            "the `legacy` entries are parts [1, 1, 2] of 2, not each of 1 to 2 once, so some tests would run twice or not at all",
        ),
        (
            legacy_only(&format!("{l1}{}{js}", entry("legacy", 3, 2, LEGACY_ARGS))),
            "the `legacy` entries are parts [1, 3] of 2, not each of 1 to 2 once, so some tests would run twice or not at all",
        ),
        (
            legacy_only(&format!("{l1}{}{js}", entry("legacy", 2, 3, LEGACY_ARGS))),
            "the `legacy` entries do not all say `parts: 2`, so their parts are of different splits",
        ),
        (
            legacy_only(&format!(
                "{l1}{}{js}",
                entry("legacy", 2, 2, "--workspace --exclude kbf-front")
            )),
            "the `legacy` entries do not run the same packages (`args`), so the parts are not parts of one set of tests",
        ),
        (
            legacy_only(&format!(
                "{}{}{js}",
                entry("legacy", 1, 2, "-p kbf-front"),
                entry("legacy", 2, 2, "-p kbf-front")
            )),
            "the `args` of the `legacy` entries do not contain `--workspace`",
        ),
        (
            legacy_only(&format!("{l1}{l2}{}", entry("js", 1, 1, "-p kbf-server"))),
            "the `args` of the `js` entries do not contain `--features kbf-server/farm-core-js`",
        ),
        (
            legacy_only(&format!(
                "{l1}{l2}{js}{}",
                entry("arm", 1, 1, "--workspace")
            )),
            "a matrix entry names the core Some(\"arm\"), which is neither `legacy` nor `js`",
        ),
        (
            legacy_only(&format!(
                "{l1}{l2}{}",
                js.replace("parts: 1", "parts: many")
            )),
            "a `js` entry has no numeric `parts`",
        ),
        (
            legacy_only(&format!("{l1}{l2}{}", js.replace("part: 1", "part: first"))),
            "a `js` entry has no numeric `part`",
        ),
    ];
    for (w, want) in cases {
        assert_eq!(shard_wiring(&w), Err(want.to_owned()), "{w}");
    }
}

#[test]
fn each_drift_of_a_shard_is_refused() {
    // Catches: each way a shard stops running its own part of its own core, or stops being
    // red when it fails, or leaves profiles that could be another shard's.
    let cases = [
        (
            changed("      PART: ${{ matrix.part }}\n", "      PART: 1\n"),
            "the job `test-shard` does not set `PART: ${{ matrix.part }}`",
        ),
        (
            changed("      PARTS: ${{ matrix.parts }}\n", ""),
            "the job `test-shard` does not set `PARTS: ${{ matrix.parts }}`",
        ),
        (
            changed(
                "      ARGS: ${{ matrix.args }}\n",
                "      ARGS: --workspace\n",
            ),
            "the job `test-shard` does not set `ARGS: ${{ matrix.args }}`",
        ),
        (
            changed(
                "kbf-${{ matrix.core }}-${{ matrix.part }}-%p",
                "kbf-${{ matrix.core }}-%p",
            ),
            "LLVM_PROFILE_FILE_NAME is `kbf-${{ matrix.core }}-%p-%4m.profraw`, which does not begin `kbf-${{ matrix.core }}-${{ matrix.part }}-` (the core and the part) and end `.profraw`, so the shards' profiles could overwrite one another",
        ),
        (
            changed("-%p-%4m.profraw", "-%p-%4m.prof"),
            "LLVM_PROFILE_FILE_NAME is `kbf-${{ matrix.core }}-${{ matrix.part }}-%p-%4m.prof`, which",
        ),
        (
            changed(
                "      - run: cargo llvm-cov nextest $ARGS",
                "      - run: cargo test $ARGS",
            ),
            "the job `test-shard` has no step running `llvm-cov nextest`",
        ),
        (
            changed(" \"--partition=count:$PART/$PARTS\"", ""),
            "the test step of `test-shard` lacks `--partition=count:$PART/$PARTS`",
        ),
        (
            changed(
                " \"--partition=count:$PART/$PARTS\"",
                " --partition=count:1/$PARTS",
            ),
            "the test step of `test-shard` lacks `--partition=count:$PART/$PARTS`",
        ),
        (
            changed("nextest $ARGS", "nextest --workspace"),
            "the test step of `test-shard` lacks `$ARGS`",
        ),
        (
            changed(" --branch", ""),
            "the test step of `test-shard` lacks `--branch`",
        ),
        (
            changed(" --no-report", ""),
            "the test step of `test-shard` lacks `--no-report`",
        ),
        (
            changed(" --no-fail-fast", ""),
            "the test step of `test-shard` lacks `--no-fail-fast`",
        ),
        (
            changed("--profile pr", "--profile ci"),
            "the test step of `test-shard` does not run `--profile pr`",
        ),
        (
            changed(" --profile pr", ""),
            "the test step of `test-shard` does not run `--profile pr`",
        ),
        (
            changed("--profile pr \"", "--profile pr -E 'package(a)' \""),
            "the test step of `test-shard` filters the tests, which would leave some out of every part",
        ),
        (
            changed("--profile pr \"", "--profile pr --filterset=all() \""),
            "the test step of `test-shard` filters the tests, which would leave some out of every part",
        ),
        (
            changed(
                "      - run: cargo llvm-cov nextest $ARGS",
                "      - if: ${{ github.event_name == 'push' }}\n        run: cargo llvm-cov nextest $ARGS",
            ),
            "the test step of `test-shard` has an `if:` or `continue-on-error`, so a shard could pass without running its tests",
        ),
        (
            changed(
                "      - uses: actions/upload-artifact@abc # v7\n",
                "      - uses: actions/upload-artifact@abc # v7\n        if: ${{ always() }}\n",
            ),
            "the last step of `test-shard` must be the upload of its raw profiles with no `if:`, so that a shard has succeeded exactly when its profiles are up",
        ),
        (
            changed(
                "          overwrite: true\n",
                "          overwrite: true\n      - run: echo done\n",
            ),
            "the last step of `test-shard` must be the upload of its raw profiles with no `if:`, so that a shard has succeeded exactly when its profiles are up",
        ),
        (
            changed(
                "xtask merge-profiles --dir target/llvm-cov-target",
                "xtask other --dir target/llvm-cov-target",
            ),
            "the job `test-shard` has no step `cargo xtask merge-profiles`",
        ),
        (
            changed(
                "--extension profraw --prefix",
                "--extension profdata --prefix",
            ),
            "the merge step of `test-shard` must merge the shard's own",
        ),
        (
            changed(
                "--prefix kbf-${{ matrix.core }}-${{ matrix.part }}- --out",
                "--prefix kbf- --out",
            ),
            "the merge step of `test-shard` must merge the shard's own",
        ),
        (
            changed(
                "-merged.profdata\n      - uses",
                "-other.profdata\n      - uses",
            ),
            "the merge step of `test-shard` must merge the shard's own",
        ),
        (
            changed(
                "      - run: cargo xtask merge-profiles",
                "      - if: ${{ always() }}\n        run: cargo xtask merge-profiles",
            ),
            "the merge step of `test-shard` has an `if:` or `continue-on-error`",
        ),
        (
            changed(
                "name: profdata-${{ matrix.core }}-${{ matrix.part }}",
                "name: profdata-${{ matrix.core }}",
            ),
            "the upload step's `name` is not `profdata-${{ matrix.core }}-${{ matrix.part }}`",
        ),
        (
            changed(
                "path: target/llvm-cov-target/kbf-${{ matrix.core }}-${{ matrix.part }}-merged.profdata",
                "path: target/llvm-cov-target/*.profdata",
            ),
            "the upload step's `path` is not the shard's own merged profile, `target/llvm-cov-target/kbf-${{ matrix.core }}-${{ matrix.part }}-merged.profdata`",
        ),
        (
            changed("          overwrite: true\n", ""),
            "the upload step lacks `overwrite: true`, so a re-run of the run fails on the names its first run uploaded",
        ),
        (
            changed("          if-no-files-found: error\n", ""),
            "the upload step lacks `if-no-files-found: error`, so a shard with no profile would pass",
        ),
        // A shard with an empty part passes under `--no-tests=pass`.
        (
            changed(" --no-fail-fast", " --no-fail-fast --no-tests=pass"),
            "the test step of `test-shard` passes when no test runs (`--no-tests`)",
        ),
        (
            changed(" --no-fail-fast", " --no-tests=pass --no-fail-fast"),
            "the test step of `test-shard` passes when no test runs (`--no-tests`)",
        ),
        (
            changed(" --no-fail-fast", " --no-fail-fast --no-tests pass"),
            "the test step of `test-shard` passes when no test runs (`--no-tests`)",
        ),
        (
            changed(" --no-fail-fast", " --no-fail-fast --no-tests=warn"),
            "the test step of `test-shard` passes when no test runs (`--no-tests`)",
        ),
        (
            changed(
                " --no-fail-fast",
                " --no-fail-fast --no-tests=fail --no-tests=pass",
            ),
            "the test step of `test-shard` passes when no test runs (`--no-tests`)",
        ),
        // The job's name is what `test` finds it by.
        (
            changed(
                "    name: test-shard (${{ matrix.core }}-${{ matrix.part }}-of-${{ matrix.parts }})\n",
                "",
            ),
            "the job `test-shard` is not named `test-shard (${{ matrix.core }}-${{ matrix.part }}-of-${{ matrix.parts }})`",
        ),
        (
            changed(
                "(${{ matrix.core }}-${{ matrix.part }}-of-${{ matrix.parts }})\n    runs-on",
                "(${{ matrix.core }} ${{ matrix.part }}/${{ matrix.parts }})\n    runs-on",
            ),
            "the job `test-shard` is not named",
        ),
        // The early disk check uses the matrix's number.
        (
            changed("      MIN_FREE_GB: ${{ matrix.min_free_gb }}\n", ""),
            "the job `test-shard` does not set `MIN_FREE_GB: ${{ matrix.min_free_gb }}`",
        ),
        (
            changed(
                "      - run: cargo xtask runner-disk --path . --min-free-gb \"$MIN_FREE_GB\"\n",
                "",
            ),
            "the job `test-shard` has no unconditional step `xtask runner-disk",
        ),
        (
            changed("--min-free-gb \"$MIN_FREE_GB\"", "--min-free-gb 10"),
            "the job `test-shard` has no unconditional step `xtask runner-disk",
        ),
        (
            changed(
                "      - run: cargo xtask runner-disk",
                "      - if: ${{ github.event_name == 'push' }}\n        run: cargo xtask runner-disk",
            ),
            "the job `test-shard` has no unconditional step `xtask runner-disk",
        ),
    ];
    for (w, want) in cases {
        let got = shard_wiring(&w);
        assert!(
            got.as_ref().is_err_and(|e| e.starts_with(want)),
            "want `{want}`, got {got:?}:\n{w}"
        );
    }
}

#[test]
fn a_shard_with_too_little_disk_is_refused() {
    // Catches: an early disk check set below what the core's build and tests measured (legacy
    // about 67 GB, js about 50), which passes a runner that then fails tests for want of room;
    // a number that is missing or is not one.
    let l2 = entry("legacy", 2, 2, LEGACY_ARGS);
    let js = entry("js", 1, 1, JS_ARGS);
    let with = |l1: String, js: String| {
        workflow(
            &shard_job(&matrix(&format!("{l1}{l2}{js}"))),
            &gate_job(&good_steps()),
        )
    };
    let l1 = |gb: &str| entry_with_disk("legacy", 1, 2, LEGACY_ARGS, gb);
    let js_gb = |gb: &str| entry_with_disk("js", 1, 1, JS_ARGS, gb);
    let cases = [
        (
            with(l1("39"), js.clone()),
            "a `legacy` entry says `min_free_gb: 39`, under the 40",
        ),
        (
            with(l1("30"), js.clone()),
            "a `legacy` entry says `min_free_gb: 30`, under the 40",
        ),
        (
            with(l1("40"), js_gb("34")),
            "a `js` entry says `min_free_gb: 34`, under the 35",
        ),
        (
            with(l1("20"), js_gb("40")),
            "a `js` entry says `min_free_gb: 20`, under the 35",
        ),
        (
            with(l1("lots"), js.clone()),
            "a `legacy` entry has no numeric `min_free_gb`",
        ),
        (
            with(
                l1("40").replace("            min_free_gb: 40\n", ""),
                js.clone(),
            ),
            "a `legacy` entry has no numeric `min_free_gb`",
        ),
    ];
    for (w, want) in cases {
        let got = shard_wiring(&w);
        assert!(
            got.as_ref().is_err_and(|e| e.starts_with(want)),
            "want `{want}`, got {got:?}"
        );
    }
    // The measured numbers and more pass.
    assert_eq!(shard_wiring(&with(l1("40"), js_gb("35"))), Ok(()));
    assert_eq!(shard_wiring(&with(l1("120"), js_gb("90"))), Ok(()));
}

#[test]
fn a_shard_may_say_no_tests_fail() {
    // `--no-tests=fail` is nextest's own default: it is allowed, not required.
    let w = changed(" --no-fail-fast", " --no-fail-fast --no-tests=fail");
    assert_eq!(shard_wiring(&w), Ok(()));
}

#[test]
fn each_drift_of_the_gate_is_refused() {
    // Catches: each way `test` stops reading every shard's profiles, in the order that makes
    // one whole report, with the binaries of both cores.
    let without = |i: usize| {
        let mut steps = good_steps();
        steps.remove(i);
        workflow(&shard_job(&good_matrix()), &gate_job(&steps))
    };
    let order = |a: usize, b: usize| {
        let mut steps = good_steps();
        steps.swap(a, b);
        workflow(&shard_job(&good_matrix()), &gate_job(&steps))
    };
    let conditioned = |step: &str| {
        let with = step.replacen(
            "      - ",
            "      - if: ${{ github.event_name == 'push' }}\n        ",
            1,
        );
        changed(step, &with)
    };
    let cases = [
        (
            without(0),
            "the job `test` has no step that builds the instrumented tests of the `legacy` core (`llvm-cov nextest --workspace ... --no-tests=pass`)",
        ),
        (
            without(1),
            "the job `test` has no step that builds the instrumented tests of the `js` core (`llvm-cov nextest -p kbf-server --features kbf-server/farm-core-js ... --no-tests=pass`)",
        ),
        (
            changed(
                " --no-tests=pass\n      - run: cargo llvm-cov nextest -p",
                "\n      - run: cargo llvm-cov nextest -p",
            ),
            "the job `test` has no step that builds the instrumented tests of the `legacy` core",
        ),
        (
            without(2),
            "the job `test` has no step that drops the profiles the builds' test listing wrote (`llvm-cov clean --profraw-only`), which would be merged into the report",
        ),
        (
            without(3),
            "the job `test` has no step running `cargo xtask wait-artifacts`",
        ),
        (
            without(4),
            "the job `test` has no step that downloads the shards' raw profiles",
        ),
        (
            without(5),
            "the job `test` has no step running `cargo xtask profiles-complete`",
        ),
        (
            without(6),
            "the job `test` has no step running `cargo xtask merge-profiles` on the shards' profiles",
        ),
        (
            without(7),
            "the job `test` has no step running `llvm-cov report`",
        ),
        (
            without(8),
            "the job `test` has no step running the gate (`-p kbf-coverage`)",
        ),
        (
            order(2, 4),
            "the steps of `test` are not in the order build, drop listing profiles (before the download), wait, download, check the profiles, merge them, report, gate",
        ),
        (order(3, 4), "the steps of `test` are not in the order"),
        (order(4, 5), "the steps of `test` are not in the order"),
        (order(5, 6), "the steps of `test` are not in the order"),
        (order(6, 7), "the steps of `test` are not in the order"),
        (order(7, 8), "the steps of `test` are not in the order"),
        (order(1, 2), "the steps of `test` are not in the order"),
        (
            conditioned(BUILD_JS),
            "the build step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(CLEAN),
            "the profile-dropping step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(WAIT),
            "the wait step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(DOWNLOAD),
            "the download step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(COMPLETE),
            "the profiles-complete step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(MERGE),
            "the profile-merging step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            conditioned(REPORT),
            "the report step of `test` has an `if:` or `continue-on-error`, so the gate could pass without it",
        ),
        (
            changed(" --job-prefix test-shard", ""),
            "the wait step does not pass `--job-prefix test-shard`, so a failed shard would not end it",
        ),
        (
            changed(" --artifact \"profdata-js-1=(js-1-of-1)\"", ""),
            "the wait step's `--artifact` names are not the shards' artifacts",
        ),
        (
            changed(
                " --artifact \"profdata-js-1=(js-1-of-1)\"",
                " --artifact \"profdata-js-1=(js-1-of-1)\" --artifact \"profdata-js-2=(js-2-of-1)\"",
            ),
            "the wait step's `--artifact` names are not the shards' artifacts",
        ),
        (
            changed(
                " --artifact \"profdata-js-1=(js-1-of-1)\"",
                " --artifact \"profdata-js-1=(js-1-of-1)\" --artifact \"profdata-js-1=(js-1-of-1)\"",
            ),
            "the wait step's `--artifact` names are not the shards' artifacts",
        ),
        // An artifact asked for by its name alone would count whatever attempt made it.
        (
            changed("\"profdata-js-1=(js-1-of-1)\"", "profdata-js-1"),
            "the wait step's `--artifact` names are not the shards' artifacts",
        ),
        // Or for the wrong shard's job: the key must be that of the artifact's own shard.
        (
            changed(
                "\"profdata-js-1=(js-1-of-1)\"",
                "\"profdata-js-1=(legacy-1-of-2)\"",
            ),
            "the wait step's `--artifact` names are not the shards' artifacts",
        ),
        (
            changed("pattern: profdata-*", "pattern: other-*"),
            "the download step's `pattern` is not `profdata-*`",
        ),
        (
            changed(
                "path: target/shard-profdata\n          merge",
                "path: elsewhere\n          merge",
            ),
            "the download step must put the profiles flat in `target/shard-profdata` (`merge-multiple: true`)",
        ),
        (
            changed("merge-multiple: true", "merge-multiple: false"),
            "the download step must put the profiles flat in `target/shard-profdata` (`merge-multiple: true`)",
        ),
        (
            changed(
                "profiles-complete --dir target/shard-profdata",
                "profiles-complete --dir elsewhere",
            ),
            "the profiles-complete step does not check `--dir target/shard-profdata --extension profdata`",
        ),
        (
            changed(
                "--workspace-profile-in target/llvm-cov-target",
                "--workspace-profile-in elsewhere",
            ),
            "the merge step of `test` must merge the `.profdata` files of `target/shard-profdata` into the profile `llvm-cov report` reads",
        ),
        (
            changed(
                "merge-profiles --dir target/shard-profdata",
                "merge-profiles --dir elsewhere",
            ),
            "the merge step of `test` must merge the `.profdata` files of `target/shard-profdata` into the profile `llvm-cov report` reads",
        ),
        (
            changed(" --prefix kbf-js-1-", ""),
            "the profiles-complete step's `--prefix` values are not the shards' file prefixes",
        ),
        (
            changed(" --prefix kbf-js-1-", " --prefix kbf-js-2-"),
            "the profiles-complete step's `--prefix` values are not the shards' file prefixes",
        ),
    ];
    for (w, want) in cases {
        let got = gate_wiring(&w);
        assert!(
            got.as_ref().is_err_and(|e| e.starts_with(want)),
            "want `{want}`, got {got:?}:\n{w}"
        );
    }
    // A workflow whose matrix has no entry for a core has nothing to build for it.
    let no_args = workflow(
        &shard_job(&matrix(&entry("legacy", 1, 1, LEGACY_ARGS))),
        &gate_job(&good_steps()),
    );
    assert_eq!(
        gate_wiring(&no_args),
        Err("the matrix has no `args` for the `js` core".to_owned())
    );
}

#[test]
fn each_drift_of_the_check_names_is_refused() {
    // Catches: the required checks renamed, missing or skippable, and `test` without the scope
    // that lets it wait for the shards.
    let cases = [
        (
            good().replace("  lint:\n", "  lints:\n"),
            "the workflow has no job `lint`, a required check",
        ),
        (
            good().replace("  test:\n", "  tests:\n"),
            "the workflow has no job `test`, a required check",
        ),
        (
            good().replace("  lint:\n", "  lint:\n    name: format\n"),
            "the job `lint` has a `name:`, so its check is no longer called `lint`",
        ),
        (
            good().replace("  test:\n", "  test:\n    name: coverage\n"),
            "the job `test` has a `name:`, so its check is no longer called `test`",
        ),
        (
            good().replace("  test:\n", "  test:\n    if: ${{ !cancelled() }}\n"),
            "the job `test` has an `if:`, so it can be skipped, and a skipped required check passes",
        ),
        (
            good().replace("      actions: read\n", ""),
            "the job `test` lacks `actions: read`",
        ),
        (
            good().replace("      actions: read\n", "      actions: none\n"),
            "the job `test` lacks `actions: read`",
        ),
    ];
    for (w, want) in cases {
        let got = check_names(&w);
        assert!(
            got.as_ref().is_err_and(|e| e.starts_with(want)),
            "want `{want}`, got {got:?}"
        );
    }
}
