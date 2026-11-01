//! The seed build: every file a release ships is built by plain `cargo build --locked
//! --release` on GitHub-hosted runners, and nothing reaches a release that did not come
//! from that build.
//!
//! This is how a broken kbf is rebuilt without kbf (docs/artifacts.md, "The seed build").
//! When kbf builds kbf, a release built on a kbf farm can only be rebuilt while that farm
//! works; the seed build needs nothing of kbf's, so a release that fixes or rolls back a
//! broken farm can always be cut. A build on kbf may run beside the seed build, never
//! instead of it. [`problems`] reads `artifacts.yml`, `pipeline.yml` and `release.yml` and
//! reports every way they differ from these rules:
//!
//! - **The seed jobs exist.** `artifacts.yml` has each of [`SEED_JOBS`], a job with steps (not
//!   a call of another workflow).
//! - **On a GitHub-hosted runner.** Each one's `runs-on` is one label of GitHub's hosted
//!   runner table, written out literally ([`workflows::job_runs_on`],
//!   [`workflows::is_hosted_label`]): not `self-hosted`, a custom label, a runner group or an
//!   expression.
//! - **Plain cargo.** Each builds the `kbf-cli` package (`kbf`) in an unconditional step, and
//!   every `cargo build` line of the job is plain: `cargo build --locked --release`, then only
//!   [`BUILD_FLAGS`] (a target, packages, binaries), after no variable but
//!   [`BUILD_VARIABLES`], in a step with no `shell:` of its own. A line that compiles with
//!   another cargo verb ([`OTHER_BUILD_VERBS`]) is refused, and no scalar of the job (a key or
//!   value: its runner, `env`, steps, scripts and action inputs) nor of the workflow's own
//!   `env:` names any of [`NOT_PLAIN`] (Buck or Bazel, a remote-execution or remote-cache
//!   flag, a gRPC farm address, a rustc wrapper).
//! - **The published files are theirs.** Each seed job uploads its files as the artifact
//!   `assets-<job>` in an unconditional step, and no other job of `artifacts.yml`, of
//!   `pipeline.yml` or of a workflow the pipeline calls (whose artifacts share the
//!   pipeline's run) uploads an artifact whose name starts [`ASSETS_PREFIX`], or might (a
//!   name an expression starts). An attest job downloads each seed job's artifact, and the
//!   release's `verify` job downloads every seed job's artifact from the pipeline run that
//!   started it.
//! - **The release needs them.** In `artifacts.yml` each seed job is needed by the sums job
//!   of its platform group and by `sums` ([`SEED_GROUPS`]: the sums the attest job holds its
//!   files to, by the digest the pipeline passes, which `pipeline_workflow` pins), and each
//!   group's verify job needs its attest job. In `pipeline.yml` the build call `artifacts`
//!   (stage `build`) is needed by the sign call `artifacts-sign` (stage `sign`) and by
//!   `passed`, whose success alone starts the release. In `release.yml`, started by the
//!   pipeline's completion, `publish` needs `verify`.
//!
//! Whether the release runs only after a successful pipeline run of main is
//! `release_workflow`'s rule; which files are built and attested, `release_artifacts`'.

use super::workflows::{self, Step};

#[cfg(test)]
mod tests;

/// The jobs of `artifacts.yml` that build the release's files, each named by its platform.
pub const SEED_JOBS: &[&str] = &["darwin-arm64", "linux-x86_64", "linux-arm64"];

/// Each seed job with the sums, attest and verify jobs of its platform group.
pub const SEED_GROUPS: &[(&str, &str, &str, &str)] = &[
    (
        "darwin-arm64",
        "sums-darwin-arm64",
        "attest-darwin-arm64",
        "verify-darwin-arm64",
    ),
    ("linux-x86_64", "sums-linux", "attest-linux", "verify-linux"),
    ("linux-arm64", "sums-linux", "attest-linux", "verify-linux"),
];

/// The job that sums every platform's files, the set a release carries.
const ALL_SUMS: &str = "sums";

/// The package whose binary is `kbf`, which every seed job builds.
const KBF_PACKAGE: &str = "kbf-cli";

/// The flags a plain build line may pass after `cargo build --locked --release`, and whether
/// each takes a value.
pub const BUILD_FLAGS: &[(&str, bool)] = &[
    ("--target", true),
    ("-p", true),
    ("--package", true),
    ("--bin", true),
    ("--bins", false),
];

/// The variables a plain build line may set before `cargo` (the commit `kbf-updater` is stamped with).
pub const BUILD_VARIABLES: &[&str] = &["KBF_BUILD_COMMIT"];

/// Cargo verbs that compile, other than `build`: a seed job compiles with `cargo build` alone.
pub const OTHER_BUILD_VERBS: &[&str] = &["b", "rustc", "install", "zigbuild", "auditable"];

/// What no scalar of a seed job, nor the workflow's `env:`, may name, compared ignoring ASCII
/// case: another build system, a remote-execution or remote-cache setting, a farm's gRPC
/// address, or a wrapper that hands rustc's work to something else.
pub const NOT_PLAIN: &[&str] = &[
    "buck",
    "bazel",
    "remote_executor",
    "remote-executor",
    "remote_cache",
    "remote-cache",
    "remote_instance_name",
    "engine_address",
    "cas_address",
    "action_cache_address",
    "grpc://",
    "grpcs://",
    "rustc_wrapper",
    "rustc-wrapper",
    "rustc_workspace_wrapper",
    "rustc-workspace-wrapper",
    "sccache",
];

/// The prefix of the artifacts that carry a release's files.
pub const ASSETS_PREFIX: &str = "assets-";

/// The pipeline's call of `artifacts.yml` that builds, the one that signs, and the job whose
/// success is the pipeline's.
const BUILD_CALL: &str = "artifacts";
const SIGN_CALL: &str = "artifacts-sign";
const PASSED: &str = "passed";
const ARTIFACTS_USES: &str = "./.github/workflows/artifacts.yml";

/// The release jobs: the one that downloads and verifies the pipeline's files, and the one
/// that publishes them.
const RELEASE_VERIFY: &str = "verify";
const RELEASE_PUBLISH: &str = "publish";
const PIPELINE_RUN_ID: &str = "${{ github.event.workflow_run.id }}";

/// The workflow texts the rules read: `artifacts.yml`, `pipeline.yml`, `release.yml`, and
/// every other workflow the pipeline calls, by its `uses:` path.
pub struct Workflows<'a> {
    pub artifacts: &'a str,
    pub pipeline: &'a str,
    pub release: &'a str,
    pub called: Vec<(String, String)>,
}

/// What differs between the workflows and the seed-build rules: one line per difference,
/// empty when there is none. An error when a workflow cannot be read.
pub fn problems(w: &Workflows<'_>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let jobs = workflows::job_names(w.artifacts)?;
    for job in SEED_JOBS {
        if !jobs.iter().any(|j| j.eq_ignore_ascii_case(job)) {
            out.push(format!(
                "artifacts.yml has no seed job `{job}`: nothing builds its release files with plain cargo on a hosted runner"
            ));
            continue;
        }
        seed_job(w.artifacts, job, &mut out)?;
    }
    published(w, &mut out)?;
    needed(w, &mut out)?;
    Ok(out)
}

/// The rules of one seed job: a hosted runner, plain cargo, and its upload.
fn seed_job(text: &str, job: &str, out: &mut Vec<String>) -> Result<(), String> {
    if workflows::job_uses(text, job)?.is_some() {
        out.push(format!(
            "the seed job `{job}` calls another workflow instead of building on its own runner"
        ));
        return Ok(());
    }
    match workflows::job_runs_on(text, job) {
        Ok(label) if workflows::is_hosted_label(&label) => {}
        Ok(label) => out.push(format!(
            "the seed job `{job}` runs on `{label}`, not a GitHub-hosted runner"
        )),
        Err(e) => out.push(format!(
            "the seed job `{job}` is not held to a GitHub-hosted runner: {e}"
        )),
    }
    let mut scalars = workflows::job_strings(text, job)?;
    for (k, v) in workflows::top_env(text)? {
        scalars.push(k);
        scalars.push(v);
    }
    for s in &scalars {
        let lower = s.to_ascii_lowercase();
        if let Some(word) = NOT_PLAIN.iter().find(|w| lower.contains(*w)) {
            out.push(format!(
                "the seed job `{job}` names `{}` (in `{s}`): the seed build is plain cargo, with no other build system, farm or remote execution",
                word.trim()
            ));
        }
    }
    let steps = workflows::job_steps(text, job)?;
    let mut builds_kbf = false;
    for step in &steps {
        for line in step.run.as_deref().unwrap_or_default().lines() {
            match build_line(line) {
                Build::None => {}
                Build::Plain(packages) => {
                    if step.shell.is_some() {
                        out.push(format!(
                            "the seed job `{job}` runs `{}` under a `shell:` of its own",
                            line.trim()
                        ));
                    } else if !step.conditional && packages.contains(&KBF_PACKAGE) {
                        builds_kbf = true;
                    }
                }
                Build::NotPlain(why) => out.push(format!(
                    "the seed job `{job}` runs `{}`, not a plain `cargo build --locked --release`: {why}",
                    line.trim()
                )),
            }
        }
    }
    if !builds_kbf {
        out.push(format!(
            "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `{KBF_PACKAGE}`"
        ));
    }
    let want = format!("{ASSETS_PREFIX}{job}");
    let uploads: Vec<&Step> = steps
        .iter()
        .filter(|s| is_upload(s) && upload_name(s).eq_ignore_ascii_case(&want))
        .collect();
    if !matches!(uploads.as_slice(), [one] if !one.conditional) {
        out.push(format!(
            "the seed job `{job}` does not upload its files as `{want}` in one unconditional step"
        ));
    }
    Ok(())
}

/// What a script line builds with.
#[derive(Debug, PartialEq, Eq)]
enum Build<'a> {
    /// The line does not compile with cargo.
    None,
    /// A plain `cargo build --locked --release`, with the packages it names.
    Plain(Vec<&'a str>),
    /// A build that is not plain, and why.
    NotPlain(String),
}

/// What the script line `line` builds with (its words split at blanks, double quotes dropped).
fn build_line(line: &str) -> Build<'_> {
    let mut words = line
        .split_whitespace()
        .map(|w| w.trim_matches('"'))
        .peekable();
    let mut set = Vec::new();
    while let Some(w) = words.peek()
        && let Some((name, _)) = w.split_once('=')
        && !name.is_empty()
        && !name.starts_with('-')
    {
        set.push(name);
        words.next();
    }
    let words: Vec<&str> = words.collect();
    let verb = match words.as_slice() {
        ["cargo", verb, ..] => *verb,
        _ => return Build::None,
    };
    if verb != "build" {
        return if verb.starts_with('+') || OTHER_BUILD_VERBS.contains(&verb) {
            Build::NotPlain(format!("it compiles with `cargo {verb}`"))
        } else {
            Build::None
        };
    }
    if let Some(name) = set.iter().find(|n| !BUILD_VARIABLES.contains(n)) {
        return Build::NotPlain(format!("it sets `{name}`"));
    }
    if words.get(2..4) != Some(&["--locked", "--release"]) {
        return Build::NotPlain("`--locked --release` do not follow `cargo build`".to_owned());
    }
    let mut packages = Vec::new();
    let mut rest = words[4..].iter();
    while let Some(flag) = rest.next() {
        match BUILD_FLAGS.iter().find(|(f, _)| f == flag) {
            None => return Build::NotPlain(format!("it passes `{flag}`")),
            Some((_, false)) => {}
            Some((f, true)) => match rest.next() {
                Some(v) if !v.starts_with('-') => {
                    if matches!(*f, "-p" | "--package") {
                        packages.push(*v);
                    }
                }
                _ => return Build::NotPlain(format!("`{flag}` has no value")),
            },
        }
    }
    Build::Plain(packages)
}

/// Whether `step` uploads a workflow artifact.
fn is_upload(step: &Step) -> bool {
    step.uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/upload-artifact@"))
}

/// Whether `step` downloads workflow artifacts.
fn is_download(step: &Step) -> bool {
    step.uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/download-artifact@"))
}

/// The value of `step`'s input `key`, if it passes one.
fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// The artifact name an upload step gives (`artifact` without a `name:`, as the action does).
fn upload_name(step: &Step) -> &str {
    input(step, "name").unwrap_or("artifact")
}

/// Whether an upload named `name` could be read as a release file's artifact: its name
/// starts [`ASSETS_PREFIX`], or the text before its first expression could begin such a name.
fn may_be_assets(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match name.split_once("${{") {
        Some((head, _)) => ASSETS_PREFIX.starts_with(head) || head.starts_with(ASSETS_PREFIX),
        None => name.starts_with(ASSETS_PREFIX),
    }
}

/// Whether the artifact name `name` matches the download pattern `pattern`, whose only
/// wildcard is `*` (ignoring ASCII case, as artifact names are).
fn matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// Whether a download step of `steps` reads the artifact `name`, with `run-id` `run_id` (none:
/// this run's).
fn downloads(steps: &[Step], name: &str, run_id: Option<&str>) -> bool {
    steps.iter().any(|s| {
        is_download(s)
            && input(s, "run-id") == run_id
            && input(s, "pattern")
                .or_else(|| input(s, "name"))
                .is_some_and(|p| matches(p, name))
    })
}

/// The release's files are the seed jobs': no other job uploads an assets artifact, and the
/// attest jobs and the release's verify read each seed job's.
fn published(w: &Workflows<'_>, out: &mut Vec<String>) -> Result<(), String> {
    let mut texts = vec![("artifacts.yml", w.artifacts), ("pipeline.yml", w.pipeline)];
    texts.extend(w.called.iter().map(|(p, t)| (p.as_str(), t.as_str())));
    for (path, text) in texts {
        for job in workflows::job_names(text)? {
            let seed = path == "artifacts.yml" && SEED_JOBS.contains(&job.as_str());
            if seed || workflows::job_uses(text, &job)?.is_some() {
                continue;
            }
            for step in workflows::job_steps(text, &job)? {
                if is_upload(&step) && may_be_assets(upload_name(&step)) {
                    out.push(format!(
                        "the job `{job}` of {path} uploads `{}`, which the release would take for a seed job's files",
                        upload_name(&step)
                    ));
                }
            }
        }
    }
    for (seed, _, attest, _) in SEED_GROUPS {
        let name = format!("{ASSETS_PREFIX}{seed}");
        if !downloads(&workflows::job_steps(w.artifacts, attest)?, &name, None) {
            out.push(format!(
                "the attest job `{attest}` does not download `{name}`: the seed job's files are not what it signs"
            ));
        }
        let verify = workflows::job_steps(w.release, RELEASE_VERIFY)?;
        if !downloads(&verify, &name, Some(PIPELINE_RUN_ID)) {
            out.push(format!(
                "the release's `{RELEASE_VERIFY}` does not download `{name}` from the pipeline run: the seed job's files are not what it releases"
            ));
        }
    }
    Ok(())
}

/// Whether the job `job` of `text` needs `need`.
fn needs(text: &str, job: &str, need: &str) -> Result<bool, String> {
    Ok(workflows::job_needs(text, job)?
        .iter()
        .any(|n| n.eq_ignore_ascii_case(need)))
}

/// The release needs the seed jobs, through the sums, the pipeline's calls and `verify`.
fn needed(w: &Workflows<'_>, out: &mut Vec<String>) -> Result<(), String> {
    let mut edges: Vec<(&str, &str, &str)> = Vec::new();
    for (seed, sums, attest, verify) in SEED_GROUPS {
        for edge in [(*sums, *seed), (ALL_SUMS, *seed), (*verify, *attest)] {
            if !edges.iter().any(|(_, j, n)| (*j, *n) == edge) {
                edges.push(("artifacts.yml", edge.0, edge.1));
            }
        }
    }
    edges.push(("pipeline.yml", SIGN_CALL, BUILD_CALL));
    edges.push(("pipeline.yml", PASSED, BUILD_CALL));
    edges.push(("pipeline.yml", PASSED, SIGN_CALL));
    edges.push(("release.yml", RELEASE_PUBLISH, RELEASE_VERIFY));
    for (path, job, need) in edges {
        let text = match path {
            "artifacts.yml" => w.artifacts,
            "pipeline.yml" => w.pipeline,
            _ => w.release,
        };
        if !needs(text, job, need)? {
            out.push(format!(
                "the job `{job}` of {path} does not need `{need}`: the release no longer waits for the seed build"
            ));
        }
    }
    for (call, stage) in [(BUILD_CALL, "build"), (SIGN_CALL, "sign")] {
        let uses = workflows::job_uses(w.pipeline, call)?;
        let with = workflows::job_with(w.pipeline, call)?;
        let staged = with
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("stage") && v == stage);
        if uses.as_deref() != Some(ARTIFACTS_USES) || !staged {
            out.push(format!(
                "the pipeline's `{call}` is not the `{stage}` stage of {ARTIFACTS_USES}"
            ));
        }
    }
    Ok(())
}

/// The workflows the pipeline calls, as `(uses path, text)`, read from the repository.
fn called(root: &std::path::Path, pipeline: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for job in workflows::job_names(pipeline).expect("pipeline.yml is read") {
        let uses = workflows::job_uses(pipeline, &job).expect("a `uses` is read");
        if let Some(path) = uses.as_deref().and_then(|u| u.strip_prefix("./"))
            && path != ARTIFACTS_USES.trim_start_matches("./")
            && !out.iter().any(|(p, _): &(String, String)| p == path)
        {
            let text = std::fs::read_to_string(root.join(path)).expect(path);
            out.push((path.to_owned(), text));
        }
    }
    out
}

#[test]
fn the_release_is_built_by_plain_cargo_on_hosted_runners() {
    // Catches: a seed job of artifacts.yml removed, moved off GitHub's hosted runners, given
    // Buck2, a farm address or a remote-execution flag; the published files coming from
    // another job; the release no longer waiting for the seed jobs.
    let root = super::repo_root();
    let read = |p: &str| std::fs::read_to_string(root.join(p)).expect(p);
    let pipeline = read(".github/workflows/pipeline.yml");
    let w = Workflows {
        artifacts: &read(super::ARTIFACTS_WORKFLOW),
        pipeline: &pipeline,
        release: &read(".github/workflows/release.yml"),
        called: called(&root, &pipeline),
    };
    assert!(
        w.called.len() >= 3,
        "the pipeline calls ci, beta and smoke: {:?}",
        w.called.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    let found = problems(&w).expect("the workflows are read");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_docs_say_why_the_seed_build_stays() {
    // Catches: docs/artifacts.md losing the section that says the seed build is how a broken
    // kbf is rebuilt without kbf, and that a build on kbf runs beside it, never instead.
    let doc = std::fs::read_to_string(super::repo_root().join("docs/artifacts.md"))
        .expect("docs/artifacts.md");
    let doc = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    for want in [
        "## The seed build: plain cargo on GitHub-hosted runners",
        "rebuilt without kbf",
        "beside the seed build, never instead of it",
        "crates/kbf-integration-test/tests/seed_build",
    ] {
        assert!(
            doc.contains(want),
            "docs/artifacts.md does not say `{want}`"
        );
    }
}
