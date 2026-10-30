//! The CycloneDX SBOM of the task image in the pinned release set (docs/artifacts.md): the SBOM
//! of the `kbf-task` tarball describes the binary, so it says nothing of the Ubuntu base's
//! packages the image carries. Each Linux build job therefore reads the image it made an asset
//! with a pinned syft and makes `kbf-task-image-<commit>-<platform>.cdx.json` an asset beside it
//! (`cargo xtask pkg-image-sbom`); the attest job signs an SBOM attestation of the image with it
//! (`attested_assets` names a job that does not), and the verify job checks the SBOM beside each
//! signed image. No pull request run attests or verifies anything, so a step that goes missing
//! would turn `main` red, or release an image whose packages nobody listed. [`rules`] reads the
//! workflow and names a Linux job that:
//!
//! - does not make the SBOM exactly once, or makes it with another service, commit, platform,
//!   image, syft path or time (`$SOURCE_DATE_EPOCH`, the commit's, which `images` already needs in
//!   the environment) than its own, or runs it under `if:` or `continue-on-error:`;
//! - makes it before the task image is an asset (`cargo xtask pkg-image-asset --service task`);
//! - makes it without, in an earlier step that cannot be skipped, fetching syft: the archive of
//!   its architecture from the project's release of the pinned version, by the digest of that
//!   archive (`$SYFT_SHA256_X86_64_LINUX` or `$SYFT_SHA256_AARCH64_LINUX`, which `fetch_pins`
//!   holds to a workflow variable of 64 hex digits), extracted to the path `--syft` names;
//!
//! and names the verify job when it does not run `pkg-image-sbom --verify-only` on each signed
//! task image, with the commit, in an unconditional step after the attestation check.

use super::*;

/// The command that makes the SBOM an asset, and the one that checks the signed one.
const VERB: &str = "cargo xtask pkg-image-sbom ";

/// The command that makes the image an asset.
const IMAGE_VERB: &str = "cargo xtask pkg-image-asset ";

/// Where the jobs fetch syft to.
const SYFT: &str = "$RUNNER_TEMP/tools/syft";

/// The archive of syft for the job `platform`, and the variable that holds its digest.
fn archive(platform: &str) -> (String, &'static str) {
    let (arch, var) = match platform {
        "linux-arm64" => ("linux_arm64", "SYFT_SHA256_AARCH64_LINUX"),
        _ => ("linux_amd64", "SYFT_SHA256_X86_64_LINUX"),
    };
    (
        format!(
            "https://github.com/anchore/syft/releases/download/v$SYFT_VERSION/syft_${{SYFT_VERSION}}_{arch}.tar.gz"
        ),
        var,
    )
}

/// Appends to `out` what is wrong with how `workflow` makes and checks the SBOM of the task
/// image. An error when the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for platform in PACKAGES {
        build_job(workflow, platform, out)?;
    }
    verify_job(workflow, out)
}

/// What is wrong with the job `platform`'s steps around the line that makes the SBOM an asset.
fn build_job(workflow: &str, platform: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    let mut made = Vec::new();
    for (at, step) in steps.iter().enumerate() {
        for line in lines(step) {
            if let Some(words) = args(line, VERB)
                && !words.contains(&"--verify-only")
            {
                made.push((at, step, words));
            }
        }
    }
    let [(at, step, words)] = made.as_slice() else {
        out.push(format!(
            "`{platform}` makes the task image SBOM by {} `cargo xtask pkg-image-sbom` lines, not exactly one",
            made.len()
        ));
        return Ok(());
    };
    let who = format!("the task image SBOM of `{platform}`");
    if step.conditional {
        out.push(format!("{who} is made under `if:` or `continue-on-error:`"));
    }
    let image = format!("dist/kbf-task-image-$GITHUB_SHA-{platform}.oci.tar");
    for (name, want) in [
        ("--service", "task"),
        ("--commit", "$GITHUB_SHA"),
        ("--platform", platform),
        ("--image", image.as_str()),
        ("--syft", SYFT),
        ("--source-date-epoch", "$SOURCE_DATE_EPOCH"),
    ] {
        if flag(words, name) != Some(want) {
            out.push(format!(
                "`cargo xtask pkg-image-sbom` in `{platform}` has {name} {:?}, not `{want}`",
                flag(words, name).unwrap_or("(none)")
            ));
        }
    }
    let asset = first(&steps, 0, |l| {
        args(l, IMAGE_VERB)
            .is_some_and(|a| !a.contains(&"--verify-only") && flag(&a, "--service") == Some("task"))
    });
    if asset.is_none_or(|(a, _)| a >= *at) {
        out.push(format!(
            "{who} is made before the task image is an asset (`cargo xtask pkg-image-asset --service task`), or the image is made none"
        ));
    }
    let (url, var) = archive(platform);
    let fetched = steps[..*at].iter().any(|s| {
        !s.conditional
            && lines(s).any(|l| {
                args(l, "cargo xtask fetch ").is_some_and(|a| {
                    flag(&a, "--url") == Some(url.as_str())
                        && flag(&a, "--sha256") == Some(format!("${var}").as_str())
                        && flag(&a, "--out") == Some(SYFT)
                        && flag(&a, "--extract") == Some("tar.gz")
                        && flag(&a, "--member") == Some("syft")
                })
            })
    });
    if !fetched {
        out.push(format!(
            "{who} is made without the pinned syft fetched (`cargo xtask fetch --url {url} --sha256 \"${var}\" --out {SYFT} --extract tar.gz --member syft`) in a step before, that cannot be skipped"
        ));
    }
    Ok(())
}

/// What is wrong with the verify job's check of the SBOM beside each signed task image.
fn verify_job(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let verify = workflows::job_steps(workflow, "verify-linux")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    for platform in PACKAGES {
        let image = format!("dist/kbf-task-image-$GITHUB_SHA-{platform}.oci.tar");
        let found = first(&verify, 0, |l| {
            args(l, VERB).is_some_and(|a| {
                a.contains(&"--verify-only")
                    && flag(&a, "--service") == Some("task")
                    && flag(&a, "--image") == Some(image.as_str())
                    && flag(&a, "--platform") == Some(platform)
                    && flag(&a, "--commit") == Some("$GITHUB_SHA")
                    && flag(&a, "--syft").is_none()
            })
        });
        let checked = found.is_some_and(|(at, step)| at > verified_at && !step.conditional);
        if !checked {
            out.push(format!(
                "`verify-linux` does not run `cargo xtask pkg-image-sbom --verify-only` on `{image}` in an unconditional step after the attestation check, so the SBOM that was signed is not the one checked"
            ));
        }
    }
    Ok(())
}
