//! The container images in the pinned release set (docs/artifacts.md): the frontend (`kbf`)
//! and the update coordinator (`kbf-coordinator`), each saved with `docker save` as
//! `kbf-<image>-image-<commit>-<platform>.tar`, with an SBOM of its binary, in each Linux job.
//! There is no registry: the archive is the release asset, attested like a tarball, and the
//! operator imports it. No pull request run attests it, so a step that goes missing would turn
//! `main` red or release an image nobody loaded. [`rules`] reads the workflow and names a job
//! that:
//!
//! - does not make an image of [`IMAGES`] an asset exactly once (`cargo xtask image-asset`),
//!   or makes it for another platform than its own, or an image the set does not list;
//! - gives that line another commit, platform, image, binary (the file the job built, which
//!   its identity check named), archive, SBOM (the one of that binary) or output directory
//!   than the ones the rules name, or runs it under `if:` or `continue-on-error:`;
//! - does not build the image (`cargo xtask image-build`) from the same binary, commit and the
//!   Dockerfile directory of the image, or does not test the archive (`cargo xtask
//!   image-test`: it loads, its binary names the commit, it runs as a user that is not root and
//!   does not start with nothing mounted) for the same commit, in unconditional steps, in that
//!   order and before the asset is made;
//! - makes the coordinator image without `cargo xtask build-commit check` on the
//!   `kbf-coordinator` it built, before the image is built (the frontend's `kbf` is
//!   `identity`'s).
//!
//! The attest job's steps for the images are `attested_assets`' and `attested_bundles`'.

use super::*;

/// The images of the release set, as `--image` says them.
pub const IMAGES: &[&str] = &["frontend", "coordinator"];

/// The binary an image carries.
fn binary(image: &str) -> &'static str {
    if image == "frontend" {
        "kbf"
    } else {
        "kbf-coordinator"
    }
}

/// Where an image is saved, in the job's temporary directory (outside `target/`, which the
/// cache restores).
const WORK: &str = "$RUNNER_TEMP/image";

fn archive(image: &str) -> String {
    format!("{WORK}/kbf-{image}.tar")
}

/// The commands of `verb` whose `--image` is `image`: the step's index and words.
fn commands<'a>(
    steps: &'a [workflows::Step],
    verb: &str,
    image: &str,
) -> Vec<(usize, &'a workflows::Step, Vec<&'a str>)> {
    let prefix = format!("cargo xtask {verb} ");
    steps
        .iter()
        .enumerate()
        .filter_map(|(at, s)| {
            let words = lines(s).find_map(|l| args(l, &prefix))?;
            (flag(&words, "--image") == Some(image)).then_some((at, s, words))
        })
        .collect()
}

/// Appends to `out` what is wrong with how `workflow` makes the images assets. An error when
/// the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_images(workflow)?;
    for platform in PACKAGES {
        for image in IMAGES {
            let n = made
                .iter()
                .filter(|(i, p)| i == image && p == platform)
                .count();
            if n != 1 {
                out.push(format!(
                    "the {image} image for {platform} is made an asset by {n} `cargo xtask image-asset` lines, not exactly one"
                ));
            }
        }
    }
    for (image, platform) in &made {
        if !IMAGES.contains(&image.as_str()) || !PACKAGES.contains(&platform.as_str()) {
            out.push(format!(
                "`cargo xtask image-asset --image {image} --platform {platform}` makes an image that is not in `IMAGES` for `PACKAGES`: add it there and to docs/artifacts.md"
            ));
        }
    }
    for platform in PACKAGES {
        build_job(workflow, platform, out)?;
    }
    Ok(())
}

/// What is wrong with the job `platform`'s steps around the lines that make its images assets.
fn build_job(workflow: &str, platform: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    for image in IMAGES {
        let who = format!("the {image} image of `{platform}`");
        let assets = commands(&steps, "image-asset", image);
        let [(asset_at, asset_step, words)] = assets.as_slice() else {
            // The count is `rules`' to name.
            continue;
        };
        let bin = format!("target/$TARGET/release/{}", binary(image));
        let sbom = format!("crates/{}/{}_bin.cdx.json", crate_of(image), binary(image));
        let archive = archive(image);
        if asset_step.conditional {
            out.push(format!(
                "{who} is made an asset under `if:` or `continue-on-error:`"
            ));
        }
        for (name, want) in [
            ("--commit", "$GITHUB_SHA"),
            ("--platform", platform),
            ("--binary", bin.as_str()),
            ("--archive", archive.as_str()),
            ("--sbom", sbom.as_str()),
            ("--out", "dist"),
        ] {
            if flag(words, name) != Some(want) {
                out.push(format!(
                    "`cargo xtask image-asset` for the {image} image in `{platform}` has {name} {:?}, not `{want}`",
                    flag(words, name).unwrap_or("(none)")
                ));
            }
        }
        let dockerfiles = format!("packaging/container/kbf-{image}");
        let built = commands(&steps, "image-build", image);
        match built.as_slice() {
            [(at, step, words)] if at < asset_at => {
                if step.conditional {
                    out.push(format!(
                        "{who}: `cargo xtask image-build` is under `if:` or `continue-on-error:`"
                    ));
                }
                for (name, want) in [
                    ("--commit", "$GITHUB_SHA"),
                    ("--binary", bin.as_str()),
                    ("--dockerfile-dir", dockerfiles.as_str()),
                    ("--out", WORK),
                ] {
                    if flag(words, name) != Some(want) {
                        out.push(format!(
                            "{who}: `cargo xtask image-build` has {name} {:?}, not `{want}`",
                            flag(words, name).unwrap_or("(none)")
                        ));
                    }
                }
            }
            _ => out.push(format!(
                "{who} is made an asset without exactly one `cargo xtask image-build` in a step before"
            )),
        }
        let build_at = built.first().map_or(usize::MAX, |(at, ..)| *at);
        let tested = commands(&steps, "image-test", image);
        match tested.as_slice() {
            [(at, step, words)] if at > &build_at && at < asset_at => {
                if step.conditional {
                    out.push(format!(
                        "{who}: `cargo xtask image-test` is under `if:` or `continue-on-error:`"
                    ));
                }
                for (name, want) in [("--commit", "$GITHUB_SHA"), ("--archive", archive.as_str())] {
                    if flag(words, name) != Some(want) {
                        out.push(format!(
                            "{who}: `cargo xtask image-test` has {name} {:?}, not `{want}`",
                            flag(words, name).unwrap_or("(none)")
                        ));
                    }
                }
            }
            _ => out.push(format!(
                "{who} is made an asset without exactly one `cargo xtask image-test` of its archive in a step after the build and before"
            )),
        }
        if *image == "coordinator" {
            identity(&steps, &bin, build_at, &who, out);
        }
    }
    Ok(())
}

/// The crate whose SBOM describes the binary of `image`.
fn crate_of(image: &str) -> &'static str {
    if image == "frontend" {
        "kbf-cli"
    } else {
        "kbf-coordinator"
    }
}

/// The binary `bin` is checked to name the commit (`cargo xtask build-commit check`), in a step
/// that cannot be skipped, before the image is built at `build_at`.
fn identity(
    steps: &[workflows::Step],
    bin: &str,
    build_at: usize,
    who: &str,
    out: &mut Vec<String>,
) {
    let checked = first(steps, 0, |l| {
        args(l, "cargo xtask build-commit check ")
            .is_some_and(|a| a.len() == 2 && a[0] == bin && a[1] == "$GITHUB_SHA")
    });
    match checked {
        Some((at, step)) if at < build_at && !step.conditional => {}
        _ => out.push(format!(
            "{who} is built without `cargo xtask build-commit check \"{bin}\" \"$GITHUB_SHA\"` in an unconditional step before: the image would carry a binary that was never shown to name the commit"
        )),
    }
}
