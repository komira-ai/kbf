//! The seed path: the release is always also built with plain cargo on GitHub-hosted runners.
//!
//! A kbf that builds kbf may run beside the build jobs of `artifacts.yml`, never instead of
//! them: they are how a broken kbf is rebuilt without kbf, and the files they build are the
//! ones that are signed and released. [`problems`] reads `artifacts.yml`, `pipeline.yml`,
//! `release.yml` and `docs/artifacts.md` and reports each way they differ from these rules; the
//! real files must report none, and `tests` spoils a copy for each rule.
//!
//! - **The jobs exist, on hosted runners.** `darwin-arm64` (`macos-15`), `linux-x86_64`
//!   (`ubuntu-24.04`) and `linux-arm64` (`ubuntu-24.04-arm`) are the build jobs, each on exactly
//!   that GitHub-hosted label.
//! - **Plain cargo.** Every `cargo build` of those jobs has `--locked` and `--release`, has
//!   nothing in front of it but `NAME=value` assignments (no `kbf` or other tool wrapping it),
//!   and each job builds `kbf-cli`. The jobs use no `buck2`, no remote-execution flag and no kbf
//!   farm setting: no word `farm` or `remote`, and no `KBF_` variable but `KBF_BUILD_COMMIT`
//!   (the commit the binary reports).
//! - **What is signed and released comes from them.** Each job uploads its `assets-*` artifact;
//!   the sums job of its group needs it; the attest job of the group downloads that artifact
//!   from the same run; in `pipeline.yml` `artifacts-sign` needs `artifacts` and `passed` needs
//!   both; in `release.yml` `verify` downloads the pipeline run's `assets-*` and `publish`
//!   needs `verify`.
//! - **The doc says why.** `docs/artifacts.md` has the section [`DOC_HEADING`] and says a
//!   kbf-on-kbf build is never instead of the seed path.

/// The heading of the section of `docs/artifacts.md` that says the seed path must stay.
pub const DOC_HEADING: &str = "## The seed path: plain cargo on GitHub-hosted runners";

use super::workflows::{self, Step};

/// One build job of `artifacts.yml` and what hangs off it.
struct Build {
    job: &'static str,
    runner: &'static str,
    /// The artifact the job uploads, which the sums, attest and release jobs consume.
    asset: &'static str,
    /// The job that assembles the SHA256SUMS of the job's platform group.
    sums: &'static str,
    /// The job that signs the group's files.
    attest: &'static str,
}

const BUILDS: [Build; 3] = [
    Build {
        job: "darwin-arm64",
        runner: "macos-15",
        asset: "assets-darwin-arm64",
        sums: "sums-darwin-arm64",
        attest: "attest-darwin-arm64",
    },
    Build {
        job: "linux-x86_64",
        runner: "ubuntu-24.04",
        asset: "assets-linux-x86_64",
        sums: "sums-linux",
        attest: "attest-linux",
    },
    Build {
        job: "linux-arm64",
        runner: "ubuntu-24.04-arm",
        asset: "assets-linux-arm64",
        sums: "sums-linux",
        attest: "attest-linux",
    },
];

/// The one `KBF_` variable a build may set: the commit the binary reports.
const ALLOWED_KBF_VARIABLE: &str = "KBF_BUILD_COMMIT";

/// Words (lower case) that mean a farm or remote execution, wherever they appear in a build job.
const FORBIDDEN_WORDS: &[&str] = &["buck2", "--remote", "farm", "remote", "kbf-cargo"];

/// The crate whose binary a broken kbf is rebuilt from: every build job builds it.
const SEED_CRATE: &str = "kbf-cli";

/// What differs between the files and the rules above: one line per difference, empty when
/// there is none. An error when a workflow cannot be read.
pub fn problems(
    artifacts: &str,
    pipeline: &str,
    release: &str,
    doc: &str,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let names = workflows::job_names(artifacts)?;
    for build in &BUILDS {
        if names.iter().any(|n| n == build.job) {
            build_job(artifacts, build, &mut out)?;
            consumers(artifacts, build, &names, &mut out)?;
        } else {
            out.push(format!(
                "the seed build job `{}` is missing from artifacts.yml: the release is always also built with plain cargo, and nothing else may stand in for it",
                build.job
            ));
        }
    }
    sums_all(artifacts, &mut out)?;
    pipeline_chain(pipeline, &mut out)?;
    release_chain(release, &mut out)?;
    if !doc.lines().any(|l| l.trim_end() == DOC_HEADING) || !doc.contains("never instead of") {
        out.push(format!(
            "docs/artifacts.md has no section `{DOC_HEADING}` saying a kbf-on-kbf build is never instead of the seed path"
        ));
    }
    Ok(out)
}

/// Whether the GitHub artifact glob `pattern` (one optional `*`) matches `name`.
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
    }
}

fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Whether `word` is `NAME=value`, the form of an environment assignment before a command.
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The `KBF_` variables named in `text`: identifiers that start with `KBF_` (a name that only
/// contains it, such as `UPGRADE_FROM_KBF_SHA256_X86_64`, is not one).
fn kbf_variables(text: &str) -> Vec<&str> {
    text.match_indices("KBF_")
        .filter(|(at, _)| {
            !text[..*at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(|(at, _)| {
            let rest = &text[at..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .collect()
}

/// The rules for one build job: its runner, its `cargo build` lines and what it may not name.
fn build_job(text: &str, build: &Build, out: &mut Vec<String>) -> Result<(), String> {
    let job = build.job;
    let runner = workflows::job_runs_on(text, job)?;
    if runner.as_deref() != Some(build.runner) {
        out.push(format!(
            "the seed build job `{job}` runs on {runner:?}, not the GitHub-hosted `{}`: it never runs on a self-hosted runner or one the farm owns",
            build.runner
        ));
    }
    let steps = workflows::job_steps(text, job)?;
    let mut seen: Vec<String> = steps
        .iter()
        .flat_map(|s| {
            s.run
                .iter()
                .cloned()
                .chain(s.uses.iter().cloned())
                .chain(s.env.iter().map(|(k, v)| format!("{k}={v}")))
                .chain(s.with.iter().map(|(k, v)| format!("{k}={v}")))
        })
        .collect();
    seen.extend(
        workflows::job_env(text, job)?
            .into_iter()
            .map(|(k, v)| format!("{k}={v}")),
    );
    for text in &seen {
        let lower = text.to_lowercase();
        for word in FORBIDDEN_WORDS {
            if lower.contains(word) {
                out.push(format!(
                    "the seed build job `{job}` names `{word}`: it builds with plain cargo, with no buck2, farm or remote execution"
                ));
            }
        }
        for variable in kbf_variables(text) {
            if variable != ALLOWED_KBF_VARIABLE {
                out.push(format!(
                    "the seed build job `{job}` names `{variable}`: no kbf setting reaches a seed build but {ALLOWED_KBF_VARIABLE}"
                ));
            }
        }
    }
    let mut builds = 0;
    let mut builds_seed_crate = false;
    for step in &steps {
        let script = step.run.as_deref().unwrap_or_default().replace("\\\n", " ");
        for line in script.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let Some(at) = words.windows(2).position(|w| w == ["cargo", "build"]) else {
                continue;
            };
            builds += 1;
            if let Some(front) = words[..at].iter().find(|w| !assignment(w)) {
                out.push(format!(
                    "the seed build job `{job}` runs `{front}` in front of `cargo build`: plain cargo, with only NAME=value assignments before it"
                ));
            }
            for flag in ["--locked", "--release"] {
                if !words.contains(&flag) {
                    out.push(format!(
                        "the seed build job `{job}` has a `cargo build` without `{flag}`: {line}"
                    ));
                }
            }
            builds_seed_crate |= words.windows(2).any(|w| w == ["-p", SEED_CRATE]);
        }
    }
    if builds == 0 {
        out.push(format!(
            "the seed build job `{job}` has no `cargo build` step"
        ));
    }
    if !builds_seed_crate {
        out.push(format!(
            "the seed build job `{job}` does not build `{SEED_CRATE}` (`cargo build -p {SEED_CRATE}`): the kbf a broken kbf is replaced by comes from it"
        ));
    }
    let uploads = steps.iter().any(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/upload-artifact@"))
            && input(s, "name") == Some(build.asset)
            && !s.conditional
    });
    if !uploads {
        out.push(format!(
            "the seed build job `{job}` does not upload `{}`: the sums, the signing and the release take the files from that artifact",
            build.asset
        ));
    }
    Ok(())
}

/// The jobs of `artifacts.yml` that take a build's files: the group's sums and attest jobs.
fn consumers(
    text: &str,
    build: &Build,
    names: &[String],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let needs = workflows::job_needs(text, build.sums)?;
    if !needs.iter().any(|n| n == build.job) {
        out.push(format!(
            "`{}` does not need the seed build job `{}`: the sums are made of what it built",
            build.sums, build.job
        ));
    }
    if !names.iter().any(|n| n == build.attest) {
        out.push(format!("`{}` is missing from artifacts.yml", build.attest));
        return Ok(());
    }
    let steps = workflows::job_steps(text, build.attest)?;
    let consumes = steps.iter().any(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/download-artifact@"))
            && !s.conditional
            && input(s, "run-id").is_none()
            && [input(s, "pattern"), input(s, "name")]
                .into_iter()
                .flatten()
                .any(|p| matches(p, build.asset))
    });
    if !consumes {
        out.push(format!(
            "`{}` does not download `{}` (from this run, with no `run-id`): what is signed must be what the seed build job `{}` built",
            build.attest, build.asset, build.job
        ));
    }
    Ok(())
}

/// The sums job over every platform needs all three builds.
fn sums_all(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let needs = workflows::job_needs(text, "sums")?;
    for build in &BUILDS {
        if !needs.iter().any(|n| n == build.job) {
            out.push(format!(
                "`sums` does not need the seed build job `{}`",
                build.job
            ));
        }
    }
    Ok(())
}

/// In `pipeline.yml`: signing needs the build call, and the run is green only with it.
fn pipeline_chain(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let calls = workflows::job_uses(text, "artifacts")?;
    if calls.as_deref() != Some("./.github/workflows/artifacts.yml") {
        out.push(format!(
            "the pipeline job `artifacts` calls {calls:?}, not artifacts.yml: the seed build jobs are in that file"
        ));
    }
    for (job, wants) in [
        ("artifacts-sign", &["artifacts"][..]),
        ("passed", &["artifacts", "artifacts-sign"][..]),
    ] {
        let needs = workflows::job_needs(text, job)?;
        for want in wants {
            if !needs.iter().any(|n| n == want) {
                out.push(format!(
                    "the pipeline job `{job}` does not need `{want}`: nothing is signed or released without the cargo build call"
                ));
            }
        }
    }
    Ok(())
}

/// In `release.yml`: `verify` takes the pipeline run's build files and `publish` needs it.
fn release_chain(text: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(text, "verify")?;
    for build in &BUILDS {
        let downloads = steps.iter().any(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/download-artifact@"))
                && !s.conditional
                && input(s, "run-id").is_some()
                && input(s, "pattern").is_some_and(|p| matches(p, build.asset))
        });
        if !downloads {
            out.push(format!(
                "the release job `verify` does not download `{}` from the pipeline run (`run-id`): the release is made of what the seed build job `{}` built",
                build.asset, build.job
            ));
        }
    }
    if !workflows::job_needs(text, "publish")?
        .iter()
        .any(|n| n == "verify")
    {
        out.push(
            "the release job `publish` does not need `verify`: nothing is published unless the build's files were checked"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
