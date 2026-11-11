//! The rules about the kbf base tree bundle, read on the workflow of `rules` broken one way at a
//! time. Each Linux build job makes the bundle from the task image it made an asset and the
//! `kbf-task` it built, once, after that asset, and the macOS job makes none. No pull request run
//! attests or releases anything, so a step that goes missing turns `main` red or releases a
//! version the frontend cannot publish a tree for.

use super::*;

/// The step that makes the base tree bundle of `platform`, after the task image is an asset.
pub(super) fn base_tree_step(platform: &str) -> String {
    format!(
        "      - name: Base tree\n        run: cargo xtask base-tree --image target/task-image-out/1.oci.tar --platform {platform} --commit \"$GITHUB_SHA\" --binary \"target/$STATIC_TARGET/release/kbf-task\" --out dist\n"
    )
}

/// `good()` with `from` replaced by `to` in the base tree step of `platform`.
fn edited(platform: &str, from: &str, to: &str) -> String {
    let step = base_tree_step(platform);
    let changed = step.replace(from, to);
    assert_ne!(changed, step, "{from}");
    let w = good().replacen(&step, &changed, 1);
    assert_ne!(w, good());
    w
}

/// Asserts that a Linux job that does not make the base tree bundle, or makes it twice, is named.
/// Catches: a release with no bundle for a platform, which the frontend then cannot publish a
/// tree from, and a bundle made twice (the second is refused as already listed).
#[test]
fn the_base_tree_bundle_is_made_exactly_once_per_linux_job() {
    for platform in PACKAGES {
        let step = base_tree_step(platform);
        let none = good().replacen(&step, "", 1);
        assert_ne!(none, good(), "{platform}");
        let wanted = format!(
            "`{platform}` makes the base tree bundle by 0 `cargo xtask base-tree` lines, not exactly one"
        );
        assert!(found(&none).contains(&wanted), "{:?}", found(&none));
        let twice = good().replacen(&step, &format!("{step}{step}"), 1);
        let wanted = wanted.replace("by 0", "by 2");
        assert!(found(&twice).contains(&wanted), "{:?}", found(&twice));
    }
}

/// Asserts that the lint names a bundle made from another image, platform, commit, `kbf-task` or
/// directory than the job's own, or in a step that can be skipped.
/// Catches: a bundle of the wrong platform's base, of a binary that is not the one the task image
/// holds, or one that goes missing quietly when a step is skipped.
#[test]
fn the_base_tree_bundle_is_made_of_this_jobs_image_and_kbf_task() {
    let platform = "linux-x86_64";
    for (from, to, wanted) in [
        (
            "--image target/task-image-out/1.oci.tar",
            "--image target/task-image-out/2.oci.tar",
            "has --image \"target/task-image-out/2.oci.tar\", not `target/task-image-out/1.oci.tar`",
        ),
        (
            "--platform linux-x86_64",
            "--platform linux-arm64",
            "has --platform \"linux-arm64\", not `linux-x86_64`",
        ),
        (
            "--commit \"$GITHUB_SHA\"",
            "--commit abc",
            "has --commit \"abc\", not `$GITHUB_SHA`",
        ),
        (
            "--binary \"target/$STATIC_TARGET/release/kbf-task\"",
            "--binary target/release/kbf",
            "has --binary \"target/release/kbf\", not `target/$STATIC_TARGET/release/kbf-task`",
        ),
        (" --out dist", "", "has --out \"(none)\", not `dist`"),
    ] {
        let got = found(&edited(platform, from, to));
        assert!(
            got.iter().any(
                |l| l.starts_with(&format!("`cargo xtask base-tree` in `{platform}` "))
                    && l.contains(wanted)
            ),
            "{from}: {got:#?}"
        );
    }
    for condition in ["if: always()", "continue-on-error: true"] {
        let step = base_tree_step(platform);
        let skippable = step.replace(
            "        run:",
            &format!("        {condition}\n        run:"),
        );
        let w = good().replacen(&step, &skippable, 1);
        assert_ne!(w, good());
        names(
            &w,
            &["the base tree bundle of `linux-x86_64` is made under `if:` or `continue-on-error:`"],
        );
    }
}

/// Asserts that the lint names a bundle made before the task image is an asset, or in a job that
/// makes no task image an asset at all (it would read nothing the release ships).
/// Catches: a bundle of an image that is not the one the release lists, read before it was
/// checked.
#[test]
fn the_base_tree_bundle_is_made_after_the_task_image_is_an_asset() {
    for platform in PACKAGES {
        let step = base_tree_step(platform);
        let tasks = task_steps(platform);
        let before = good()
            .replacen(&step, "", 1)
            .replacen(&tasks, &format!("{step}{tasks}"), 1);
        assert_ne!(before, good(), "{platform}");
        names(
            &before,
            &[&format!(
                "the base tree bundle of `{platform}` is made before the task image is an asset"
            )],
        );
    }
    let platform = "linux-arm64";
    let asset = task_steps(platform);
    let (_, asset_step) = asset
        .split_once("      - name: Task asset\n")
        .expect("asset step");
    let without = good().replacen(&format!("      - name: Task asset\n{asset_step}"), "", 1);
    assert_ne!(without, good());
    names(
        &without,
        &[&format!(
            "the base tree bundle of `{platform}` is made before the task image is an asset"
        )],
    );
}

/// Asserts that the lint names the macOS job when it makes a base tree bundle.
/// Catches: a base tree for a platform with no base image, which `cargo xtask base-tree` refuses
/// only when the job runs.
#[test]
fn the_macos_job_makes_no_base_tree_bundle() {
    let step = base_tree_step("darwin-arm64");
    let anchor = "      - name: Package the bundle\n";
    let w = in_job("darwin-arm64", anchor, &format!("{step}{anchor}"));
    assert_ne!(w, good());
    names(
        &w,
        &["`darwin-arm64` makes a base tree bundle: it is made by the Linux jobs only"],
    );
}
