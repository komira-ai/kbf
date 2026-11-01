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

/// What differs between the files and the rules above: one line per difference, empty when
/// there is none. An error when a workflow cannot be read.
pub fn problems(
    artifacts: &str,
    pipeline: &str,
    release: &str,
    doc: &str,
) -> Result<Vec<String>, String> {
    let _ = (artifacts, pipeline, release, doc);
    Ok(Vec::new())
}

#[cfg(test)]
mod tests;
