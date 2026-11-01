//! Tests of the seed-build lint. The real workflows are the control (no problem); each other
//! test spoils one copy of them and names the rule it catches. A spoil must change the text,
//! and it is made inside the one job it names, so a rule whose anchor text moved, or that
//! only read another job, cannot go quiet.

use super::*;

const ARTIFACTS: &str = ".github/workflows/artifacts.yml";
const PIPELINE: &str = ".github/workflows/pipeline.yml";
const RELEASE: &str = ".github/workflows/release.yml";

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// The real workflows, as owned texts a test may spoil.
struct Real {
    artifacts: String,
    pipeline: String,
    release: String,
    called: Vec<(String, String)>,
}

fn real() -> Real {
    let pipeline = read(PIPELINE);
    Real {
        artifacts: read(ARTIFACTS),
        called: called(&crate::repo_root(), &pipeline),
        pipeline,
        release: read(RELEASE),
    }
}

impl Real {
    fn problems(&self) -> Vec<String> {
        problems(&Workflows {
            artifacts: &self.artifacts,
            pipeline: &self.pipeline,
            release: &self.release,
            called: self.called.clone(),
        })
        .unwrap_or_else(|e| vec![e])
    }
}

/// The lines of the job `job` of `text`: its header line to the next job's, comments included.
fn job_span(text: &str, job: &str) -> (Vec<String>, usize, usize) {
    let header = format!("  {job}:");
    let all: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let start = all
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("no job `{job}`"));
    let end = all[start + 1..]
        .iter()
        .position(|l| l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().ends_with(':'))
        .map_or(all.len(), |n| start + 1 + n);
    (all, start, end)
}

/// `text` with `from` replaced by `to` inside the job `job` only, which must hold `from`
/// exactly once.
fn in_job(text: &str, job: &str, from: &str, to: &str) -> String {
    let (all, start, end) = job_span(text, job);
    let region = all[start..end].concat();
    assert_eq!(
        region.matches(from).count(),
        1,
        "job `{job}` holds `{from}` {} times",
        region.matches(from).count()
    );
    [
        all[..start].concat(),
        region.replace(from, to),
        all[end..].concat(),
    ]
    .concat()
}

/// `text` without the job `job`.
fn without_job(text: &str, job: &str) -> String {
    let (all, start, end) = job_span(text, job);
    [all[..start].concat(), all[end..].concat()].concat()
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

/// The problems of the real workflows with `from` replaced by `to` in the job `job` of
/// artifacts.yml.
fn spoil_artifacts(job: &str, from: &str, to: &str) -> Vec<String> {
    let mut w = real();
    w.artifacts = in_job(&w.artifacts, job, from, to);
    w.problems()
}

/// The `cargo build` of `kbf` in each seed job, as the real artifacts.yml writes it.
const KBF_BUILDS: &[(&str, &str)] = &[
    (
        "darwin-arm64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-cli --bin kbf\n",
    ),
    (
        "linux-x86_64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-node -p kbf-server -p kbf-cli --bins\n",
    ),
    (
        "linux-arm64",
        "- run: cargo build --locked --release --target \"$TARGET\" -p kbf-node -p kbf-server -p kbf-cli --bins\n",
    ),
];

/// The `runs-on` line of each seed job.
const RUNNERS: &[(&str, &str)] = &[
    ("darwin-arm64", "    runs-on: macos-15\n"),
    ("linux-x86_64", "    runs-on: ubuntu-24.04\n"),
    ("linux-arm64", "    runs-on: ubuntu-24.04-arm\n"),
];

#[test]
fn the_real_workflows_meet_every_rule() {
    // Catches: the control for every spoil below failing on the real workflows.
    let found = real().problems();
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn a_removed_seed_job_is_refused() {
    // Catches: the cargo release job of a platform removed from artifacts.yml (or renamed,
    // which removes it as far as the release's needs are concerned).
    for job in SEED_JOBS {
        let mut w = real();
        w.artifacts = without_job(&w.artifacts, job);
        red(&w.problems(), &format!("has no seed job `{job}`"));
        let mut w = real();
        w.artifacts = in_job(
            &w.artifacts,
            job,
            &format!("  {job}:\n"),
            &format!("  {job}-gone:\n"),
        );
        red(&w.problems(), &format!("has no seed job `{job}`"));
    }
}

#[test]
fn a_seed_job_off_the_hosted_runners_is_refused() {
    // Catches: a seed job moved to a self-hosted runner, a custom label (a kbf farm's
    // runners), a runner group, an expression or a second label.
    for (job, line) in RUNNERS {
        for to in [
            "    runs-on: self-hosted\n",
            "    runs-on: [self-hosted, linux, x64]\n",
            "    runs-on: kbf-farm\n",
            "    runs-on:\n      group: kbf-builders\n",
            "    runs-on:\n      group: kbf-builders\n      labels: ubuntu-24.04\n",
            "    runs-on: ${{ vars.RELEASE_RUNNER }}\n",
            "    runs-on: [ubuntu-24.04, kbf]\n",
        ] {
            let found = spoil_artifacts(job, line, to);
            red(&found, &format!("the seed job `{job}`"));
            assert!(
                found.iter().any(|p| p.contains("hosted runner")),
                "{job} with {to:?}: {found:#?}"
            );
        }
    }
}

#[test]
fn a_seed_job_that_calls_another_workflow_is_refused() {
    // Catches: a seed job turned into a call of another workflow (a kbf farm's build), whose
    // runners and build this lint never reads.
    for (job, line) in RUNNERS {
        red(
            &spoil_artifacts(job, line, "    uses: ./.github/workflows/farm-build.yml\n"),
            &format!("the seed job `{job}` calls another workflow"),
        );
    }
}

#[test]
fn buck2_in_a_seed_job_is_refused() {
    // Catches: the kbf build of a seed job replaced by a Buck2 or Bazel build, or Buck2
    // fetched and run beside it in the same job.
    for (job, build) in KBF_BUILDS {
        for to in [
            "- run: buck2 build //crates/kbf-cli:kbf\n".to_owned(),
            "- run: bazel build //crates/kbf-cli:kbf\n".to_owned(),
            format!("- run: $RUNNER_TEMP/tools/buck2 build //...\n      {build}"),
            format!(
                "- uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        with:\n          path: buck-out\n          key: x\n      {build}"
            ),
        ] {
            let found = spoil_artifacts(job, build, &to);
            assert!(
                found
                    .iter()
                    .any(|p| p.contains(&format!("the seed job `{job}` names `"))),
                "{job} with {to:?}: {found:#?}"
            );
        }
        // With the build gone, the job no longer builds kbf with cargo either.
        red(
            &spoil_artifacts(job, build, "- run: buck2 build //crates/kbf-cli:kbf\n"),
            &format!("the seed job `{job}` has no unconditional plain"),
        );
    }
}

#[test]
fn a_farm_or_remote_execution_flag_in_a_seed_job_is_refused() {
    // Catches: a seed job's cargo pointed at a kbf farm: a remote executor or cache, a gRPC
    // farm address, a rustc wrapper (sccache, or one set by `--config` or a variable), in
    // the build line, the step's or the job's `env:`, or the workflow's.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for to in [
            format!("- run: {line} --config build.rustc-wrapper=kbf-rustc\n"),
            format!("- run: RUSTC_WRAPPER=sccache {line}\n"),
            format!("- run: CARGO_BUILD_RUSTC=/opt/kbf/rustc {line}\n"),
            format!("- run: {line} -Z remote-executor\n"),
            format!("- run: {line} --remote_executor=grpc://farm.example.com:8980\n"),
            format!(
                "- env:\n          KBF_FRONTEND: grpcs://farm.example.com:443\n        run: {line}\n"
            ),
            format!("- env:\n          SCCACHE_BUCKET: kbf-cache\n        run: {line}\n"),
        ] {
            let found = spoil_artifacts(job, build, &to);
            red(&found, &format!("the seed job `{job}`"));
        }
    }
    let mut w = real();
    w.artifacts = w.artifacts.replacen(
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  RUSTC_WRAPPER: sccache\n",
        1,
    );
    assert_ne!(w.artifacts, read(ARTIFACTS), "the workflow env is spoiled");
    for job in SEED_JOBS {
        red(
            &w.problems(),
            &format!("the seed job `{job}` names `rustc_wrapper`"),
        );
    }
    let mut w = real();
    w.artifacts = in_job(
        &w.artifacts,
        "linux-x86_64",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: ubuntu-24.04\n    env:\n      RUSTC_WRAPPER: /opt/kbf/rustc-remote\n",
    );
    red(
        &w.problems(),
        "the seed job `linux-x86_64` names `rustc_wrapper`",
    );
}

#[test]
fn a_build_that_is_not_plain_cargo_is_refused() {
    // Catches: a seed job's kbf built without `--locked` or `--release`, with another verb
    // or toolchain, under a shell of its own, or in a step that can be skipped.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for (to, want) in [
            (line.replace(" --locked", ""), "do not follow `cargo build`"),
            (
                line.replace(" --release", ""),
                "do not follow `cargo build`",
            ),
            (
                line.replace("cargo build", "cargo +nightly build"),
                "compiles with `cargo +nightly`",
            ),
            (
                line.replace("cargo build", "cargo b"),
                "compiles with `cargo b`",
            ),
            (
                format!("{line} --features remote"),
                "it passes `--features`",
            ),
        ] {
            let found = spoil_artifacts(job, build, &format!("- run: {to}\n"));
            red(&found, want);
        }
        red(
            &spoil_artifacts(
                job,
                build,
                &format!("- shell: kbf-shell {{0}}\n        run: {line}\n"),
            ),
            "under a `shell:` of its own",
        );
        red(
            &spoil_artifacts(
                job,
                build,
                &format!("- if: ${{{{ false }}}}\n        run: {line}\n"),
            ),
            &format!("the seed job `{job}` has no unconditional plain"),
        );
    }
}

#[test]
fn a_build_line_is_read_word_by_word() {
    // Catches: the reader of a build line taking a non-plain build for a plain one, or a
    // line that compiles nothing for a build.
    assert_eq!(
        build_line(
            r#"KBF_BUILD_COMMIT="${GITHUB_SHA::12}" cargo build --locked --release --target "$TARGET" -p kbf-deploy --bins"#
        ),
        Build::Plain(vec!["kbf-deploy"])
    );
    assert_eq!(build_line("cargo xtask package dist kbf"), Build::None);
    assert_eq!(build_line("cargo cyclonedx --format json"), Build::None);
    assert_eq!(
        build_line("cargo build --locked --release -p"),
        Build::NotPlain("`-p` has no value".to_owned())
    );
    assert_eq!(
        build_line("cargo build --locked --release -p --bins"),
        Build::NotPlain("`-p` has no value".to_owned())
    );
    assert_eq!(
        build_line("cargo rustc --locked --release -p kbf-cli"),
        Build::NotPlain("it compiles with `cargo rustc`".to_owned())
    );
    assert_eq!(
        build_line("RUSTFLAGS=-Cx cargo build --locked --release"),
        Build::NotPlain("it sets `RUSTFLAGS`".to_owned())
    );
}

#[test]
fn published_files_from_another_job_are_refused() {
    // Catches: a seed job's upload removed, renamed or made skippable (so the release takes
    // files from elsewhere or none), another job of the pipeline's run uploading an assets
    // artifact (by name or by an expression), and an attest job or the release's verify no
    // longer reading the seed job's files.
    for job in SEED_JOBS {
        let upload = format!("name: assets-{job}\n");
        red(
            &spoil_artifacts(job, &upload, &format!("name: built-{job}\n")),
            &format!("does not upload its files as `assets-{job}`"),
        );
        let uses =
            "- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n";
        red(
            &spoil_artifacts(
                job,
                uses,
                &format!("- if: ${{{{ false }}}}\n        uses: {}", &uses[8..]),
            ),
            &format!("does not upload its files as `assets-{job}`"),
        );
    }
    let upload_step = |name: &str| {
        format!(
            "      - run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: {name}\n          path: dist/\n"
        )
    };
    for name in [
        "assets-linux-x86_64-farm",
        "ASSETS-kbf",
        "${{ inputs.stage }}-files",
        "asset${{ inputs.stage }}",
    ] {
        let from = "      - name: Assemble SHA256SUMS\n        run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n";
        red(
            &spoil_artifacts(ALL_SUMS, from, &upload_step(name)),
            &format!("the job `sums` of artifacts.yml uploads `{name}`"),
        );
    }
    let mut w = real();
    let (path, text) = w
        .called
        .iter_mut()
        .find(|(p, _)| p.ends_with("smoke.yml"))
        .expect("the pipeline calls smoke.yml");
    let spoiled = text.replacen(
        "name: beta-b0-${{ github.run_attempt }}",
        "name: assets-darwin-arm64-b0",
        1,
    );
    assert_ne!(*text, spoiled, "smoke.yml is spoiled");
    *text = spoiled;
    let path = path.clone();
    red(
        &w.problems(),
        &format!("of {path} uploads `assets-darwin-arm64-b0`"),
    );
    let mut w = real();
    w.pipeline = in_job(
        &w.pipeline,
        PASSED,
        "    steps:\n",
        "    steps:\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: assets-extra\n          path: x\n",
    );
    red(
        &w.problems(),
        "the job `passed` of pipeline.yml uploads `assets-extra`",
    );

    red(
        &spoil_artifacts(
            "attest-linux",
            "pattern: assets-linux-*\n",
            "pattern: assets-linux-x86_64\n",
        ),
        "the attest job `attest-linux` does not download `assets-linux-arm64`",
    );
    red(
        &spoil_artifacts(
            "attest-darwin-arm64",
            "pattern: assets-darwin-arm64\n",
            "pattern: farm-darwin-arm64\n",
        ),
        "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
    );
    for (from, to) in [
        ("pattern: assets-*\n", "pattern: farm-*\n"),
        ("pattern: assets-*\n", "pattern: assets-linux-*\n"),
        (
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: ${{ github.event.workflow_run.id }}\n",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: ${{ vars.FARM_RUN }}\n",
        ),
    ] {
        let mut w = real();
        w.release = in_job(&w.release, RELEASE_VERIFY, from, to);
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-darwin-arm64`",
        );
    }
}

#[test]
fn a_release_that_no_longer_needs_the_seed_jobs_is_refused() {
    // Catches: publish no longer needing verify; the pipeline's sign call or `passed` (whose
    // success starts the release) no longer needing the build call, or the calls naming
    // another workflow or stage; a sums job no longer needing a seed job (so the attested
    // sums need not cover its files); a verify job no longer needing its attest job.
    let mut w = real();
    w.release = in_job(
        &w.release,
        RELEASE_PUBLISH,
        "needs: [plan, verify, tag, notes]",
        "needs: [plan, tag, notes]",
    );
    red(
        &w.problems(),
        "the job `publish` of release.yml does not need `verify`",
    );

    for (job, from, to, want) in [
        (
            SIGN_CALL,
            "needs: [ci, artifacts]",
            "needs: [ci]",
            "the job `artifacts-sign` of pipeline.yml does not need `artifacts`",
        ),
        (
            PASSED,
            "needs: [ci, beta, artifacts, artifacts-sign, smoke]",
            "needs: [ci, beta, artifacts-sign, smoke]",
            "the job `passed` of pipeline.yml does not need `artifacts`",
        ),
        (
            PASSED,
            "needs: [ci, beta, artifacts, artifacts-sign, smoke]",
            "needs: [ci, beta, artifacts, smoke]",
            "the job `passed` of pipeline.yml does not need `artifacts-sign`",
        ),
        (
            BUILD_CALL,
            "stage: build",
            "stage: farm",
            "the pipeline's `artifacts` is not the `build` stage",
        ),
        (
            SIGN_CALL,
            "uses: ./.github/workflows/artifacts.yml",
            "uses: ./.github/workflows/ci.yml",
            "the pipeline's `artifacts-sign` is not the `sign` stage",
        ),
    ] {
        let mut w = real();
        w.pipeline = in_job(&w.pipeline, job, from, to);
        red(&w.problems(), want);
    }

    for (job, from, to, want) in [
        (
            "sums-linux",
            "needs: [linux-x86_64, linux-arm64]",
            "needs: [linux-x86_64]",
            "the job `sums-linux` of artifacts.yml does not need `linux-arm64`",
        ),
        (
            "sums-darwin-arm64",
            "needs: [darwin-arm64]",
            "needs: []",
            "the job `sums-darwin-arm64` of artifacts.yml does not need `darwin-arm64`",
        ),
        (
            "sums",
            "needs: [darwin-arm64, linux-x86_64, linux-arm64]",
            "needs: [darwin-arm64, linux-arm64]",
            "the job `sums` of artifacts.yml does not need `linux-x86_64`",
        ),
        (
            "verify-linux",
            "needs: [attest-linux]",
            "needs: []",
            "the job `verify-linux` of artifacts.yml does not need `attest-linux`",
        ),
    ] {
        red(&spoil_artifacts(job, from, to), want);
    }
}

#[test]
fn artifact_patterns_match_as_the_download_action_does() {
    // Catches: the pattern match taking a name a download would not read for one it does,
    // or the reverse, so a rule about who reads the seed files passes on a wrong reader.
    assert!(matches("assets-*", "assets-linux-x86_64"));
    assert!(matches("assets-linux-*", "ASSETS-linux-arm64"));
    assert!(matches("assets-darwin-arm64", "assets-darwin-arm64"));
    assert!(matches("*-arm64", "assets-darwin-arm64"));
    assert!(matches("a*s*4", "assets-linux-arm64"));
    assert!(!matches("assets-linux-*", "assets-darwin-arm64"));
    assert!(!matches("assets-darwin-arm64", "assets-darwin-arm64x"));
    assert!(!matches("*-x86_64", "assets-linux-arm64"));
    assert!(!matches("a*rm*rm64", "assets-darwin-arm64"));
    assert!(may_be_assets("assets-linux-x86_64"));
    assert!(may_be_assets("${{ env.NAME }}"));
    assert!(may_be_assets("ass${{ env.NAME }}"));
    assert!(may_be_assets("assets-${{ env.NAME }}"));
    assert!(!may_be_assets("profdata-${{ matrix.core }}"));
    assert!(!may_be_assets("sha256sums-linux"));
}
