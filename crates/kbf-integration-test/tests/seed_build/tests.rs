//! Tests of the seed-path lint. The real files are the control (no problem); each other test
//! changes one thing in a copy of one file, inside the one job it names, and names the rule it
//! holds. A change must alter the text, so a rule whose anchor text moved cannot go quiet.

use super::*;

const WORKFLOWS: &str = ".github/workflows";

struct Texts {
    artifacts: String,
    pipeline: String,
    release: String,
    doc: String,
}

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

fn real() -> Texts {
    Texts {
        artifacts: read(&format!("{WORKFLOWS}/artifacts.yml")),
        pipeline: read(&format!("{WORKFLOWS}/pipeline.yml")),
        release: read(&format!("{WORKFLOWS}/release.yml")),
        doc: read("docs/artifacts.md"),
    }
}

fn found(texts: &Texts) -> Vec<String> {
    problems(
        &texts.artifacts,
        &texts.pipeline,
        &texts.release,
        &texts.doc,
    )
    .unwrap_or_else(|e| vec![e])
}

/// The line range `[start, end)` of the job `job`: its header line to the next job's header.
fn job_lines(lines: &[&str], job: &str) -> (usize, usize) {
    let header = format!("  {job}:");
    let start = lines
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("no job `{job}`"));
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().ends_with(':'))
        .map_or(lines.len(), |n| start + 1 + n);
    (start, end)
}

/// `text` with `from` replaced by `to` inside the job `job` only, which must hold `from` once.
fn in_job(text: &str, job: &str, from: &str, to: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (start, end) = job_lines(&lines, job);
    let region = lines[start..end].concat();
    assert_eq!(
        region.matches(from).count(),
        1,
        "job `{job}` holds `{from}` {} times",
        region.matches(from).count()
    );
    [
        lines[..start].concat(),
        region.replace(from, to),
        lines[end..].concat(),
    ]
    .concat()
}

/// `text` without the job `job` (its header to the next job's header).
fn without_job(text: &str, job: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (start, end) = job_lines(&lines, job);
    [lines[..start].concat(), lines[end..].concat()].concat()
}

/// The problems of the real files with `from` replaced by `to` in the job `job` of `artifacts.yml`.
fn spoil_artifacts(job: &str, from: &str, to: &str) -> Vec<String> {
    let mut texts = real();
    texts.artifacts = in_job(&texts.artifacts, job, from, to);
    found(&texts)
}

fn red(found: &[String], wants: &[&str]) {
    assert!(
        found.iter().any(|p| wants.iter().all(|w| p.contains(w))),
        "want a problem with {wants:?}: {found:#?}"
    );
}

const X86_BUILD: &str = "cargo build --locked --release --target \"$TARGET\" -p kbf-node -p kbf-server -p kbf-cli --bins";

#[test]
fn the_real_files_meet_every_rule() {
    // Catches: the seed path drifting (a build job gone, moved off a hosted runner, built
    // another way, or no longer what is signed and released), and the control for every test
    // below.
    let found = found(&real());
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn a_removed_build_job_is_named() {
    // Catches: the cargo build of a platform deleted (a kbf-built binary left in its place).
    let mut texts = real();
    texts.artifacts = without_job(&texts.artifacts, "linux-arm64");
    red(&found(&texts), &["`linux-arm64`", "missing"]);
}

#[test]
fn a_build_job_off_its_hosted_runner_is_named() {
    // Catches: a seed build moved to a self-hosted runner, or to another label than the one
    // the release pins.
    red(
        &spoil_artifacts(
            "linux-arm64",
            "runs-on: ubuntu-24.04-arm",
            "runs-on: self-hosted",
        ),
        &["`linux-arm64`", "self-hosted"],
    );
    red(
        &spoil_artifacts("darwin-arm64", "runs-on: macos-15", "runs-on: macos-latest"),
        &["`darwin-arm64`", "macos-15"],
    );
}

#[test]
fn buck2_in_a_build_step_is_named() {
    // Catches: the release binary built through Buck2 instead of cargo.
    red(
        &spoil_artifacts(
            "linux-x86_64",
            X86_BUILD,
            "buck2 build //crates/kbf-cli:kbf",
        ),
        &["`linux-x86_64`", "buck2"],
    );
}

#[test]
fn a_remote_execution_flag_on_a_build_is_named() {
    // Catches: a remote-execution flag added to the cargo build.
    red(
        &spoil_artifacts("linux-x86_64", X86_BUILD, &format!("{X86_BUILD} --remote")),
        &["`linux-x86_64`", "--remote"],
    );
}

#[test]
fn a_farm_variable_in_front_of_a_build_is_named() {
    // Catches: a kbf farm address handed to the build through the environment.
    red(
        &spoil_artifacts(
            "linux-x86_64",
            "- run: cargo build --locked",
            "- run: KBF_FARM_ADDR=somewhere cargo build --locked",
        ),
        &["`linux-x86_64`", "KBF_FARM_ADDR"],
    );
}

#[test]
fn a_kbf_tool_in_front_of_a_build_is_named() {
    // Catches: the cargo build run through kbf, so a broken kbf cannot rebuild itself.
    red(
        &spoil_artifacts(
            "linux-x86_64",
            "- run: cargo build --locked",
            "- run: kbf run -- cargo build --locked",
        ),
        &["`linux-x86_64`", "in front of"],
    );
}

#[test]
fn a_build_without_locked_or_release_is_named() {
    // Catches: a build that may resolve other crates than Cargo.lock names, or is not the
    // release profile.
    red(
        &spoil_artifacts(
            "linux-x86_64",
            X86_BUILD,
            &X86_BUILD.replace(" --locked", ""),
        ),
        &["`linux-x86_64`", "--locked"],
    );
    red(
        &spoil_artifacts(
            "darwin-arm64",
            "cargo build --locked --release --target \"$TARGET\" -p kbf-cli --bin kbf",
            "cargo build --locked --target \"$TARGET\" -p kbf-cli --bin kbf",
        ),
        &["`darwin-arm64`", "--release"],
    );
}

#[test]
fn a_job_that_no_longer_builds_kbf_is_named() {
    // Catches: the seed job building something other than the kbf a broken kbf is replaced by.
    red(
        &spoil_artifacts("linux-x86_64", " -p kbf-cli --bins", " --bins"),
        &["`linux-x86_64`", "kbf-cli"],
    );
}

#[test]
fn an_asset_upload_renamed_is_named() {
    // Catches: a build job whose files no later job downloads.
    red(
        &spoil_artifacts(
            "linux-arm64",
            "name: assets-linux-arm64",
            "name: assets-arm",
        ),
        &["`linux-arm64`", "assets-linux-arm64"],
    );
}

#[test]
fn a_sums_job_that_does_not_need_a_build_is_named() {
    // Catches: SHA256SUMS assembled without waiting for a build.
    red(
        &spoil_artifacts(
            "sums-linux",
            "needs: [linux-x86_64, linux-arm64]",
            "needs: [linux-x86_64]",
        ),
        &["`sums-linux`", "`linux-arm64`"],
    );
}

#[test]
fn an_attest_job_that_no_longer_consumes_the_build_is_named() {
    // Catches: the signing step attesting something other than what the cargo jobs built.
    red(
        &spoil_artifacts(
            "attest-linux",
            "pattern: assets-linux-*",
            "pattern: assets-other-*",
        ),
        &["`attest-linux`", "assets-linux-x86_64"],
    );
    red(
        &spoil_artifacts(
            "attest-darwin-arm64",
            "pattern: assets-darwin-arm64",
            "pattern: assets-none",
        ),
        &["`attest-darwin-arm64`", "assets-darwin-arm64"],
    );
}

#[test]
fn an_attest_download_from_another_run_is_named() {
    // Catches: the signing job taking files from some other run than the one that built them.
    red(
        &spoil_artifacts(
            "attest-linux",
            "pattern: assets-linux-*",
            "pattern: assets-linux-*\n          run-id: 1",
        ),
        &["`attest-linux`", "run-id"],
    );
}

#[test]
fn a_pipeline_that_signs_or_passes_without_the_build_is_named() {
    // Catches: a run that signs, or is green, without the build call.
    let mut texts = real();
    texts.pipeline = in_job(
        &texts.pipeline,
        "artifacts-sign",
        "needs: [ci, artifacts]",
        "needs: [ci]",
    );
    red(&found(&texts), &["`artifacts-sign`", "`artifacts`"]);
    let mut texts = real();
    texts.pipeline = in_job(
        &texts.pipeline,
        "passed",
        "needs: [ci, beta, artifacts, artifacts-sign, smoke]",
        "needs: [ci, beta, artifacts-sign, smoke]",
    );
    red(&found(&texts), &["`passed`", "`artifacts`"]);
}

#[test]
fn a_publish_that_does_not_need_verify_is_named() {
    // Catches: a release published without the job that checks the build's files.
    let mut texts = real();
    texts.release = in_job(
        &texts.release,
        "publish",
        "needs: [plan, verify, tag, notes]",
        "needs: [plan, tag, notes]",
    );
    red(&found(&texts), &["`publish`", "`verify`"]);
}

#[test]
fn a_verify_that_downloads_other_files_than_the_build_is_named() {
    // Catches: the release checking files that are not the ones the pipeline run built.
    let mut texts = real();
    texts.release = in_job(
        &texts.release,
        "verify",
        "pattern: assets-*",
        "pattern: assetz-*",
    );
    red(&found(&texts), &["`verify`", "assets-linux-x86_64"]);
}

#[test]
fn a_doc_without_the_seed_section_is_named() {
    // Catches: the reason the seed path must stay going unwritten.
    let mut texts = real();
    texts.doc = texts.doc.replace(DOC_HEADING, "## Something else");
    red(&found(&texts), &["docs/artifacts.md", "seed path"]);
}
