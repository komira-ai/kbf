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
//!   a call of another workflow) that needs no other job, downloads no artifact and runs no
//!   local action: it builds from the checkout alone.
//! - **On a GitHub-hosted runner.** Each one's `runs-on` is one label of GitHub's hosted
//!   runner table, written out literally ([`workflows::job_runs_on`],
//!   [`workflows::is_hosted_label`]): not `self-hosted`, a custom label, a runner group or an
//!   expression.
//! - **Plain cargo.** Each builds every binary its platform ships ([`shipped`]: its tarballs'
//!   and images', by package, [`SHIPPED_BINARIES`]) for one of [`BUILD_TARGETS`] in an
//!   unconditional step, and every
//!   `cargo build` line of the job is plain: `cargo build --locked --release`, then only
//!   [`BUILD_FLAGS`] (a target, packages, binaries), after no variable but
//!   [`BUILD_VARIABLES`], in a step with no `shell:` of its own and a job with no `defaults:`
//!   of its own (the workflow's shell is held to bash by `attested_bundles`). A line that compiles with another cargo verb
//!   ([`OTHER_BUILD_VERBS`]), puts a cargo option before its verb (`cargo --config ...
//!   build`) or runs cargo through anything (`env cargo build`, a path to cargo), is refused, and no scalar of the job (a key or value: its runner, `env`, steps,
//!   scripts and action inputs) nor of the workflow's own `env:` names any of [`NOT_PLAIN`]
//!   (Buck2 or Bazel, a remote-execution or remote-cache flag, a gRPC farm address, another
//!   rustc or a wrapper of it). Cargo's own configuration files are not read: a change there
//!   is a change to every build of the repository, reviewed as such.
//! - **The published files are theirs.** Each seed job uploads its packaged files (`dist`,
//!   with `if-no-files-found: error`) as the artifact `assets-<job>`, spelled exactly (the
//!   readers' selectors are case-sensitive), in an unconditional step, and no other job of `artifacts.yml`, of
//!   `pipeline.yml` or of a workflow the pipeline calls, at any depth (whose artifacts share
//!   the pipeline's run), uploads an artifact whose name starts [`ASSETS_PREFIX`], or might (a
//!   name an expression starts); nor does such a job run a local action or call a workflow
//!   this lint did not read. An attest job downloads each seed job's artifact, and the
//!   release's `verify` job downloads every seed job's artifact from the pipeline run that
//!   started it, each in an unconditional step that names it by one `pattern:` (merged) or
//!   `name:` and extracts it into `dist`. The seed upload is `actions/upload-artifact` itself,
//!   not a sub-action such as `merge`, which re-uploads artifacts that exist instead of `dist`.
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

/// The package each shipped binary is built from, as `(binary, package)`: every binary of a
/// platform's release set (`release_artifacts::ASSETS`; the `kbf-task-bundle` is the signed
/// `kbf-task`) and of its images ([`IMAGE_BINARIES`]) must be built by an unconditional plain
/// build in that platform's seed job. `the_shipped_binaries_are_the_packages_targets` holds the
/// table to each package's `Cargo.toml`.
pub const SHIPPED_BINARIES: &[(&str, &str)] = &[
    ("kbf", "kbf-cli"),
    ("kbf-daemon", "kbf-node"),
    ("kbf-server", "kbf-server"),
    ("kbf-deploy", "kbf-deploy"),
    ("kbf-installer", "kbf-installer"),
    ("kbf-host-install", "kbf-host-install"),
    ("kbf-updater", "kbf-updater"),
    ("kbf-mac-provision", "kbf-mac-provision"),
    ("kbf-signer", "kbf-signer"),
    ("kbf-task", "kbf-task"),
    ("kbf-coordinator", "kbf-coordinator"),
];

/// The binaries a platform's images are built from that are not its tarballs: the update
/// coordinator's image, on Linux (`release_artifacts`' images).
pub const IMAGE_BINARIES: &[(&str, &str)] = &[
    ("kbf-coordinator", "linux-x86_64"),
    ("kbf-coordinator", "linux-arm64"),
];

/// The binaries the seed job `job` ships, in the order the release set lists them.
pub fn shipped(job: &str) -> Vec<&'static str> {
    let out: Vec<&str> = super::release_artifacts::ASSETS
        .iter()
        .chain(IMAGE_BINARIES)
        .filter(|(_, platform)| *platform == job)
        .map(|(name, _)| {
            if *name == "kbf-task-bundle" {
                "kbf-task"
            } else {
                *name
            }
        })
        .fold(Vec::new(), |mut out, name| {
            if !out.contains(&name) {
                out.push(name);
            }
            out
        });
    out
}

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
/// address, or another rustc (`RUSTC`, `CARGO_BUILD_RUSTC`, a wrapper, a line that writes one
/// to `$GITHUB_ENV`: no seed job names rustc at all), a cache that hands its work elsewhere, or
/// another target directory (`CARGO_TARGET_DIR`, `--target-dir`), which would leave the build
/// credited here out of the `target/` the packaging steps read.
pub const NOT_PLAIN: &[&str] = &[
    "buck2",
    "buck-out",
    "buckconfig",
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
    "rustc",
    "sccache",
    "target_dir",
    "target-dir",
];

/// The directory a seed job packages its release files into (`cargo xtask package dist ...`,
/// `release_artifacts`' rules), as its upload may name it, and the one the attest jobs and the
/// release's verify extract them into and check.
pub const UPLOAD_PATHS: &[&str] = &["dist", "dist/"];

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
const THIS_REPOSITORY: &str = "${{ github.repository }}";

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
    if workflows::job_has_key(text, job, "defaults")? {
        out.push(format!(
            "the seed job `{job}` has `defaults:` of its own, which can run its build under another shell"
        ));
    }
    if !workflows::job_needs(text, job)?.is_empty() {
        out.push(format!(
            "the seed job `{job}` needs another job: it builds from the checkout alone"
        ));
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
    let mut built: Vec<Built<'_>> = Vec::new();
    for step in &steps {
        if is_download(step) {
            out.push(format!(
                "the seed job `{job}` downloads an artifact: its files are built by its own cargo, not taken from another job"
            ));
        }
        if let Some(local) = step.uses.as_deref().filter(|u| u.starts_with("./")) {
            out.push(format!(
                "the seed job `{job}` runs the local action `{local}`, which this lint does not read"
            ));
        }
        for line in step.run.as_deref().unwrap_or_default().lines() {
            match build_line(line) {
                Build::None => {}
                Build::Plain(b) => {
                    if step.shell.is_some() {
                        out.push(format!(
                            "the seed job `{job}` runs `{}` under a `shell:` of its own",
                            line.trim()
                        ));
                    } else if !step.conditional {
                        built.push(b);
                    }
                }
                Build::NotPlain(why) => out.push(format!(
                    "the seed job `{job}` runs `{}`, not a plain `cargo build --locked --release`: {why}",
                    line.trim()
                )),
            }
        }
    }
    for binary in shipped(job) {
        let Some((_, package)) = SHIPPED_BINARIES.iter().find(|(b, _)| *b == binary) else {
            out.push(format!(
                "the seed job `{job}` ships `{binary}`, which `SHIPPED_BINARIES` does not name a package for"
            ));
            continue;
        };
        if !built.iter().any(|b| b.builds(package, binary)) {
            out.push(format!(
                "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `{binary}` (package `{package}`)"
            ));
        }
    }
    let want = format!("{ASSETS_PREFIX}{job}");
    let uploads: Vec<&Step> = steps
        .iter()
        .filter(|s| is_root_upload(s) && upload_name(s) == want)
        .collect();
    match uploads.as_slice() {
        [one] if !one.conditional => {
            let path = input(one, "path").map(str::trim);
            if !matches!(path, Some(p) if UPLOAD_PATHS.contains(&p)) {
                out.push(format!(
                    "the seed job `{job}` uploads `{want}` from {path:?}, not the packaged release files in `dist`"
                ));
            }
            if input(one, "if-no-files-found").map(str::trim) != Some("error") {
                out.push(format!(
                    "the seed job `{job}` uploads `{want}` without `if-no-files-found: error`: an empty `dist` would pass"
                ));
            }
        }
        _ => out.push(format!(
            "the seed job `{job}` does not upload its files as `{want}` (exactly) in one unconditional step"
        )),
    }
    Ok(())
}

/// What a script line builds with.
#[derive(Debug, PartialEq, Eq)]
enum Build<'a> {
    /// The line does not compile with cargo.
    None,
    /// A plain `cargo build --locked --release`, with what it builds.
    Plain(Built<'a>),
    /// A build that is not plain, and why.
    NotPlain(String),
}

/// What a plain build line builds: the packages it names, and their binaries it names
/// (`--bin`) or all of them (`--bins`), for the target it names (`--target`).
#[derive(Debug, Default, PartialEq, Eq)]
struct Built<'a> {
    packages: Vec<&'a str>,
    bins: Vec<&'a str>,
    all_bins: bool,
    target: Option<&'a str>,
}

/// The targets a seed job's credited build may name: the job's `TARGET`, or `STATIC_TARGET`
/// (the static `kbf-task`), the triples whose `target/<triple>/release` the packaging steps
/// read. A build without `--target` lands in `target/release`, which no package step reads.
pub const BUILD_TARGETS: &[&str] = &["$TARGET", "$STATIC_TARGET"];

impl Built<'_> {
    /// Whether the line builds the binary `binary` of the package `package`, for one of
    /// [`BUILD_TARGETS`].
    fn builds(&self, package: &str, binary: &str) -> bool {
        self.packages.contains(&package)
            && (self.all_bins || self.bins.contains(&binary))
            && self.target.is_some_and(|t| BUILD_TARGETS.contains(&t))
    }
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
    let compiles = |w: &&str| *w == "build" || OTHER_BUILD_VERBS.contains(w);
    let is_cargo = |w: &&str| *w == "cargo" || w.ends_with("/cargo");
    let verb = match words.as_slice() {
        ["cargo", verb, ..] => *verb,
        [first, ..] if words.iter().any(is_cargo) => {
            let after = words.iter().skip_while(|w| !is_cargo(w)).skip(1);
            return if after.clone().any(compiles) {
                Build::NotPlain(format!("cargo is run through `{first}`"))
            } else {
                Build::None
            };
        }
        _ => return Build::None,
    };
    if verb.starts_with('-') && words.iter().any(compiles) {
        return Build::NotPlain(format!("`{verb}` comes before the verb"));
    }
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
    let mut built = Built::default();
    let mut rest = words[4..].iter();
    while let Some(flag) = rest.next() {
        match BUILD_FLAGS.iter().find(|(f, _)| f == flag) {
            None => return Build::NotPlain(format!("it passes `{flag}`")),
            Some((_, false)) => built.all_bins = true,
            Some((f, true)) => match rest.next() {
                Some(v) if !v.starts_with('-') => match *f {
                    "-p" | "--package" => built.packages.push(*v),
                    "--bin" => built.bins.push(*v),
                    "--target" => built.target = Some(*v),
                    _ => {}
                },
                _ => return Build::NotPlain(format!("`{flag}` has no value")),
            },
        }
    }
    Build::Plain(built)
}

/// Whether `step` runs the action `action` or one of its sub-actions (`actions/upload-artifact/merge`).
fn runs_action(step: &Step, action: &str) -> bool {
    step.uses.as_deref().is_some_and(|u| {
        let name = u
            .split_once('@')
            .map_or(u, |(name, _)| name)
            .to_ascii_lowercase();
        name == action || name.starts_with(&format!("{action}/"))
    })
}

/// Whether `step` uploads a workflow artifact.
fn is_upload(step: &Step) -> bool {
    runs_action(step, "actions/upload-artifact")
}

/// Whether `step` runs `actions/upload-artifact` itself, the action that uploads the job's
/// files (its sub-actions, such as `merge`, re-upload artifacts that already exist).
fn is_root_upload(step: &Step) -> bool {
    step.uses.as_deref().is_some_and(|u| {
        u.split_once('@')
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("actions/upload-artifact"))
    })
}

/// Whether `step` downloads workflow artifacts.
fn is_download(step: &Step) -> bool {
    runs_action(step, "actions/download-artifact")
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
    match input(step, "name").map(str::trim) {
        Some(name) if !name.is_empty() => name,
        _ => "artifact",
    }
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
/// wildcard is `*`, case-sensitively, as the download action's pattern is.
fn matches(pattern: &str, name: &str) -> bool {
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

/// Whether an unconditional download step of `steps` reads the artifact `name`, with `run-id`
/// `run_id` (none: this run's), into the files the checks after it read: by a `pattern:` that
/// matches it, merged (`merge-multiple: true`: otherwise each artifact lands in a directory of
/// its own), or a `name:` that is exactly it (one lookup, no wildcard), not both; no
/// `artifact-ids:`, which the action reads first; extracted into `dist` (`path:`, and no
/// `skip-decompress: true`, which would leave the zip); from this repository (no `repository:`
/// but [`THIS_REPOSITORY`]).
fn downloads(steps: &[Step], name: &str, run_id: Option<&str>) -> bool {
    let is = |s: &Step, key: &str, value: &str| input(s, key).map(str::trim) == Some(value);
    steps.iter().any(|s| {
        let reads = match (input(s, "pattern"), input(s, "name")) {
            (Some(p), None) => matches(p.trim(), name) && is(s, "merge-multiple", "true"),
            (None, Some(n)) => n.trim() == name,
            _ => false,
        };
        let into_dist = input(s, "path")
            .map(str::trim)
            .is_some_and(|p| UPLOAD_PATHS.contains(&p));
        is_download(s)
            && !s.conditional
            && input(s, "artifact-ids").is_none()
            && input(s, "run-id") == run_id
            && input(s, "repository").is_none_or(|r| r.trim() == THIS_REPOSITORY)
            && into_dist
            && input(s, "skip-decompress").is_none_or(|v| v.trim() == "false")
            && reads
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
            if seed {
                continue;
            }
            if let Some(uses) = workflows::job_uses(text, &job)? {
                let read = uses == ARTIFACTS_USES
                    || w.called
                        .iter()
                        .any(|(p, _)| uses.strip_prefix("./") == Some(p.as_str()));
                if !read {
                    out.push(format!(
                        "the job `{job}` of {path} calls `{uses}`, which this lint does not read for uploads"
                    ));
                }
                continue;
            }
            for step in workflows::job_steps(text, &job)? {
                if let Some(local) = step.uses.as_deref().filter(|u| u.starts_with("./")) {
                    out.push(format!(
                        "the job `{job}` of {path} runs the local action `{local}`, which this lint does not read for uploads"
                    ));
                }
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

/// The workflows the pipeline calls, and those they call in turn, as `(uses path, text)`, read
/// from the repository.
fn called(root: &std::path::Path, pipeline: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut next = 0;
    let mut text = pipeline.to_owned();
    loop {
        for job in workflows::job_names(&text).expect("a called workflow is read") {
            let uses = workflows::job_uses(&text, &job).expect("a `uses` is read");
            if let Some(path) = uses.as_deref().and_then(|u| u.strip_prefix("./"))
                && path != ARTIFACTS_USES.trim_start_matches("./")
                && !out.iter().any(|(p, _)| p == path)
            {
                let found = std::fs::read_to_string(root.join(path)).expect(path);
                out.push((path.to_owned(), found));
            }
        }
        let Some((_, t)) = out.get(next) else {
            return out;
        };
        text = t.clone();
        next += 1;
    }
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
