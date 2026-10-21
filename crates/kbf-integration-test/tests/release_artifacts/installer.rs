//! `kbf-installer` in the pinned release set, on the real workflow with one of its steps
//! taken out at a time.
//!
//! The installer is the one root tool of a host and is built, attested and released like
//! every component, so a tarball of it per platform has to reach `dist/` with everything
//! the others have. Each lint reads the workflow for its own part of that, and none of
//! them runs an attestation on a pull request (the attest and verify jobs run on a push):
//! a step of the installer that goes missing would pass the pull request and turn the
//! branch red, or release the tarball without a bundle. So the test below takes the real
//! `artifacts.yml`, removes the installer's steps one kind at a time, on every platform,
//! and requires the lint that owns that kind to name the installer:
//!
//! - the package line: the pinned set (`release_artifacts`), and the SBOM attestation and
//!   the provenance step left behind (`attested_assets`, `attested_bundles`);
//! - the SBOM attestation (`attested_assets`);
//! - the provenance step of its own, and the step that names its bundle
//!   (`attested_bundles`);
//! - `build-commit check --json` of the binary built and `build-commit check-asset` of
//!   the tarball (`release_artifacts`' identity rule);
//! - on macOS the signature before and `darwin-asset` after (`release_artifacts`' darwin
//!   rule).

use super::*;

const INSTALLER: &str = "kbf-installer";

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// `workflow` without the first step of `job` whose text holds every one of `needles`.
///
/// A job runs from its `  <job>:` line to the next line indented by exactly two spaces
/// (the next job, or a comment between jobs); a step from a `      - ` line to the next
/// one or the end of the job. Panics when there is no such job or step: a plant that
/// removes nothing is a failure of the test, not a quiet pass.
fn drop_step(workflow: &str, job: &str, needles: &[&str]) -> String {
    let lines: Vec<&str> = workflow.split_inclusive('\n').collect();
    let header = format!("  {job}:");
    let start = lines
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("no job `{job}` in the workflow"));
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.starts_with("  ") && !l.starts_with("   "))
        .map_or(lines.len(), |(i, _)| i);
    let starts: Vec<usize> = (start..end)
        .filter(|&i| lines[i].starts_with("      - "))
        .collect();
    for (n, &from) in starts.iter().enumerate() {
        let to = starts.get(n + 1).copied().unwrap_or(end);
        let block = lines[from..to].concat();
        if needles.iter().all(|needle| block.contains(needle)) {
            return [&lines[..from], &lines[to..]].concat().concat();
        }
    }
    panic!("no step of `{job}` holds all of {needles:?}")
}

/// What every lint that reads `artifacts` says (with the real `release.yml`): the pinned
/// set and its rules, the SBOM attestations, the provenance bundles.
fn all_problems(artifacts: &str) -> Vec<String> {
    let release = read(".github/workflows/release.yml");
    let mut out = problems(artifacts).expect("release_artifacts reads the workflow");
    out.extend(
        attested_assets::unattested(artifacts, ATTEST_JOBS)
            .expect("attested_assets reads the workflow"),
    );
    out.extend(
        attested_bundles::gaps(artifacts, &release, &xtask::release::ARTIFACTS)
            .expect("attested_bundles reads the workflow"),
    );
    out
}

/// The attest job that signs the tarballs of `platform`.
fn attest_job(platform: &str) -> &'static str {
    ATTEST_JOBS
        .iter()
        .copied()
        .find(|job| {
            job.strip_prefix("attest-")
                .is_some_and(|group| platform.starts_with(group))
        })
        .expect("an attest job for the platform")
}

#[test]
fn the_installer_is_pinned_for_every_platform_and_the_real_workflow_passes() {
    // Catches: the installer dropped from a platform of the pinned set (or from the
    // identified binaries, which would leave it unchecked), and a real workflow that
    // some lint refuses: the control for the plants below, which would otherwise go red
    // for a reason of their own.
    for platform in BUILD_JOBS {
        assert!(
            ASSETS.contains(&(INSTALLER, *platform)),
            "{INSTALLER} for {platform} is not in ASSETS"
        );
    }
    assert!(IDENTIFIED.contains(&INSTALLER), "{IDENTIFIED:?}");
    let real = read(crate::ARTIFACTS_WORKFLOW);
    assert_eq!(all_problems(&real), Vec::<String>::new());
}

#[test]
fn each_lint_names_the_installer_step_that_is_missing() {
    // Catches: an installer tarball that is packaged without the rest of what a
    // tarball gets, on any platform: its package line, its SBOM attestation, a
    // provenance step of its own, the step that names its bundle, the check of
    // `kbf-installer --version --json` before packaging and `check-asset` of the
    // tarball after, and on macOS the signature before and `darwin-asset` after. Each
    // plant must be named by the lint that owns it and leave nothing unrelated to the
    // installer broken (the plant took out the one step and no more).
    let real = read(crate::ARTIFACTS_WORKFLOW);
    let mut unnamed = Vec::new();
    for platform in BUILD_JOBS {
        let job = *platform;
        let attest = attest_job(platform);
        let stem = attested_assets::stem(INSTALLER, platform);
        let tarball = format!("dist/{stem}.tar.gz");
        let released = format!("dist/{INSTALLER}-$GITHUB_SHA-{platform}.tar.gz");
        let bundle = format!("dist/{INSTALLER}-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json");
        let binary = format!("release/{INSTALLER}\"");
        let mut plants: Vec<(&str, String, Vec<String>)> = vec![
            (
                "its package line",
                drop_step(&real, job, &["cargo xtask package", &binary]),
                vec![
                    format!(
                        "{INSTALLER} for {platform} is in the release set, but no `cargo xtask package` line packages it"
                    ),
                    format!("attests {tarball} with dist/{stem}.cdx.json, a pair no job packages"),
                    format!(
                        "`{attest}` attests {tarball} with a provenance step of its own, a tarball no job of its platform group packages"
                    ),
                ],
            ),
            (
                "its SBOM attestation",
                drop_step(
                    &real,
                    attest,
                    &["actions/attest-sbom@", &format!("subject-path: {tarball}")],
                ),
                vec![format!(
                    "{INSTALLER} for {platform} is packaged, but `attest-linux` or `attest-darwin-arm64` has no attest-sbom step with subject-path {tarball} and sbom-path dist/{stem}.cdx.json"
                )],
            ),
            (
                "its provenance step",
                drop_step(
                    &real,
                    attest,
                    &[
                        "actions/attest-build-provenance@",
                        &format!("subject-path: {tarball}"),
                    ],
                ),
                vec![format!(
                    "{INSTALLER} for {platform} is packaged, but `{attest}` has no actions/attest-build-provenance step with subject-path {tarball}"
                )],
            ),
            (
                "the step that names its bundle",
                drop_step(&real, attest, &["cp \"$BUNDLE\"", &bundle]),
                vec![format!(
                    "the step after the provenance step for {INSTALLER} for {platform} in `{attest}` does not name its bundle"
                )],
            ),
            (
                "the check of the binary built",
                drop_step(&real, job, &["build-commit check --json", &binary]),
                vec![format!(
                    "`{job}` packages {INSTALLER} without `cargo xtask build-commit check --json <{INSTALLER}> \"$GITHUB_SHA\"` before"
                )],
            ),
            (
                "the check of the tarball",
                drop_step(&real, job, &["build-commit check-asset", &released]),
                vec![format!(
                    "`{job}` packages {INSTALLER} without `cargo xtask build-commit check-asset \"{released}\" \"$GITHUB_SHA\"` in a step after"
                )],
            ),
        ];
        if *platform == "darwin-arm64" {
            plants.push((
                "its signature",
                drop_step(&real, job, &["cargo xtask sign-darwin", &binary]),
                vec![format!(
                    "`{job}` packages {INSTALLER} without a `cargo xtask sign-darwin` of it in an unconditional step before"
                )],
            ));
            plants.push((
                "the check of the signed tarball",
                drop_step(&real, job, &["cargo xtask darwin-asset", &released]),
                vec![format!(
                    "`{job}` packages {INSTALLER} without a `cargo xtask darwin-asset` of its tarball in an unconditional step after"
                )],
            ));
        }
        for (what, planted, wanted) in plants {
            assert_ne!(planted, real, "{platform}: {what} is in the workflow");
            let got = all_problems(&planted);
            for want in &wanted {
                if !got.iter().any(|p| p.contains(want.as_str())) {
                    unnamed.push(format!(
                        "{platform}, without {what}: wanted a problem containing {want:?}, got {got:#?}"
                    ));
                }
            }
            // The installer's bundle is the last of its group's, so without the step that
            // names it the upload follows its provenance step directly: that rule is
            // right to say so, and it does not name a tarball.
            let about_the_installer = |p: &&String| {
                p.contains(INSTALLER) || p.contains("comes before the last bundle is named")
            };
            if let Some(other) = got.iter().find(|p| !about_the_installer(p)) {
                unnamed.push(format!(
                    "{platform}, without {what}: a problem that does not name the installer, so the plant broke more than one step: {other:?}"
                ));
            }
        }
    }
    assert!(unnamed.is_empty(), "{}", unnamed.join("\n"));
}
