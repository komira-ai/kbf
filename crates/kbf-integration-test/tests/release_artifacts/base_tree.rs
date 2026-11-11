//! The kbf base tree bundle in the pinned release set (docs/artifacts.md, "The kbf base tree
//! bundle"): each Linux build job reads the task image it made an asset and writes
//! `kbf-base-tree-<commit>-<platform>.tree.tar` beside it (`cargo xtask base-tree`), which the
//! sums list and the attest job signs with the rest of `SHA256SUMS`: a bundle nobody built is a
//! release the frontend cannot start from, and no pull request run attests or releases
//! anything. [`rules`] reads the workflow and names a job that:
//!
//! - makes the bundle by any number of `cargo xtask base-tree` lines but one, or makes it under
//!   `if:` or `continue-on-error:`;
//! - makes it for another image, platform, commit, `kbf-task` or directory than the ones the task
//!   image asset of the same job is made from (`--image`, `--binary`), `$GITHUB_SHA`, its own
//!   platform and `dist`;
//! - makes it before the task image is an asset (`cargo xtask pkg-image-asset --service task`).
//!
//! and names the macOS job when it makes one: the base tree is Linux only.

use super::*;

/// The command that makes the bundle.
const VERB: &str = "cargo xtask base-tree ";

/// The command that makes the task image an asset.
const IMAGE_VERB: &str = "cargo xtask pkg-image-asset ";

/// Appends to `out` what is wrong with how `workflow` makes the base tree bundle. An error when
/// the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for platform in PACKAGES {
        build_job(workflow, platform, out)?;
    }
    let darwin = workflows::job_steps(workflow, DARWIN_JOB)?;
    if first(&darwin, 0, |l| args(l, VERB).is_some()).is_some() {
        out.push(format!(
            "`{DARWIN_JOB}` makes a base tree bundle: it is made by the Linux jobs only"
        ));
    }
    Ok(())
}

/// What is wrong with the job `platform`'s steps around the line that makes the bundle.
fn build_job(workflow: &str, platform: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    let mut made = Vec::new();
    for (at, step) in steps.iter().enumerate() {
        for line in lines(step) {
            if let Some(words) = args(line, VERB) {
                made.push((at, step, words));
            }
        }
    }
    let [(at, step, words)] = made.as_slice() else {
        out.push(format!(
            "`{platform}` makes the base tree bundle by {} `cargo xtask base-tree` lines, not exactly one",
            made.len()
        ));
        return Ok(());
    };
    let who = format!("the base tree bundle of `{platform}`");
    if step.conditional {
        out.push(format!("{who} is made under `if:` or `continue-on-error:`"));
    }
    let asset = first(&steps, 0, |l| {
        args(l, IMAGE_VERB).is_some_and(|a| flag(&a, "--service") == Some("task"))
    });
    // The image and the `kbf-task` are the ones the task image is made an asset from.
    let from = |name: &str| {
        asset
            .and_then(|(_, s)| lines(s).find_map(|l| args(l, IMAGE_VERB)))
            .and_then(|a| flag(&a, name).map(str::to_owned))
    };
    for (name, want) in [
        ("--image", from("--image")),
        ("--binary", from("--binary")),
        ("--platform", Some(platform.to_owned())),
        ("--commit", Some("$GITHUB_SHA".to_owned())),
        ("--out", Some("dist".to_owned())),
    ] {
        let got = flag(words, name);
        if got != want.as_deref() {
            out.push(format!(
                "`cargo xtask base-tree` in `{platform}` has {name} {:?}, not `{}`",
                got.unwrap_or("(none)"),
                want.as_deref().unwrap_or("the task image asset's")
            ));
        }
    }
    if asset.is_none_or(|(a, _)| a >= *at) {
        out.push(format!(
            "{who} is made before the task image is an asset (`cargo xtask pkg-image-asset --service task`), or the image is made none"
        ));
    }
    Ok(())
}
