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
            format!("- run: {line} --remote_executor=grpc://\n"),
            format!("- env:\n          KBF_FRONTEND: grpcs://\n        run: {line}\n"),
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
            &format!("the seed job `{job}` names `rustc`"),
        );
    }
    let mut w = real();
    w.artifacts = in_job(
        &w.artifacts,
        "linux-x86_64",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: ubuntu-24.04\n    env:\n      RUSTC_WRAPPER: /opt/kbf/rustc-remote\n",
    );
    red(&w.problems(), "the seed job `linux-x86_64` names `rustc`");
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
        Build::Plain(Built {
            packages: vec!["kbf-deploy"],
            bins: vec![],
            all_bins: true
        })
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
    assert!(!matches("assets-linux-*", "ASSETS-linux-arm64"));
    assert!(!matches("Assets-*", "assets-linux-arm64"));
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

/// The problems of the real workflows with `to` in place of each seed job's kbf build, `{build}`
/// in `to` standing for that build's step.
fn around_kbf_build(job: &str, build: &str, to: &str) -> Vec<String> {
    spoil_artifacts(job, build, &to.replace("{build}", build))
}

#[test]
fn another_rustc_in_a_seed_job_is_refused() {
    // Catches: a seed job's cargo given another compiler, a shim that can hand its work to a
    // farm, by `RUSTC` or `CARGO_BUILD_RUSTC` in a step's `env:`, or written to `$GITHUB_ENV`
    // by an earlier step.
    for (job, build) in KBF_BUILDS {
        for to in [
            "- env:\n          RUSTC: /opt/kbf/remote\n        run: {line}\n",
            "- env:\n          CARGO_BUILD_RUSTC: /opt/kbf/remote\n        run: {line}\n",
            "- run: echo \"RUSTC=/opt/kbf/remote\" >> \"$GITHUB_ENV\"\n      {build}",
        ] {
            let line = build.trim_start_matches("- run: ").trim_end();
            let found = around_kbf_build(job, build, &to.replace("{line}", line));
            red(&found, &format!("the seed job `{job}` names `rustc`"));
        }
    }
}

#[test]
fn every_word_a_seed_job_may_not_name_is_refused_alone() {
    // Catches: an entry of `NOT_PLAIN` that no longer refuses the scalar holding it (each
    // other spoil may be caught by another rule first).
    for word in NOT_PLAIN {
        let to = format!("- env:\n          SEED_SPOIL: \"x-{word}-y\"\n        {{build}}");
        let (job, build) = KBF_BUILDS[1];
        let to = to.replace("{build}", &build.replacen("- run:", "run:", 1));
        red(
            &spoil_artifacts(job, build, &to),
            &format!("the seed job `{job}` names `{word}`"),
        );
    }
}

#[test]
fn a_step_named_after_a_bucket_is_not_refused() {
    // Catches: a refused word matching inside an ordinary one (`buck` in `bucket`), which
    // would refuse an innocent edit of a seed job.
    for (job, build) in KBF_BUILDS {
        let found = spoil_artifacts(
            job,
            build,
            &format!("- name: Copy to the bucket\n        {}", &build[2..]),
        );
        assert!(found.is_empty(), "{job}: {found:#?}");
    }
}

#[test]
fn a_cargo_option_before_the_verb_is_refused() {
    // Catches: `cargo --config ... build` or `cargo -Z ... build`, whose first word after
    // `cargo` is not the verb, read as a line that builds nothing.
    for (job, build) in KBF_BUILDS {
        for to in [
            build.replace("cargo build", "cargo --config build.jobs=1 build"),
            build.replace("cargo build", "cargo -Zunstable-options build"),
        ] {
            red(&spoil_artifacts(job, build, &to), "comes before the verb");
        }
    }
    assert_eq!(build_line("cargo --version"), Build::None);
}

#[test]
fn a_seed_job_that_takes_anything_but_the_checkout_is_refused() {
    // Catches: a seed job given `needs:` (another job's outputs), an artifact download (files
    // built elsewhere swapped in before the upload), a local action this lint never reads
    // or `defaults:` of its own (a shell that runs the build somewhere else).
    for (job, line) in RUNNERS {
        for (to, want) in [
            (format!("{line}    needs: [sums]\n"), "needs another job"),
            (
                format!("{line}    defaults:\n      run:\n        shell: farm-shell {{0}}\n"),
                "has `defaults:` of its own",
            ),
        ] {
            red(&spoil_artifacts(job, line, &to), want);
        }
    }
    for (job, build) in KBF_BUILDS {
        red(
            &around_kbf_build(
                job,
                build,
                "- uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: farm-kbf\n      {build}",
            ),
            &format!("the seed job `{job}` downloads an artifact"),
        );
        red(
            &around_kbf_build(
                job,
                build,
                "- uses: ./.github/actions/farm-build\n      {build}",
            ),
            &format!("the seed job `{job}` runs the local action"),
        );
    }
}

#[test]
fn uploads_and_downloads_in_every_spelling_are_read() {
    // Catches: an assets artifact uploaded by a sub-action (`upload-artifact/merge`) or under
    // a name with blanks around it; an attest download made skippable, or given
    // `artifact-ids:` or both `name:` and `pattern:` (which the action reads before the
    // pattern); a job of a called workflow calling one this lint never read, or running a
    // local action.
    let from = "      - name: Assemble SHA256SUMS\n        run: cargo xtask sums assemble dist --summary \"$GITHUB_STEP_SUMMARY\"\n";
    for (uses, name) in [
        (
            "actions/upload-artifact/merge@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "assets-merged",
        ),
        (
            "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "\" assets-padded\"",
        ),
    ] {
        let to = format!(
            "      - uses: {uses} # v7.0.1\n        with:\n          name: {name}\n          path: dist/\n"
        );
        red(
            &spoil_artifacts(ALL_SUMS, from, &to),
            "the job `sums` of artifacts.yml uploads `assets-",
        );
    }
    let pattern = "        with:\n          pattern: assets-linux-*\n";
    for to in [
        format!("        if: ${{{{ false }}}}\n{pattern}"),
        format!("{pattern}          artifact-ids: 1\n"),
        format!("{pattern}          name: sha256sums-linux\n"),
    ] {
        red(
            &spoil_artifacts("attest-linux", pattern, &to),
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        );
    }
    let mut w = real();
    let (path, text) = w
        .called
        .iter_mut()
        .find(|(p, _)| p.ends_with("smoke.yml"))
        .expect("the pipeline calls smoke.yml");
    text.push_str("  farm:\n    uses: ./.github/workflows/farm-upload.yml\n");
    let path = path.clone();
    red(
        &w.problems(),
        &format!("the job `farm` of {path} calls `./.github/workflows/farm-upload.yml`"),
    );
    let mut w = real();
    w.pipeline = in_job(
        &w.pipeline,
        PASSED,
        "    steps:\n",
        "    steps:\n      - uses: ./.github/actions/upload\n",
    );
    red(
        &w.problems(),
        "the job `passed` of pipeline.yml runs the local action `./.github/actions/upload`",
    );
}

#[test]
fn every_shipped_binary_needs_its_own_unconditional_plain_build() {
    // Catches: a binary a platform ships (a tarball's, or the coordinator image's) whose plain
    // cargo build was removed from its seed job, or made skippable, while another plain build
    // (kbf's) stays, so the shipped file need not come from the seed build.
    for job in SEED_JOBS {
        let binaries = shipped(job);
        assert!(binaries.contains(&"kbf"), "{job} ships kbf: {binaries:?}");
        assert!(
            binaries.contains(&"kbf-host-install"),
            "{job}: {binaries:?}"
        );
        if job.starts_with("linux-") {
            assert!(
                binaries.contains(&"kbf-coordinator"),
                "{job} ships the coordinator image's binary: {binaries:?}"
            );
        }
        for binary in binaries {
            let (_, package) = SHIPPED_BINARIES
                .iter()
                .find(|(b, _)| *b == binary)
                .expect("every shipped binary has a package");
            red(
                &spoil_artifacts(job, &format!(" -p {package} "), " "),
                &format!(
                    "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `{binary}`"
                ),
            );
        }
        let host = "        run: KBF_BUILD_COMMIT=";
        let found = spoil_artifacts(job, host, &format!("        if: ${{{{ false }}}}\n{host}"));
        for binary in ["kbf-host-install", "kbf-updater", "kbf-deploy"] {
            red(&found, &format!("of `{binary}` (package `{binary}`)"));
        }
    }
    // A package built, but not the binary it ships.
    red(
        &spoil_artifacts(
            "darwin-arm64",
            "-p kbf-cli --bin kbf\n",
            "-p kbf-cli --bin kbf-cli-fake-exits\n",
        ),
        "the seed job `darwin-arm64` has no unconditional plain `cargo build --locked --release` of `kbf` (package `kbf-cli`)",
    );
    let task = "        run: cargo build --locked --release --target \"$STATIC_TARGET\" -p kbf-task --bin kbf-task\n";
    for job in ["linux-x86_64", "linux-arm64"] {
        red(
            &spoil_artifacts(job, task, &format!("        if: ${{{{ false }}}}\n{task}")),
            &format!(
                "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `kbf-task`"
            ),
        );
    }
}

#[test]
fn the_shipped_binaries_are_the_packages_targets() {
    // Catches: a row of `SHIPPED_BINARIES` naming a package that has no such binary (so a
    // build of the wrong package would satisfy it), or a shipped binary with no row.
    let root = crate::repo_root();
    for (binary, package) in SHIPPED_BINARIES {
        let manifest = std::fs::read_to_string(root.join(format!("crates/{package}/Cargo.toml")))
            .unwrap_or_else(|e| panic!("crates/{package}/Cargo.toml: {e}"));
        assert!(
            manifest.contains(&format!("[package]\nname = \"{package}\"\n")),
            "crates/{package}/Cargo.toml is not the package `{package}`"
        );
        let declared = manifest.contains(&format!("[[bin]]\nname = \"{binary}\"\n"));
        let default =
            binary == package && root.join(format!("crates/{package}/src/main.rs")).is_file();
        assert!(
            declared || default,
            "the package `{package}` has no binary `{binary}`"
        );
    }
    for job in SEED_JOBS {
        for binary in shipped(job) {
            assert!(
                SHIPPED_BINARIES.iter().any(|(b, _)| *b == binary),
                "`{binary}`, shipped for {job}, has no row in `SHIPPED_BINARIES`"
            );
        }
    }
}

#[test]
fn cargo_run_through_another_program_is_refused() {
    // Catches: a second, non-plain build added beside the valid one by running cargo through
    // `env`, `time` or a path to another cargo, which a reader of lines that start with
    // `cargo` would pass over.
    for (job, build) in KBF_BUILDS {
        for wrapped in [
            "env cargo build --release -p kbf-cli",
            "env -i PATH=/opt/kbf cargo build --locked --release -p kbf-cli",
            "time cargo build --release -p kbf-cli",
            "/opt/kbf/bin/cargo build --locked --release -p kbf-cli",
        ] {
            let first = wrapped.split_whitespace().next().expect("a word");
            red(
                &around_kbf_build(job, build, &format!("{{build}}      - run: {wrapped}\n")),
                &format!("cargo is run through `{first}`"),
            );
        }
    }
    assert_eq!(build_line("echo cargo xtask fetch"), Build::None);
}

#[test]
fn a_download_selector_is_read_as_the_action_reads_it() {
    // Catches: an attest or release download whose `name:` holds a wildcard (the action looks
    // one name up, so `name: assets-linux-*` reads nothing), or whose selector differs in case
    // (the action's pattern match is case-sensitive), taken for a reader of the seed files.
    for (from, to, want) in [
        (
            "pattern: assets-linux-*\n",
            "name: assets-linux-*\n",
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        ),
        (
            "pattern: assets-linux-*\n",
            "pattern: ASSETS-linux-*\n",
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        ),
    ] {
        red(&spoil_artifacts("attest-linux", from, to), want);
    }
    red(
        &spoil_artifacts(
            "attest-darwin-arm64",
            "pattern: assets-darwin-arm64\n",
            "name: Assets-darwin-arm64\n",
        ),
        "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
    );
    for to in ["name: assets-*\n", "pattern: Assets-*\n"] {
        let mut w = real();
        w.release = in_job(&w.release, RELEASE_VERIFY, "pattern: assets-*\n", to);
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-linux-x86_64`",
        );
    }
    // An exact `name:` is a reader.
    let found = spoil_artifacts(
        "attest-darwin-arm64",
        "pattern: assets-darwin-arm64\n",
        "name: assets-darwin-arm64\n",
    );
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn a_seed_upload_of_another_name_case_or_directory_is_refused() {
    // Catches: a seed upload renamed only in case (`Assets-<job>`, which the attest and
    // release selectors, case-sensitive, never read), pointed at a directory other than the
    // packaged release files, or allowed to upload nothing.
    for job in SEED_JOBS {
        let name = format!("name: assets-{job}\n");
        red(
            &spoil_artifacts(job, &name, &format!("name: Assets-{job}\n")),
            &format!("the seed job `{job}` does not upload its files as `assets-{job}` (exactly)"),
        );
        for to in [
            "          path: build/\n",
            "          path: target/release/\n",
        ] {
            red(
                &spoil_artifacts(job, "          path: dist/\n", to),
                &format!("the seed job `{job}` uploads `assets-{job}` from Some("),
            );
        }
        red(
            &spoil_artifacts(
                job,
                "          if-no-files-found: error\n",
                "          if-no-files-found: warn\n",
            ),
            "without `if-no-files-found: error`",
        );
    }
}

#[test]
fn a_seed_upload_by_a_sub_action_is_refused() {
    // Catches: a seed upload replaced by `actions/upload-artifact/merge` with the same inputs,
    // which re-uploads artifacts that already exist, not the job's `dist`.
    let uses =
        "- uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n";
    for job in SEED_JOBS {
        red(
            &spoil_artifacts(
                job,
                uses,
                &uses.replace("upload-artifact@", "upload-artifact/merge@"),
            ),
            &format!("the seed job `{job}` does not upload its files as `assets-{job}` (exactly)"),
        );
    }
}

#[test]
fn a_download_that_does_not_put_the_seed_files_in_dist_is_refused() {
    // Catches: an attest or release download of the seed files sent to another directory,
    // unmerged (each artifact in a directory of its own) or left as zips, so the checks that
    // read `dist` no longer read the seed files.
    let merged = "          path: dist\n          merge-multiple: true\n";
    for to in [
        "          path: other\n          merge-multiple: true\n",
        "          path: dist\n          merge-multiple: false\n",
        "          path: dist\n",
        "          path: dist\n          merge-multiple: true\n          skip-decompress: true\n",
    ] {
        for (job, want) in [
            (
                "attest-linux",
                "the attest job `attest-linux` does not download `assets-linux-x86_64`",
            ),
            (
                "attest-darwin-arm64",
                "the attest job `attest-darwin-arm64` does not download `assets-darwin-arm64`",
            ),
        ] {
            let mut w = real();
            let (all, start, end) = job_span(&w.artifacts, job);
            let region = all[start..end].concat();
            let first = region
                .find(merged)
                .unwrap_or_else(|| panic!("{job} merges into dist"));
            let spoiled = format!(
                "{}{to}{}",
                &region[..first],
                &region[first + merged.len()..]
            );
            w.artifacts = [all[..start].concat(), spoiled, all[end..].concat()].concat();
            red(&w.problems(), want);
        }
        let mut w = real();
        let from = format!("          pattern: assets-*\n{merged}");
        w.release = in_job(
            &w.release,
            RELEASE_VERIFY,
            &from,
            &format!("          pattern: assets-*\n{to}"),
        );
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-darwin-arm64`",
        );
    }
    // `skip-decompress: false` is the default, and still extracts.
    let found = spoil_artifacts(
        "attest-darwin-arm64",
        "          pattern: assets-darwin-arm64\n",
        "          pattern: assets-darwin-arm64\n          skip-decompress: false\n",
    );
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn a_seed_build_into_another_target_directory_is_refused() {
    // Catches: a seed job's build sent to another target directory (`CARGO_TARGET_DIR` in a
    // step's, the job's or the workflow's `env:`, written to `$GITHUB_ENV`, or `--target-dir`),
    // so the plain build credited here is not the one in `target/` that packaging reads.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for to in [
            format!("- env:\n          CARGO_TARGET_DIR: /opt/kbf/target\n        run: {line}\n"),
            format!(
                "- env:\n          CARGO_BUILD_TARGET_DIR: /opt/kbf/target\n        run: {line}\n"
            ),
            format!(
                "- run: echo \"CARGO_TARGET_DIR=/opt/kbf/target\" >> \"$GITHUB_ENV\"\n      {build}"
            ),
            format!("- run: {line} --target-dir /opt/kbf/target\n"),
        ] {
            red(
                &spoil_artifacts(job, build, &to),
                &format!("the seed job `{job}` names `target"),
            );
        }
    }
    let mut w = real();
    w.artifacts = w.artifacts.replacen(
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  CARGO_TARGET_DIR: /opt/kbf/target\n",
        1,
    );
    assert_ne!(w.artifacts, read(ARTIFACTS), "the workflow env is spoiled");
    for job in SEED_JOBS {
        red(
            &w.problems(),
            &format!("the seed job `{job}` names `target_dir`"),
        );
    }
    let mut w = real();
    w.artifacts = in_job(
        &w.artifacts,
        "linux-arm64",
        "    runs-on: ubuntu-24.04-arm\n",
        "    runs-on: ubuntu-24.04-arm\n    env:\n      CARGO_TARGET_DIR: /opt/kbf/target\n",
    );
    red(
        &w.problems(),
        "the seed job `linux-arm64` names `target_dir`",
    );
}

#[test]
fn a_download_from_another_repository_is_refused() {
    // Catches: an attest or release download of the seed files pointed at another
    // repository (`repository:`), whose artifacts are not this repository's seed build.
    let pipeline_run = "          run-id: ${{ github.event.workflow_run.id }}\n";
    let from = format!(
        "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n{pipeline_run}"
    );
    for repository in ["someone/kbf-farm", "komira-ai/kbf-builds"] {
        let mut w = real();
        w.release = in_job(
            &w.release,
            RELEASE_VERIFY,
            &from,
            &format!("{from}          repository: {repository}\n"),
        );
        red(
            &w.problems(),
            "the release's `verify` does not download `assets-linux-x86_64`",
        );
        red(
            &spoil_artifacts(
                "attest-linux",
                "          pattern: assets-linux-*\n",
                &format!("          pattern: assets-linux-*\n          repository: {repository}\n"),
            ),
            "the attest job `attest-linux` does not download `assets-linux-x86_64`",
        );
    }
    // This repository, named, is the same download.
    let mut w = real();
    w.release = in_job(
        &w.release,
        RELEASE_VERIFY,
        &from,
        &format!("{from}          repository: ${{{{ github.repository }}}}\n"),
    );
    let found = w.problems();
    assert!(found.is_empty(), "{found:#?}");
}
