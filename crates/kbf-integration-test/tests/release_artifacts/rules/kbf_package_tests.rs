//! The rules about the `kbf` package, read on the workflow of `rules` broken one way at a time.

use super::*;

/// Asserts that the lint names a Linux job that does not make its `kbf` deb an asset, one
/// that makes it twice, and an asset for a platform the set does not list.
#[test]
fn the_kbf_package_is_made_an_asset_exactly_where_the_set_says() {
    for platform in PACKAGES {
        let step = kbf_asset_step(platform);
        let none = good().replacen(&step, "", 1);
        assert_ne!(none, good(), "{platform}: in the fixture");
        names(
            &none,
            &[&format!(
                "the kbf package for {platform} is made an asset by 0 `cargo xtask pkg-kbf-asset` lines, not exactly one"
            )],
        );
        let twice = good().replacen(&step, &format!("{step}{step}"), 1);
        names(
            &twice,
            &[&format!(
                "the kbf package for {platform} is made an asset by 2 `cargo xtask pkg-kbf-asset` lines, not exactly one"
            )],
        );
    }
    let step = kbf_asset_step("linux-arm64");
    let riscv = step.replace("--platform linux-arm64", "--platform linux-riscv");
    names(
        &good().replacen(&step, &riscv, 1),
        &[
            "`cargo xtask pkg-kbf-asset --platform linux-riscv` makes a package that is not in `PACKAGES`",
            "the kbf package for linux-arm64 is made an asset by 0",
        ],
    );
}

/// Asserts that the lint names a `kbf` deb made an asset with another commit, tarball,
/// package directory or output directory than the job's own, or in a step that can be
/// skipped; the package not built, built from another configuration, or built only after;
/// the tarball of the `kbf` checked only after; and a configuration written from another
/// `kbf` than the one the job packages.
#[test]
fn the_kbf_package_is_built_and_made_an_asset_for_the_commit_and_tarball_it_names() {
    let step = kbf_asset_step("linux-x86_64");
    let change = |from: &str, to: &str, wanted: &str| {
        let w = good().replacen(&step, &step.replace(from, to), 1);
        assert_ne!(w, good(), "{from}");
        names(&w, &[wanted]);
    };
    change(
        "--commit \"$GITHUB_SHA\"",
        "--commit abc",
        "has --commit \"abc\", not `$GITHUB_SHA`",
    );
    change(
        "--kbf-tarball \"dist/kbf-$GITHUB_SHA-",
        "--kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-",
        "has --kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-linux-x86_64.tar.gz\", not `dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz`",
    );
    change(
        "--package target/pkg/out",
        "--package target/pkg/kbf-out",
        "has --package \"target/pkg/kbf-out\", not `target/pkg/out`",
    );
    change(
        "--out dist",
        "--out elsewhere",
        "has --out \"elsewhere\", not `dist`",
    );
    change(
        "\n        run:",
        "\n        if: always()\n        run:",
        "the kbf package of `linux-x86_64` is made an asset under `if:` or `continue-on-error:`",
    );
    let build = "      - name: Build kbf\n        run: cargo xtask pkg-build --nfpm n --config target/pkg/nfpm.yaml --out target/pkg/out\n";
    let missing = "is made an asset without `cargo xtask pkg-build` of the kbf configuration in a step before";
    names(&good().replacen(build, "", 1), &[missing]);
    let elsewhere = build.replace("target/pkg/nfpm.yaml", "target/pkg/other/nfpm.yaml");
    names(&good().replacen(build, &elsewhere, 1), &[missing]);
    let without = good().replacen(build, "", 1);
    let after = without.replacen(&step, &format!("{step}{build}"), 1);
    names(&after, &[missing]);
    let skippable = build.replace("\n        run:", "\n        if: always()\n        run:");
    let wanted = "the kbf package of `linux-x86_64`: `cargo xtask pkg-build` of the kbf configuration is under `if:` or `continue-on-error:`";
    names(&good().replacen(build, &skippable, 1), &[wanted]);
    let check = check_asset_step("kbf", "linux-x86_64");
    let without = good().replacen(&check, "", 1);
    let late = without.replacen(&step, &format!("{step}{check}"), 1);
    let wanted =
        "is made an asset before `cargo xtask build-commit check-asset` of the kbf tarball";
    names(&late, &[wanted]);
    let kbf = "--kbf \"target/$TARGET/release/kbf\" ";
    let other = good().replacen(kbf, "--kbf \"target/other/release/kbf\" ", 1);
    let wanted = "`linux-x86_64` writes the package configuration with --kbf Some(\"target/other/release/kbf\")";
    names(&other, &[wanted]);
    let none = good().replacen(kbf, "", 1);
    let wanted = "`linux-x86_64` writes the package configuration with --kbf None";
    names(&none, &[wanted]);
}

/// Asserts that the lint names a verify job that does not check the signed `kbf` deb of
/// each platform with its own tarball and the commit, in a step that cannot be skipped
/// and comes after the attestation check.
#[test]
fn the_verify_job_checks_the_signed_kbf_package_with_its_tarball_and_commit() {
    for platform in PACKAGES {
        let w = good().replacen(&verify_kbf_package_step(platform), "", 1);
        assert_ne!(w, good(), "{platform}");
        names(
            &w,
            &[&format!(
                "`verify-linux` does not run `cargo xtask pkg-kbf-asset --verify-only` on `dist/kbf-$GITHUB_SHA-{platform}.deb`"
            )],
        );
    }
    let wanted = "`verify-linux` does not run `cargo xtask pkg-kbf-asset --verify-only` on `dist/kbf-$GITHUB_SHA-linux-arm64.deb`";
    let step = verify_kbf_package_step("linux-arm64");
    let tarball = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-arm64.tar.gz\"";
    let other = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\"";
    for to in [
        step.replace(tarball, other),
        step.replace("--commit \"$GITHUB_SHA\"", "--commit abc"),
        step.replace("--platform linux-arm64", "--platform linux-x86_64"),
        step.replace("      - name", "      - if: always()\n        name"),
    ] {
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        names(&w, &[wanted]);
    }
    let early = good().replacen(&step, "", 1);
    let (head, tail) = early.split_once("  verify-linux:\n").expect("verify-linux");
    let tail = tail.replacen("    steps:\n", &format!("    steps:\n{step}"), 1);
    names(&format!("{head}  verify-linux:\n{tail}"), &[wanted]);
    // The check is of the branch the job verifies: `$GITHUB_REF`, not a literal, not the
    // build job's variable, and not left out.
    for expect in [
        "--expect-ref refs/heads/main",
        "--expect-ref \"$PKG_SOURCE_REF\"",
        "",
    ] {
        let to = step.replace(" --expect-ref \"$GITHUB_REF\"", &format!(" {expect}"));
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        names(&w, &[wanted]);
    }
}

/// Asserts that the lint names a Linux job that does not make its native deb an asset, one that
/// makes it twice, and a native asset for a platform the set does not list; and that the native
/// lines are no part of the count of the other deb's.
#[test]
fn the_native_package_is_made_an_asset_exactly_where_the_set_says() {
    for platform in PACKAGES {
        let step = native_asset_step(platform);
        let none = good().replacen(&step, "", 1);
        assert_ne!(none, good(), "{platform}: in the fixture");
        names(
            &none,
            &[&format!(
                "the native kbf package for {platform} is made an asset by 0 `cargo xtask pkg-kbf-asset --native` lines, not exactly one"
            )],
        );
        let twice = good().replacen(&step, &format!("{step}{step}"), 1);
        names(
            &twice,
            &[&format!(
                "the native kbf package for {platform} is made an asset by 2 `cargo xtask pkg-kbf-asset --native` lines, not exactly one"
            )],
        );
    }
    let step = native_asset_step("linux-arm64");
    let riscv = step.replace("--platform linux-arm64", "--platform linux-riscv");
    names(
        &good().replacen(&step, &riscv, 1),
        &[
            "`cargo xtask pkg-kbf-asset --native --platform linux-riscv` makes a package that is not in `PACKAGES`",
            "the native kbf package for linux-arm64 is made an asset by 0",
        ],
    );
}

/// Asserts that the lint names a native deb made an asset with another commit, tarball, package
/// directory, output directory or `kbf` than the job's own, or in a step that can be skipped; the
/// native package not configured without the host agent, configured from another directory or
/// `kbf`, not built, or built only after.
#[test]
fn the_native_package_is_configured_built_and_made_an_asset_for_the_kbf_the_job_packages() {
    let step = native_asset_step("linux-x86_64");
    let change = |from: &str, to: &str, wanted: &str| {
        let w = good().replacen(&step, &step.replace(from, to), 1);
        assert_ne!(w, good(), "{from}");
        names(&w, &[wanted]);
    };
    change(
        "--commit \"$GITHUB_SHA\"",
        "--commit abc",
        "has --commit \"abc\", not `$GITHUB_SHA`",
    );
    change(
        "--kbf-tarball \"dist/kbf-$GITHUB_SHA-",
        "--kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-",
        "has --kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-linux-x86_64.tar.gz\", not `dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz`",
    );
    change(
        "--package target/pkg-native/out",
        "--package target/pkg/out",
        "has --package \"target/pkg/out\", not `target/pkg-native/out`",
    );
    change(
        "--out dist",
        "--out elsewhere",
        "has --out \"elsewhere\", not `dist`",
    );
    change(
        "--native-version-of \"target/$TARGET/release/kbf\"",
        "--native-version-of \"target/other/release/kbf\"",
        "has --native-version-of Some(\"target/other/release/kbf\")",
    );
    change(
        "\n        run:",
        "\n        if: always()\n        run:",
        "the native kbf package of `linux-x86_64` is made an asset under `if:` or `continue-on-error:`",
    );
    change(
        " --gh \"$RUNNER_TEMP/tools/gh\"",
        "",
        "is not given the pinned gh",
    );
    let (config, build) = native_config_and_build("linux-x86_64");
    let missing_config = "is made an asset without `cargo xtask pkg-config --no-host-agent --native-version-of` of the job's kbf into `target/pkg-native` in a step before";
    names(&good().replacen(&config, "", 1), &[missing_config]);
    for (from, to) in [
        (" --no-host-agent", ""),
        (
            " --no-host-agent",
            " --host-agent packaging/host-agent/linux",
        ),
        ("--out target/pkg-native", "--out target/pkg"),
        (
            "--native-version-of \"target/$TARGET/release/kbf\" --out",
            "--version 0.1.0+ci --out",
        ),
        ("--kbf \"target/$TARGET/release/kbf\"", "--kbf k"),
    ] {
        let w = good().replacen(&config, &config.replacen(from, to, 1), 1);
        assert_ne!(w, good(), "{from}");
        names(&w, &[missing_config]);
    }
    let missing_build = "is made an asset without `cargo xtask pkg-build` of the native configuration in a step before";
    names(&good().replacen(&build, "", 1), &[missing_build]);
    let elsewhere = build.replace("target/pkg-native/nfpm.yaml", "target/pkg/nfpm.yaml");
    names(&good().replacen(&build, &elsewhere, 1), &[missing_build]);
    let without = good().replacen(&build, "", 1);
    let after = without.replacen(&step, &format!("{step}{build}"), 1);
    names(&after, &[missing_build]);
    let skippable = build.replace("\n        run:", "\n        if: always()\n        run:");
    let wanted = "the native kbf package of `linux-x86_64`: `cargo xtask pkg-build` of the native configuration is under `if:` or `continue-on-error:`";
    names(&good().replacen(&build, &skippable, 1), &[wanted]);
}

/// Asserts that the lint names a verify job that does not check the signed native deb of each
/// platform with its own tarball and the commit, with `--native`, in a step that cannot be
/// skipped and comes after the attestation check, and that the other deb's check does not stand
/// for it.
#[test]
fn the_verify_job_checks_the_signed_native_package_with_its_tarball_and_commit() {
    for platform in PACKAGES {
        let w = good().replacen(&verify_native_step(platform), "", 1);
        assert_ne!(w, good(), "{platform}");
        names(
            &w,
            &[&format!(
                "`verify-linux` does not run `cargo xtask pkg-kbf-asset --verify-only --native` on `dist/kbf-native-$GITHUB_SHA-{platform}.deb`"
            )],
        );
    }
    let wanted = "`verify-linux` does not run `cargo xtask pkg-kbf-asset --verify-only --native` on `dist/kbf-native-$GITHUB_SHA-linux-arm64.deb`";
    let step = verify_native_step("linux-arm64");
    let tarball = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-arm64.tar.gz\"";
    let other = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\"";
    for to in [
        step.replace(tarball, other),
        step.replace("--commit \"$GITHUB_SHA\"", "--commit abc"),
        step.replace("--platform linux-arm64", "--platform linux-x86_64"),
        step.replace(" --native", ""),
        step.replace("      - name", "      - if: always()\n        name"),
    ] {
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        names(&w, &[wanted]);
    }
    let early = good().replacen(&step, "", 1);
    let (head, tail) = early.split_once("  verify-linux:\n").expect("verify-linux");
    let tail = tail.replacen("    steps:\n", &format!("    steps:\n{step}"), 1);
    names(&format!("{head}  verify-linux:\n{tail}"), &[wanted]);
    for expect in [
        "--expect-ref refs/heads/main",
        "--expect-ref \"$PKG_SOURCE_REF\"",
        "",
    ] {
        let to = step.replace(" --expect-ref \"$GITHUB_REF\"", &format!(" {expect}"));
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        names(&w, &[wanted]);
    }
}
