//! The rules about the CycloneDX SBOM of the task image, read on the workflow of `rules` broken
//! one way at a time. Each Linux build job fetches a pinned syft, and after it makes the task
//! image an asset it makes the image's SBOM an asset (`cargo xtask pkg-image-sbom`); the verify
//! job checks the SBOM beside each signed image. No pull request run attests anything, so a
//! step that goes missing turns `main` red or releases an image whose packages nobody listed.

use super::*;

/// The Linux job names, which the rules name in their messages.
const LINUX: [&str; 2] = ["linux-x86_64", "linux-arm64"];

/// `good()` with `from` replaced by `to` in the SBOM step of `platform`.
fn sbom_edited(platform: &str, from: &str, to: &str) -> String {
    let step = task_sbom_step(platform);
    let edited = step.replace(from, to);
    assert_ne!(edited, step, "{from}");
    let w = good().replacen(&step, &edited, 1);
    assert_ne!(w, good());
    w
}

/// Asserts that a Linux job that does not make the SBOM of its task image an asset, or makes it
/// twice, is named, and so is an image SBOM for a platform the set does not list.
#[test]
fn the_task_image_sbom_is_made_an_asset_exactly_once_per_linux_job() {
    for platform in LINUX {
        let step = task_sbom_step(platform);
        let none = good().replacen(&step, "", 1);
        assert_ne!(none, good(), "{platform}");
        let wanted = format!(
            "`{platform}` makes the task image SBOM by 0 `cargo xtask pkg-image-sbom` lines, not exactly one"
        );
        assert!(found(&none).contains(&wanted), "{:?}", found(&none));
        let twice = good().replacen(&step, &format!("{step}{step}"), 1);
        let wanted = wanted.replace("by 0", "by 2");
        assert!(found(&twice).contains(&wanted), "{:?}", found(&twice));
    }
}

/// Asserts that the lint names an SBOM made for another commit, platform, image, service, time or
/// syft than the job's own, or in a step that can be skipped.
#[test]
fn the_task_image_sbom_is_made_for_this_job_this_commit_and_the_fetched_syft() {
    let platform = "linux-x86_64";
    for (from, to, wanted) in [
        (
            "--commit \"$GITHUB_SHA\"",
            "--commit abc",
            "has --commit \"abc\", not `$GITHUB_SHA`",
        ),
        (
            "--platform linux-x86_64",
            "--platform linux-arm64",
            "has --platform \"linux-arm64\", not `linux-x86_64`",
        ),
        (
            "--image \"dist/kbf-task-image-",
            "--image \"dist/kbf-frontend-image-",
            "has --image \"dist/kbf-frontend-image-$GITHUB_SHA-linux-x86_64.oci.tar\", not `dist/kbf-task-image-$GITHUB_SHA-linux-x86_64.oci.tar`",
        ),
        (
            "--service task",
            "--service frontend",
            "has --service \"frontend\", not `task`",
        ),
        (
            "--syft \"$RUNNER_TEMP/tools/syft\"",
            "--syft syft",
            "has --syft \"syft\", not `$RUNNER_TEMP/tools/syft`",
        ),
        (
            "--source-date-epoch \"$SOURCE_DATE_EPOCH\"",
            "--source-date-epoch 0",
            "has --source-date-epoch \"0\", not `$SOURCE_DATE_EPOCH`",
        ),
    ] {
        let got = found(&sbom_edited(platform, from, to));
        assert!(
            got.iter().any(|l| l
                .starts_with(&format!("`cargo xtask pkg-image-sbom` in `{platform}` "))
                && l.contains(wanted)),
            "{from}: {got:#?}"
        );
    }
    for condition in ["if: always()", "continue-on-error: true"] {
        let step = task_sbom_step(platform);
        let skippable = step.replace(
            "        run:",
            &format!("        {condition}\n        run:"),
        );
        let w = good().replacen(&step, &skippable, 1);
        assert_ne!(w, good());
        names(
            &w,
            &["the task image SBOM of `linux-x86_64` is made under `if:` or `continue-on-error:`"],
        );
    }
}

/// Asserts that the lint names an SBOM made before the image it describes is an asset, or without
/// the pinned syft fetched (for the job's architecture, by the variable of that archive, to the
/// path `--syft` names) before it.
#[test]
fn the_task_image_sbom_is_made_after_the_image_and_with_a_pinned_syft() {
    for platform in LINUX {
        let tail = format!("{}{}", syft_fetch_step(platform), task_sbom_step(platform));
        let swapped = format!("{}{}", task_sbom_step(platform), syft_fetch_step(platform));
        let w = good().replacen(&tail, &swapped, 1);
        assert_ne!(w, good(), "{platform}");
        names(
            &w,
            &[&format!(
                "the task image SBOM of `{platform}` is made without the pinned syft fetched"
            )],
        );
        let before = good().replacen(
            &format!("{}{}", task_steps(platform), tail),
            &format!("{tail}{}", task_steps(platform)),
            1,
        );
        assert_ne!(before, good(), "{platform}");
        names(
            &before,
            &[&format!(
                "the task image SBOM of `{platform}` is made before the task image is an asset"
            )],
        );
    }
    let fetch = syft_fetch_step("linux-x86_64");
    for (what, bad) in [
        (
            "another architecture's archive",
            fetch.replace("linux_amd64", "linux_arm64"),
        ),
        (
            "another architecture's digest",
            fetch.replace("SYFT_SHA256_X86_64_LINUX", "SYFT_SHA256_AARCH64_LINUX"),
        ),
        (
            "another directory",
            fetch.replace("tools/syft", "tools/other"),
        ),
        (
            "no extraction",
            fetch.replace(" --extract tar.gz --member syft", ""),
        ),
        (
            "another project",
            fetch.replace("anchore/syft", "someone/syft"),
        ),
        ("no fetch", String::new()),
    ] {
        let w = good().replacen(&fetch, &bad, 1);
        assert_ne!(w, good(), "{what}");
        let got = found(&w);
        assert!(
            got.iter().any(|l| l.contains(
                "the task image SBOM of `linux-x86_64` is made without the pinned syft fetched"
            )),
            "{what}: {got:#?}"
        );
    }
}

/// Asserts that the pin of the fetched syft is held to the rule of every fetched tool: a literal
/// digest, or a variable nobody defines, is named.
#[test]
fn the_syft_digest_is_a_workflow_variable_of_64_hex_digits() {
    let fetch = syft_fetch_step("linux-x86_64");
    for bad in [
        fetch.replace("\"$SYFT_SHA256_X86_64_LINUX\"", &"ab".repeat(32)),
        fetch.replace("$SYFT_SHA256_X86_64_LINUX", "$SYFT_SHA256_UNDEFINED"),
    ] {
        let w = good().replacen(&fetch, &bad, 1);
        assert_ne!(w, good());
        names(&w, &["pins its download with --sha256"]);
    }
}

/// Asserts that the lint names a verify job that does not check the SBOM beside the signed task
/// image of each platform, after the attestation check and in a step that cannot be skipped.
#[test]
fn the_verify_job_checks_the_sbom_beside_each_signed_task_image() {
    for platform in LINUX {
        let step = verify_task_sbom_step(platform);
        let wanted = format!(
            "`verify-linux` does not run `cargo xtask pkg-image-sbom --verify-only` on `dist/kbf-task-image-$GITHUB_SHA-{platform}.oci.tar` in an unconditional step after the attestation check"
        );
        let none = good().replacen(&step, "", 1);
        assert_ne!(none, good(), "{platform}");
        let names = |w: &str| found(w).iter().any(|l| l.contains(&wanted));
        assert!(names(&none), "{:?}", found(&none));
        let skippable = step.replace("        run:", "        if: always()\n        run:");
        let w = good().replacen(&step, &skippable, 1);
        assert!(names(&w), "{:?}", found(&w));
        let other = step.replace("--platform", "--platform x-");
        let w = good().replacen(&step, &other, 1);
        assert!(names(&w), "{:?}", found(&w));
    }
    // One that runs before the attestation check is not after it.
    let step = verify_task_sbom_step("linux-arm64");
    let attest = "      - run: cargo xtask verify-assets attestations dist";
    let moved = good()
        .replacen(&step, "", 1)
        .replacen(attest, &format!("{step}{attest}"), 1);
    assert_ne!(moved, good());
    assert!(
        found(&moved)
            .iter()
            .any(|l| l.contains("`dist/kbf-task-image-$GITHUB_SHA-linux-arm64.oci.tar` in an unconditional step after the attestation check")),
        "{:?}",
        found(&moved)
    );
}
