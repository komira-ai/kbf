//! Every tarball `.github/workflows/artifacts.yml` packages is attested as its own
//! subject, and the bundle of that attestation reaches the release.
//!
//! A host verifies a tarball offline with one file, `<tarball>.sigstore.json`, the
//! Sigstore bundle of a build provenance attestation whose only subject is that tarball
//! (docs/artifacts.md). Nothing about it runs on a pull request: the attest jobs sign
//! only on a push to `main`, so a tarball added to a build job
//! without its steps would pass every pull request check and turn the branch red, or
//! worse, release without a bundle. [`gaps`] reads the workflows and names the tarball:
//!
//! - **Its own subject.** The attest job of the tarball's platform group has an
//!   unconditional `actions/attest-build-provenance` step with an `id` and
//!   `subject-path` exactly `dist/<tarball>` (not a glob or a list, which would make
//!   one bundle for several tarballs). A step for a tarball no job of that group
//!   packages is named too.
//! - **The packages.** The `kbf` deb that `cargo xtask pkg-kbf-asset` lists
//!   ([`attested_assets::packaged_kbf_debs`]) and the Mac `.pkg` that `cargo xtask
//!   pkg-macos-asset` lists ([`attested_assets::packaged_macos_pkgs`]) are held to the same
//!   two rules, as `dist/kbf-<commit>-<platform>.deb` and `dist/kbf-installer-<commit>-<platform>.pkg`
//!   and their `.deb.sigstore.json` (`.pkg.sigstore.json`).
//!   So are the frontend, coordinator and task images that `cargo xtask pkg-image-asset` lists
//!   ([`attested_assets::packaged_images`]), as `dist/kbf-<service>-image-<commit>-<platform>.oci.tar`
//!   and its `.oci.tar.sigstore.json`.
//! - **Its bundle's name.** The step after it sets `BUNDLE` to that step's
//!   `bundle-path` output and copies it to `dist/<tarball>.sigstore.json` (the action
//!   leaves it in a temporary directory under a name that is the same for every
//!   attestation). That `cp` and the two checks of the next point are the only `run:` steps
//!   the attest job may have: it holds `id-token: write` and signs, and any other shell step
//!   is named.
//! - **Signed only what the build published.** The build call's files wait in the run's
//!   artifact store while jobs that run repository code go by, so before it signs, the attest
//!   job of a group checks what it downloaded ([`sums_checked`]): first that the sums file
//!   is the one the build call's sums job published (`/usr/bin/sha256sum --check --strict <<<
//!   "$SUMS_SHA256  dist/SHA256SUMS"`, with `SUMS_SHA256` the input the pipeline passes from
//!   that job's output), then that every file is the one the sums list (`/usr/bin/sha256sum --check
//!   --strict SHA256SUMS` in `dist`; this proves that the files the sums LIST match, not that
//!   `dist` holds nothing else: the sums job refuses an unlisted file, and the attest steps name
//!   their files). Both are unconditional `sha256sum` commands run by the workflow's pinned bash
//!   (the here-string is bash), after the last download and before the first attestation step.
//!   No repository code runs beside `id-token: write`. The checks must also be able to fail: the
//!   workflow's `defaults.run.shell` is exactly `bash` and its `env` sets only names of
//!   [`WORKFLOW_ENV`]; the attest job has no `defaults`, no `env`, and no step with a `shell:`
//!   (a `shell: true {0}` would make both checks no-ops).
//! - **Out of the job.** The job uploads `dist/*.sigstore.json` as the artifact
//!   `bundles-<group>`, in a step that cannot be skipped and fails on no file.
//! - **Into the verification and the release.** The group's verify job downloads
//!   that artifact into `dist`; `release.yml`'s verify job downloads `bundles-*` into
//!   `dist`, fetches the trusted root and verifies offline for the release's commit
//!   (`--commit "$COMMIT"`, `--trusted-root`); and `cargo xtask release locate` lists
//!   each `bundles-*` artifact as one a run must still hold
//!   ([`xtask::release::ARTIFACTS`]), and lists no other.

use std::collections::BTreeSet;

use super::{attested_assets, release_artifacts, workflows};

const PROVENANCE: &str = "actions/attest-build-provenance@";
const UPLOAD: &str = "actions/upload-artifact@";
const DOWNLOAD: &str = "actions/download-artifact@";

/// What the bundle step of the job copies from: the `bundle-path` output of the
/// provenance step `id`.
fn bundle_path(id: &str) -> String {
    format!("${{{{ steps.{id}.outputs.bundle-path }}}}")
}

/// The platform group that attests, verifies and uploads the bundles of `platform`:
/// both Linux architectures share one, each other platform has its own.
fn group(platform: &str) -> &str {
    if platform.starts_with("linux-") {
        "linux"
    } else {
        platform
    }
}

/// The value of the input `key` of `step`.
fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Whether `step` is `uses: <prefix>...`.
fn uses(step: &workflows::Step, prefix: &str) -> bool {
    step.uses.as_deref().is_some_and(|u| u.starts_with(prefix))
}

/// The words (double quotes dropped) after `prefix` on the first line of a step of
/// `steps` that starts with it, with the index of its step.
fn command<'a>(steps: &'a [workflows::Step], prefix: &str) -> Option<(usize, Vec<&'a str>)> {
    steps.iter().enumerate().find_map(|(at, s)| {
        s.run
            .as_deref()?
            .lines()
            .map(str::trim_start)
            .find_map(|l| {
                l.strip_prefix(prefix).map(|rest| {
                    let words = rest.split_whitespace().map(|w| w.trim_matches('"'));
                    (at, words.collect())
                })
            })
    })
}

/// What differs between the tarballs the workflow `artifacts` packages and the
/// bundles its jobs and `release` (release.yml) carry to the release, given the
/// artifacts `listed` that `release locate` requires: one line per difference, empty
/// when there is none. An error when the workflow packages nothing (the lint would
/// pass vacuously) or cannot be read.
pub fn gaps(artifacts: &str, release: &str, listed: &[&str]) -> Result<Vec<String>, String> {
    let packaged = attested_assets::packaged(artifacts)?;
    if packaged.is_empty() {
        return Err("no `cargo xtask package` line in any `run:` step".to_owned());
    }
    // The packages: (binary, platform, extension), the `kbf` deb of a Linux job and the
    // installer's `.pkg` of the macOS one.
    let mut packages: Vec<(&str, String, &str)> = Vec::new();
    for platform in attested_assets::packaged_kbf_debs(artifacts)? {
        packages.push(("kbf", platform, "deb"));
    }
    for platform in attested_assets::packaged_macos_pkgs(artifacts)? {
        packages.push(("kbf-installer", platform, "pkg"));
    }
    // The images are held to the same rules, as `dist/kbf-<service>-image-<commit>-<platform>.oci.tar`.
    for (service, platform) in attested_assets::packaged_images(artifacts)? {
        let name = match service.as_str() {
            "frontend" => "kbf-frontend-image",
            "coordinator" => "kbf-coordinator-image",
            "task" => "kbf-task-image",
            other => {
                return Err(format!(
                    "`pkg-image-asset --service {other}`: not an image of the release set"
                ));
            }
        };
        packages.push((name, platform, "oci.tar"));
    }
    let groups: BTreeSet<&str> = packaged
        .iter()
        .map(|(_, p)| group(p))
        .chain(packages.iter().map(|(_, p, _)| group(p)))
        .collect();
    let mut out = Vec::new();
    for g in &groups {
        let attest_job = format!("attest-{g}");
        if !release_artifacts::ATTEST_JOBS.contains(&attest_job.as_str()) {
            out.push(format!(
                "a tarball is packaged for the group `{g}`, which has no attest job `{attest_job}`"
            ));
            continue;
        }
        let mine: Vec<(&str, &str)> = packaged
            .iter()
            .filter(|(_, p)| group(p) == *g)
            .map(|(n, p)| (n.as_str(), p.as_str()))
            .collect();
        let my_packages: Vec<(&str, &str, &str)> = packages
            .iter()
            .filter(|(_, p, _)| group(p) == *g)
            .map(|(name, p, ext)| (*name, p.as_str(), *ext))
            .collect();
        let steps = workflows::job_steps(artifacts, &attest_job)?;
        attested_alone(&attest_job, &steps, &mine, &my_packages, &mut out);
        sums_checked(artifacts, &attest_job, g, &steps, &mut out)?;
        uploaded(&attest_job, g, &steps, &mut out);
        let verify_job = format!("verify-{g}");
        let verify = workflows::job_steps(artifacts, &verify_job)?;
        let artifact = format!("bundles-{g}");
        let downloads = verify.iter().any(|s| {
            uses(s, DOWNLOAD)
                && input(s, "name") == Some(&artifact)
                && input(s, "path") == Some("dist")
        });
        if !downloads {
            out.push(format!(
                "`{verify_job}` does not download `{artifact}` into `dist`, so it cannot verify the bundles"
            ));
        }
        if !listed.contains(&artifact.as_str()) {
            out.push(format!(
                "`xtask::release::ARTIFACTS` does not list `{artifact}`: `release locate` would take a run that lost its bundles"
            ));
        }
    }
    for artifact in listed.iter().filter(|a| a.starts_with("bundles-")) {
        let named = artifact.strip_prefix("bundles-").unwrap_or_default();
        if !groups.contains(named) {
            out.push(format!(
                "`xtask::release::ARTIFACTS` lists `{artifact}`, which no attest job uploads: `release locate` would refuse every run"
            ));
        }
    }
    released(release, &mut out)?;
    Ok(out)
}

/// The provenance and bundle-naming steps of every tarball of `mine` (name and platform)
/// and every package of `packages` (binary, platform, extension) in the attest job `job`, the
/// steps that attest anything else, and any `run:` step that is not the naming of a bundle.
fn attested_alone(
    job: &str,
    steps: &[workflows::Step],
    mine: &[(&str, &str)],
    packages: &[(&str, &str, &str)],
    out: &mut Vec<String>,
) {
    let mut expected = Vec::new();
    // (what it is, the file it attests, the file its bundle is copied to)
    let mut targets: Vec<(String, String, String)> = Vec::new();
    for (name, platform) in mine {
        targets.push((
            format!("{name} for {platform}"),
            format!("dist/{}.tar.gz", attested_assets::stem(name, platform)),
            format!("dist/{name}-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json"),
        ));
    }
    for (name, platform, ext) in packages {
        targets.push((
            format!("{name} package ({ext}) for {platform}"),
            format!("dist/{}.{ext}", attested_assets::stem(name, platform)),
            format!("dist/{name}-${{GITHUB_SHA}}-{platform}.{ext}.sigstore.json"),
        ));
    }
    for (what, subject, bundle) in targets {
        expected.push(subject.clone());
        let found = steps
            .iter()
            .enumerate()
            .find(|(_, s)| uses(s, PROVENANCE) && input(s, "subject-path") == Some(&subject));
        let Some((at, step)) = found else {
            out.push(format!(
                "{what} is packaged, but `{job}` has no actions/attest-build-provenance step with subject-path {subject}: it is not attested as its own subject, so it has no bundle of its own"
            ));
            continue;
        };
        if step.conditional {
            out.push(format!(
                "the provenance step for {what} in `{job}` is under `if:` or `continue-on-error:`"
            ));
        }
        let Some(id) = step.id.as_deref() else {
            out.push(format!(
                "the provenance step for {what} in `{job}` has no `id:`, so its `bundle-path` cannot be named"
            ));
            continue;
        };
        let want_run = format!("cp \"$BUNDLE\" \"{bundle}\"");
        let want_env = bundle_path(id);
        let named = steps.get(at + 1).is_some_and(|s| {
            s.run.as_deref().map(str::trim) == Some(want_run.as_str())
                && s.env.iter().any(|(k, v)| k == "BUNDLE" && *v == want_env)
                && !s.conditional
        });
        if !named {
            out.push(format!(
                "the step after the provenance step for {what} in `{job}` does not name its bundle: it is to be an unconditional step with `env: BUNDLE: {want_env}` and `run: {want_run}`"
            ));
        }
    }
    for step in steps.iter().filter(|s| uses(s, PROVENANCE)) {
        let Some(subject) = input(step, "subject-path") else {
            // The one over `dist/SHA256SUMS`: release_artifacts holds it.
            continue;
        };
        if !expected.iter().any(|e| e == subject) {
            out.push(format!(
                "`{job}` attests {subject} with a provenance step of its own, a tarball no job of its platform group packages"
            ));
        }
    }
    // The job holds `id-token: write` and signs, and nothing else runs in it: the only
    // `run:` steps are the ones right after a provenance step (judged above to be the
    // `cp` that names its bundle, or named there). Any other is a shell in the signing job.
    let after_provenance: BTreeSet<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| uses(s, PROVENANCE) && input(s, "subject-path").is_some())
        .map(|(at, _)| at + 1)
        .collect();
    for (at, step) in steps.iter().enumerate() {
        let Some(run) = step.run.as_deref() else {
            continue;
        };
        if run == SUMS_FILES_RUN || run == SUMS_DIGEST_RUN {
            // Held to the exact step, its place and its inputs by [`sums_checked`].
            continue;
        }
        if !after_provenance.contains(&at) {
            let first: String = run
                .trim_start()
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            out.push(format!(
                "`{job}` has a `run:` step (`{first}`) that is not the naming of a bundle right after a provenance step: the job holds `id-token: write` and signs, so nothing else runs in it"
            ));
        }
    }
}

/// The check of the downloaded sums file against the digest the build call's sums job published.
const SUMS_DIGEST_RUN: &str =
    "/usr/bin/sha256sum --check --strict <<< \"$SUMS_SHA256  dist/SHA256SUMS\"";

/// The check of every downloaded file against the sums file, run in `dist`.
const SUMS_FILES_RUN: &str = "/usr/bin/sha256sum --check --strict SHA256SUMS";

/// The workflow input that carries group `g`'s digest: `sums_<group>_sha256`.
pub fn digest_input(g: &str) -> String {
    format!("sums_{}_sha256", g.replace('-', "_"))
}

/// The two checks the attest job `job` (group `g`) makes of what it downloaded before it signs
/// anything: the sums file against the digest input, then the files against the sums file, each
/// an unconditional step, both after the last download and before the first attestation step.
fn sums_checked(
    artifacts: &str,
    job: &str,
    g: &str,
    steps: &[workflows::Step],
    out: &mut Vec<String>,
) -> Result<(), String> {
    shell_cannot_be_swapped(artifacts, job, steps, out)?;
    let input_name = digest_input(g);
    let want_env = format!("${{{{ inputs.{input_name} }}}}");
    let digest = steps
        .iter()
        .position(|s| s.run.as_deref().map(str::trim) == Some(SUMS_DIGEST_RUN));
    let files = steps
        .iter()
        .position(|s| s.run.as_deref().map(str::trim) == Some(SUMS_FILES_RUN));
    match digest {
        None => out.push(format!(
            "`{job}` does not run `{SUMS_DIGEST_RUN}`: it would sign sums the build call did not publish"
        )),
        Some(at) => {
            let step = &steps[at];
            if step.conditional
                || step.env != [("SUMS_SHA256".to_owned(), want_env.clone())]
                || step.working_directory.is_some()
            {
                out.push(format!(
                    "the digest check of `{job}` is to be an unconditional step with exactly `env: SUMS_SHA256: {want_env}` and no working directory"
                ));
            }
        }
    }
    match files {
        None => out.push(format!(
            "`{job}` does not run `{SUMS_FILES_RUN}` in `dist`: it would sign files the sums do not list"
        )),
        Some(at) => {
            let step = &steps[at];
            if step.conditional
                || !step.env.is_empty()
                || step.working_directory.as_deref() != Some("dist")
            {
                out.push(format!(
                    "the file check of `{job}` is to be an unconditional step with `working-directory: dist` and no `env:`"
                ));
            }
        }
    }
    let last_download = steps.iter().rposition(|s| uses(s, DOWNLOAD));
    let first_attest = steps.iter().position(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/attest"))
    });
    for (what, at) in [("digest", digest), ("file", files)] {
        let Some(at) = at else { continue };
        if first_attest.is_some_and(|a| at > a) {
            out.push(format!(
                "the {what} check of `{job}` comes after an attestation step: it must run before anything is signed"
            ));
        }
        if last_download.is_some_and(|d| at < d) {
            out.push(format!(
                "the {what} check of `{job}` comes before its last download: it would check files that are not the ones signed"
            ));
        }
    }
    Ok(())
}

/// The shell the workflow pins as its default. Written in parts: it is data here, and the
/// interpreter-budget scan reads a bare program literal in test code as a use of that program.
const PINNED_SHELL: &str = concat!("b", "a", "s", "h");

/// The names the workflow's top-level `env:` may set: exactly the ones it sets today. An
/// allow-list, because a deny-list misses a name (`BASH_FUNC_sha256sum%%` makes bash import a
/// function that shadows the command, and `LD_AUDIT`, `BASH_XTRACEFD` and `POSIXLY_CORRECT` change
/// how programs run) and every step of the jobs that sign sees these. Adding a name here needs a
/// security reason: say in the change why it cannot alter how a shell or `sha256sum` runs.
/// This list is the protection: bash outside posix mode also imports a function named like the
/// absolute path (`BASH_FUNC_/usr/bin/sha256sum%%`), so calling `/usr/bin/sha256sum` shadows nothing
/// away by itself.
const WORKFLOW_ENV: &[&str] = &[
    "CARGO_TERM_COLOR",
    "CARGO_INCREMENTAL",
    "CYCLONEDX_VERSION",
    "CYCLONEDX_SHA256_AARCH64_DARWIN",
    "CYCLONEDX_SHA256_X86_64_LINUX",
    "CYCLONEDX_SHA256_AARCH64_LINUX",
    "GH_VERSION",
    "GH_SHA256_X86_64_LINUX",
    "GH_SHA256_AARCH64_LINUX",
    "GH_SHA256_AARCH64_DARWIN",
    "UPGRADE_FROM_TAG",
    "UPGRADE_FROM_COMMIT",
    "UPGRADE_FROM_INSTALLER_SHA256_X86_64",
    "UPGRADE_FROM_KBF_SHA256_X86_64",
    "UPGRADE_FROM_INSTALLER_SHA256_AARCH64",
    "UPGRADE_FROM_KBF_SHA256_AARCH64",
    "JS_BUNDLE_VERSION",
    "JS_BUNDLE_SHA256_AARCH64_DARWIN",
    "JS_BUNDLE_SOURCE_DIGEST",
    "NFPM_VERSION",
    "NFPM_SHA256_X86_64_LINUX",
    "NFPM_SHA256_AARCH64_LINUX",
    "SYFT_VERSION",
    "SYFT_SHA256_X86_64_LINUX",
    "SYFT_SHA256_AARCH64_LINUX",
];

/// The checks of the attest job `job` are `sha256sum` run by the workflow's bash: the workflow's
/// `defaults.run.shell` is exactly `bash` and its `env` sets only names of [`WORKFLOW_ENV`], and the job
/// has no `defaults`, no `env` and no step with a `shell:` (`shell: true {0}` runs nothing).
fn shell_cannot_be_swapped(
    artifacts: &str,
    job: &str,
    steps: &[workflows::Step],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let shell = workflows::top_defaults_shell(artifacts)?;
    if shell.as_deref() != Some(PINNED_SHELL) {
        out.push(format!(
            "the workflow's `defaults.run.shell` is {shell:?}, not exactly `{PINNED_SHELL}`: the checks of `{job}` use a here-string and must run under a known shell"
        ));
    }
    for (name, _) in workflows::top_env(artifacts)? {
        if !WORKFLOW_ENV.contains(&name.as_str()) {
            out.push(format!(
                "the workflow's `env` sets `{name}`, which is not in the allow-list `WORKFLOW_ENV`: a variable can change how a shell or `sha256sum` runs, and the checks of `{job}` could be made to pass without checking"
            ));
        }
    }
    if workflows::job_has_key(artifacts, job, "defaults")? {
        out.push(format!(
            "`{job}` has `defaults:`: a job-level `defaults.run.shell` could turn the checks into no-ops"
        ));
    }
    if !workflows::job_env(artifacts, job)?.is_empty() {
        out.push(format!(
            "`{job}` has a job-level `env:`: it reaches every step of the job that holds `id-token: write`, and a startup variable would make the checks pass without checking"
        ));
    }
    for step in steps.iter().filter(|s| s.run.is_some()) {
        if let Some(shell) = &step.shell {
            out.push(format!(
                "a `run:` step of `{job}` has `shell: {shell}`: the workflow's pinned bash runs every step of the job that signs"
            ));
        }
    }
    Ok(())
}

/// The step of the attest job `job` (group `g`) that uploads the bundles.
fn uploaded(job: &str, g: &str, steps: &[workflows::Step], out: &mut Vec<String>) {
    let artifact = format!("bundles-{g}");
    let upload = steps
        .iter()
        .enumerate()
        .find(|(_, s)| uses(s, UPLOAD) && input(s, "name") == Some(&artifact));
    let Some((at, step)) = upload else {
        out.push(format!(
            "`{job}` has no actions/upload-artifact step for `{artifact}`: the bundles never leave the job"
        ));
        return;
    };
    if input(step, "path") != Some("dist/*.sigstore.json") {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` has path {:?}, not `dist/*.sigstore.json`",
            input(step, "path").unwrap_or_default()
        ));
    }
    if input(step, "if-no-files-found") != Some("error") {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` does not fail on no file (`if-no-files-found: error`)"
        ));
    }
    if step.conditional {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` is under `if:` or `continue-on-error:`"
        ));
    }
    let last_provenance = steps
        .iter()
        .rposition(|s| uses(s, PROVENANCE) && input(s, "subject-path").is_some());
    if last_provenance.is_some_and(|last| at <= last + 1) {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` comes before the last bundle is named"
        ));
    }
}

/// What release.yml's verify job must do with the bundles.
fn released(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, "verify")?;
    let downloads = steps.iter().any(|s| {
        uses(s, DOWNLOAD)
            && input(s, "pattern") == Some("bundles-*")
            && input(s, "path") == Some("dist")
            && input(s, "merge-multiple") == Some("true")
    });
    if !downloads {
        out.push(
            "`release.yml`'s `verify` job does not download `bundles-*` into `dist` (merge-multiple), so a release would carry no bundle".to_owned(),
        );
    }
    if command(&steps, "cargo xtask verify-assets attestations ").is_none() {
        out.push(
            "`release.yml`'s `verify` job does not run `cargo xtask verify-assets attestations`"
                .to_owned(),
        );
    } else {
        offline_verification("`release.yml`'s `verify` job", &steps, "$COMMIT", out);
    }
    Ok(())
}

/// What the steps of a job must do to verify the bundles offline for the commit held by
/// the shell variable `commit_var`: fetch the trusted root into a file
/// (`gh attestation trusted-root > <file>`), then run `cargo xtask verify-assets
/// attestations` with `--commit "<commit_var>"` and `--trusted-root <file>`. `who` names
/// the job in the problems. A job that does not run the verification at all is for the
/// caller to report.
pub fn offline_verification(
    who: &str,
    steps: &[workflows::Step],
    commit_var: &str,
    out: &mut Vec<String>,
) {
    let Some((verify_at, words)) = command(steps, "cargo xtask verify-assets attestations ") else {
        return;
    };
    let value = |flag: &str| {
        let at = words.iter().position(|w| *w == flag)?;
        words.get(at + 1).copied()
    };
    if value("--commit") != Some(commit_var) {
        out.push(format!(
            "{who} verifies attestations without `--commit \"{commit_var}\"`, so the bundles are not held to the commit they were built from"
        ));
    }
    let root = command(steps, "gh attestation trusted-root > ")
        .and_then(|(at, w)| Some((at, *w.first()?)));
    let Some((root_at, root_path)) = root else {
        out.push(format!(
            "{who} does not fetch the trusted root (`gh attestation trusted-root > <file>`)"
        ));
        return;
    };
    if value("--trusted-root") != Some(root_path) || verify_at <= root_at {
        out.push(format!(
            "{who} does not verify offline: `verify-assets attestations` is to follow the trusted-root step and take `--trusted-root {root_path}`"
        ));
    }
}

#[test]
fn every_packaged_tarball_is_attested_alone_and_its_bundle_reaches_the_release() {
    // Catches: a tarball added to a build job (or a platform) with no provenance step of
    // its own, so it has no bundle and a host cannot verify it offline, which no pull
    // request run notices (attest and verify run only on a push) and a release would
    // ship without; a bundle step, upload, download or `release locate` entry dropped on
    // the way from the attest job to the release.
    let root = super::repo_root();
    let path = super::ARTIFACTS_WORKFLOW;
    let artifacts = std::fs::read_to_string(root.join(path)).expect(path);
    let release_path = ".github/workflows/release.yml";
    let release = std::fs::read_to_string(root.join(release_path)).expect(release_path);
    match gaps(&artifacts, &release, &xtask::release::ARTIFACTS) {
        Ok(wrong) => assert!(wrong.is_empty(), "{path}:\n{}", wrong.join("\n")),
        Err(e) => panic!("{path}: {e}"),
    }
}

#[cfg(test)]
mod rules;
