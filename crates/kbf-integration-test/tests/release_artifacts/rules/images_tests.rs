//! The rules about the container images, read on the workflow of `rules` broken one way at a
//! time.

use super::*;

/// The whole step of `block` (the steps of `image_steps`) whose `run:` starts with `verb` and
/// names `--image image`: its `- name:` line and its `run:` line.
fn step_of(platform: &str, verb: &str, image: &str) -> String {
    let block = image_steps(platform);
    let lines: Vec<&str> = block.lines().collect();
    let at = lines
        .iter()
        .position(|l| {
            l.contains(&format!("cargo xtask {verb} ")) && l.contains(&format!("--image {image} "))
        })
        .unwrap_or_else(|| panic!("no {verb} step for {image}"));
    format!("{}\n{}\n", lines[at - 1], lines[at])
}

/// The step made one that can be skipped.
fn skippable(step: &str) -> String {
    step.replacen("        run:", "        if: always()\n        run:", 1)
}

/// `good()` with `from` replaced by `to` in the step `step`.
fn edit(step: &str, from: &str, to: &str) -> String {
    assert!(step.contains(from), "{from:?} is not in {step}");
    let w = good().replacen(step, &step.replacen(from, to, 1), 1);
    assert_ne!(w, good(), "{from}");
    w
}

/// The lint names a job that makes an image an asset not exactly once, for a platform or an
/// image the set does not list.
#[test]
fn each_image_is_made_an_asset_exactly_where_the_set_says() {
    for platform in PACKAGES {
        for image in images::IMAGES {
            let step = step_of(platform, "image-asset", image);
            let none = good().replacen(&step, "", 1);
            assert_ne!(none, good());
            names(
                &none,
                &[&format!(
                    "the {image} image for {platform} is made an asset by 0 `cargo xtask image-asset` lines, not exactly one"
                )],
            );
            let twice = good().replacen(&step, &format!("{step}{step}"), 1);
            names(
                &twice,
                &[&format!(
                    "the {image} image for {platform} is made an asset by 2 `cargo xtask image-asset` lines, not exactly one"
                )],
            );
        }
    }
    let step = step_of("linux-arm64", "image-asset", "frontend");
    names(
        &edit(&step, "--platform linux-arm64", "--platform linux-riscv"),
        &[
            "`cargo xtask image-asset --image frontend --platform linux-riscv` makes an image that is not in `IMAGES` for `PACKAGES`",
            "the frontend image for linux-arm64 is made an asset by 0",
        ],
    );
    names(
        &edit(
            &step,
            "image-asset --image frontend",
            "image-asset --image proxy",
        ),
        &[
            "`cargo xtask image-asset --image proxy --platform linux-arm64` makes an image that is not in `IMAGES`",
        ],
    );
}

/// The lint names an image made an asset with another commit, platform, binary, archive,
/// SBOM or output directory than the job's own, or in a step that can be skipped.
#[test]
fn an_image_asset_is_made_for_the_commit_binary_archive_and_sbom_of_its_job() {
    let platform = "linux-x86_64";
    for (image, bin, krate) in [
        ("frontend", "kbf", "kbf-cli"),
        ("coordinator", "kbf-coordinator", "kbf-coordinator"),
    ] {
        let step = step_of(platform, "image-asset", image);
        let change = |from: &str, to: &str, wanted: &str| names(&edit(&step, from, to), &[wanted]);
        change(
            "--commit \"$GITHUB_SHA\"",
            "--commit abc",
            "has --commit \"abc\", not `$GITHUB_SHA`",
        );
        change(
            "--platform linux-x86_64",
            "--platform linux-arm64",
            "has --platform \"linux-arm64\"",
        );
        change(
            &format!("--binary \"target/$TARGET/release/{bin}\""),
            "--binary \"target/$TARGET/release/kbf-server\"",
            &format!(
                "has --binary \"target/$TARGET/release/kbf-server\", not `target/$TARGET/release/{bin}`"
            ),
        );
        change(
            &format!("--archive \"$RUNNER_TEMP/image/kbf-{image}.tar\""),
            "--archive other.tar",
            "has --archive \"other.tar\"",
        );
        change(
            &format!("--sbom crates/{krate}/{bin}_bin.cdx.json"),
            "--sbom crates/kbf-server/kbf-server_bin.cdx.json",
            &format!(
                "has --sbom \"crates/kbf-server/kbf-server_bin.cdx.json\", not `crates/{krate}/{bin}_bin.cdx.json`"
            ),
        );
        change(
            "--out dist",
            "--out elsewhere",
            "has --out \"elsewhere\", not `dist`",
        );
        names(
            &good().replacen(&step, &skippable(&step), 1),
            &[&format!(
                "the {image} image of `{platform}` is made an asset under `if:`"
            )],
        );
    }
}

/// The lint names an image made an asset with no build, a build from another binary, commit,
/// Dockerfile directory or work directory, a build after the asset, or one that can be skipped.
#[test]
fn an_image_is_built_from_the_binary_and_commit_it_is_made_an_asset_for() {
    // (The build and test steps read the same in both jobs, so the first is the x86 job's.)
    let platform = "linux-x86_64";
    for image in images::IMAGES {
        let build = step_of(platform, "image-build", image);
        let asset = step_of(platform, "image-asset", image);
        let none = "without exactly one `cargo xtask image-build` in a step before";
        names(&good().replacen(&build, "", 1), &[none]);
        names(
            &good().replacen(&build, &format!("{build}{build}"), 1),
            &[none],
        );
        let change = |from: &str, to: &str, wanted: &str| names(&edit(&build, from, to), &[wanted]);
        change(
            "--commit \"$GITHUB_SHA\"",
            "--commit abc",
            "`cargo xtask image-build` has --commit \"abc\"",
        );
        change(
            "--binary \"target/$TARGET/release/",
            "--binary \"other/",
            "`cargo xtask image-build` has --binary \"other/",
        );
        change(
            "--dockerfile-dir packaging/container/",
            "--dockerfile-dir elsewhere/",
            "`cargo xtask image-build` has --dockerfile-dir \"elsewhere/",
        );
        change(
            "--out \"$RUNNER_TEMP/image\"",
            "--out target/image",
            "`cargo xtask image-build` has --out \"target/image\"",
        );
        names(
            &good().replacen(&build, &skippable(&build), 1),
            &[&format!(
                "{image} image of `{platform}`: `cargo xtask image-build` is under `if:`"
            )],
        );
        // Built after it is made an asset.
        let late = good()
            .replacen(&build, "", 1)
            .replacen(&asset, &format!("{asset}{build}"), 1);
        names(&late, &[none]);
    }
}

/// The lint names an image made an asset with no test of its archive, a test of another archive
/// or commit, a test before the build or after the asset, or one that can be skipped.
#[test]
fn an_image_is_tested_from_its_archive_between_its_build_and_its_asset() {
    let platform = "linux-x86_64";
    for image in images::IMAGES {
        let test = step_of(platform, "image-test", image);
        let build = step_of(platform, "image-build", image);
        let asset = step_of(platform, "image-asset", image);
        let none = format!(
            "the {image} image of `{platform}` is made an asset without exactly one `cargo xtask image-test`"
        );
        names(&good().replacen(&test, "", 1), &[&none]);
        names(
            &good().replacen(&test, &format!("{test}{test}"), 1),
            &[&none],
        );
        let change = |from: &str, to: &str, wanted: &str| names(&edit(&test, from, to), &[wanted]);
        change(
            "--commit \"$GITHUB_SHA\"",
            "--commit abc",
            "`cargo xtask image-test` has --commit \"abc\"",
        );
        change(
            "--archive \"$RUNNER_TEMP/image/",
            "--archive \"other/",
            "`cargo xtask image-test` has --archive \"other/",
        );
        names(
            &good().replacen(&test, &skippable(&test), 1),
            &[&format!(
                "{image} image of `{platform}`: `cargo xtask image-test` is under `if:`"
            )],
        );
        // Before the build, and after the asset.
        let early = good()
            .replacen(&test, "", 1)
            .replacen(&build, &format!("{test}{build}"), 1);
        names(&early, &[&none]);
        let late = good()
            .replacen(&test, "", 1)
            .replacen(&asset, &format!("{asset}{test}"), 1);
        names(&late, &[&none]);
    }
}

/// The lint names a coordinator image built from a binary whose identity was never checked,
/// checked only after the build, or checked in a step that can be skipped.
#[test]
fn the_coordinator_binary_is_shown_to_name_the_commit_before_it_goes_in_an_image() {
    let step = "      - name: Coordinator identity\n        run: cargo xtask build-commit check \"target/$TARGET/release/kbf-coordinator\" \"$GITHUB_SHA\"\n";
    let wanted = "image of `linux-arm64` is built without `cargo xtask build-commit check \"target/$TARGET/release/kbf-coordinator\" \"$GITHUB_SHA\"`";
    let removed = in_job("linux-arm64", step, "");
    names(&removed, &[wanted]);
    names(&in_job("linux-arm64", step, &skippable(step)), &[wanted]);
    // After the build.
    let asset = step_of("linux-arm64", "image-asset", "coordinator");
    let late = removed.replacen(&asset, &format!("{asset}{step}"), 1);
    assert_ne!(late, removed);
    names(&late, &[wanted]);
}
