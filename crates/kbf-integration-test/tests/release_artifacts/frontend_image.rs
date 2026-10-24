//! The frontend image in the pinned release set (docs/artifacts.md): an OCI image tarball for each
//! Linux architecture, `kbf-frontend-image-<commit>-<platform>.oci.tar`, built from the `kbf` of
//! the job's own tarball. No pull request run attests it, so a step that goes missing would turn
//! `main` red or release an image nobody checked. [`rules`] reads the workflow and names a job
//! that:
//!
//! - does not make the image an asset exactly once (`cargo xtask pkg-image-asset`,
//!   [`PACKAGES`]), or makes it for another platform than its own;
//! - gives that line another commit, platform, `--kbf-tarball` (the tarball of this job's `kbf`),
//!   image or output directory than the ones the rules name, or runs it under `if:` or
//!   `continue-on-error:`;
//! - makes the image an asset without, in steps before it that cannot be skipped, the context
//!   taken out of that tarball (`tar -xzf`), the commit's time put in the environment
//!   (`cargo xtask source-date-epoch`), two `docker buildx build` runs and a `cmp` of their
//!   two files (the build is reproducible), or before `build-commit check-asset` of the tarball;
//!
//! and names the verify job when it does not run `pkg-image-asset --verify-only` on each signed
//! image, with its tarball and commit, in an unconditional step after the attestation check.

use super::*;

/// The command that makes the image an asset, and the one that checks the signed one.
const VERB: &str = "cargo xtask pkg-image-asset ";

/// The two files the image is built into, and the context directory.
const FIRST: &str = "target/image-out/1.oci.tar";
const SECOND: &str = "target/image-out/2.oci.tar";
const CONTEXT: &str = "target/image";

/// Appends to `out` what is wrong with how `workflow` makes the frontend image an asset, and
/// checks it after signing. An error when the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_frontend_images(workflow)?;
    for platform in PACKAGES {
        let n = made.iter().filter(|p| *p == platform).count();
        if n != 1 {
            out.push(format!(
                "the frontend image for {platform} is made an asset by {n} `cargo xtask pkg-image-asset` lines, not exactly one"
            ));
        }
    }
    for platform in &made {
        if !PACKAGES.contains(&platform.as_str()) {
            out.push(format!(
                "`cargo xtask pkg-image-asset --platform {platform}` makes an image that is not in `PACKAGES`: add it there and to docs/artifacts.md"
            ));
        }
    }
    for platform in PACKAGES {
        build_job(workflow, platform, out)?;
    }
    verify_job(workflow, out)
}

/// What is wrong with the job `platform`'s steps around the line that makes the image an asset.
fn build_job(workflow: &str, platform: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    let asset = first(&steps, 0, |l| {
        args(l, VERB).is_some_and(|a| !a.contains(&"--verify-only"))
    });
    let Some((asset_at, asset_step)) = asset else {
        return Ok(());
    };
    let words = lines(asset_step)
        .find_map(|l| args(l, VERB))
        .unwrap_or_default();
    let who = format!("the frontend image of `{platform}`");
    if asset_step.conditional {
        out.push(format!(
            "{who} is made an asset under `if:` or `continue-on-error:`"
        ));
    }
    let tarball = format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz");
    for (name, want) in [
        ("--commit", "$GITHUB_SHA"),
        ("--platform", platform),
        ("--kbf-tarball", tarball.as_str()),
        ("--image", FIRST),
        ("--out", "dist"),
    ] {
        if flag(&words, name) != Some(want) {
            out.push(format!(
                "`cargo xtask pkg-image-asset` in `{platform}` has {name} {:?}, not `{want}`",
                flag(&words, name).unwrap_or("(none)")
            ));
        }
    }
    // Each earlier step must exist, before the asset, and not be skippable.
    let earlier =
        |what: &str, found: Option<(usize, &workflows::Step)>, out: &mut Vec<String>| match found {
            Some((at, step)) if at < asset_at => {
                if step.conditional {
                    out.push(format!(
                        "{who}: {what} is under `if:` or `continue-on-error:`"
                    ));
                }
            }
            _ => out.push(format!(
                "{who} is made an asset without {what} in a step before"
            )),
        };
    let unpack = |l: &str| {
        l.starts_with("tar ")
            && l.contains(&format!("-xzf \"{tarball}\""))
            && l.contains(&format!("-C {CONTEXT} kbf"))
    };
    earlier(
        "the context taken out of this job's kbf tarball (`tar -xzf` into target/image)",
        first(&steps, 0, unpack),
        out,
    );
    let builds: Vec<(usize, &workflows::Step)> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| lines(s).any(|l| l.starts_with("docker buildx build ")))
        .collect();
    let built = |dest: &str| {
        builds
            .iter()
            .copied()
            .find(|(_, s)| lines(s).any(|l| l.contains(&format!("dest={dest},"))))
    };
    earlier(
        "the commit's time in the environment (`cargo xtask source-date-epoch`)",
        first(&steps, 0, |l| {
            args(l, "cargo xtask source-date-epoch ").is_some_and(|a| {
                flag(&a, "--commit") == Some("$GITHUB_SHA")
                    && flag(&a, "--output") == Some("$GITHUB_ENV")
            })
        }),
        out,
    );
    earlier("a `docker buildx build` into 1.oci.tar", built(FIRST), out);
    earlier("a `docker buildx build` into 2.oci.tar", built(SECOND), out);
    earlier(
        "`cmp` of the two builds",
        first(&steps, 0, |l| l == format!("cmp {FIRST} {SECOND}")),
        out,
    );
    for (_, step) in &builds {
        for l in lines(step).filter(|l| l.starts_with("docker buildx build ")) {
            for must in [
                "--provenance=false",
                "--sbom=false",
                "--build-arg SOURCE_DATE_EPOCH",
            ] {
                if !l.contains(must) {
                    out.push(format!(
                        "{who}: a `docker buildx build` line lacks `{must}`, so the image is not reproducible"
                    ));
                }
            }
        }
    }
    // (A kbf tarball never checked at all is `identity`'s to name; here, one checked too late.)
    let checked = first(&steps, 0, |l| {
        args(l, "cargo xtask build-commit check-asset ")
            .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "$GITHUB_SHA")
    });
    if checked.is_some_and(|(at, _)| at > asset_at) {
        out.push(format!(
            "{who} is made an asset before `cargo xtask build-commit check-asset` of the kbf tarball"
        ));
    }
    Ok(())
}

/// What is wrong with the verify job's check of each signed frontend image.
fn verify_job(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let verify = workflows::job_steps(workflow, "verify-linux")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    for platform in PACKAGES {
        let image = format!("dist/kbf-frontend-image-$GITHUB_SHA-{platform}.oci.tar");
        let tarball = format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz");
        let found = first(&verify, 0, |l| {
            args(l, VERB).is_some_and(|a| {
                a.contains(&"--verify-only")
                    && flag(&a, "--image") == Some(image.as_str())
                    && flag(&a, "--kbf-tarball") == Some(tarball.as_str())
                    && flag(&a, "--platform") == Some(platform)
                    && flag(&a, "--commit") == Some("$GITHUB_SHA")
            })
        });
        let checked = found.is_some_and(|(at, step)| at > verified_at && !step.conditional);
        if !checked {
            out.push(format!(
                "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `{image}` in an unconditional step after the attestation check, so the image that was signed is not the one checked"
            ));
        }
    }
    Ok(())
}
