//! The rules of the two Debian packages a Linux group makes assets of, the `kbf` deb and the
//! native `kbf` package: one provenance step and one bundle name each.

use super::*;

/// The provenance step of the `kbf` deb of `platform`.
pub(super) fn provenance_kbf_deb(platform: &str) -> String {
    format!(
        "      - id: provenance-kbf-deb-{platform}\n        uses: actions/attest-build-provenance@{ATTEST_PIN}\n        with:\n          subject-path: dist/{}.deb\n",
        attested_assets::stem("kbf", platform)
    )
}

/// The step after it, which names its bundle.
pub(super) fn naming_kbf_deb(platform: &str) -> String {
    format!(
        "      - name: Name the bundle of the kbf package for {platform}\n        env:\n          BUNDLE: ${{{{ steps.provenance-kbf-deb-{platform}.outputs.bundle-path }}}}\n        run: cp \"$BUNDLE\" \"dist/kbf-${{GITHUB_SHA}}-{platform}.deb.sigstore.json\"\n"
    )
}

/// The provenance step of the native `kbf` deb of `platform`.
pub(super) fn provenance_native_deb(platform: &str) -> String {
    format!(
        "      - id: provenance-kbf-native-deb-{platform}\n        uses: actions/attest-build-provenance@{ATTEST_PIN}\n        with:\n          subject-path: dist/{}.deb\n",
        attested_assets::stem("kbf-native", platform)
    )
}

/// The step after it, which names its bundle.
pub(super) fn naming_native_deb(platform: &str) -> String {
    format!(
        "      - name: Name the bundle of the native kbf package for {platform}\n        env:\n          BUNDLE: ${{{{ steps.provenance-kbf-native-deb-{platform}.outputs.bundle-path }}}}\n        run: cp \"$BUNDLE\" \"dist/kbf-native-${{GITHUB_SHA}}-{platform}.deb.sigstore.json\"\n"
    )
}

/// Asserts that the `kbf` deb made an asset is held to the same two rules as a tarball:
/// one provenance step of its own and the step that names its bundle `.deb.sigstore.json`, for
/// each architecture, and that a step left behind for a `kbf` deb no job makes is named.
#[test]
fn a_kbf_package_without_its_own_provenance_step_or_bundle_name_is_named() {
    let mut cases = Cases::default();
    for platform in DEBS {
        let w = good_artifacts();
        let dropped = w.replacen(&provenance_kbf_deb(platform), "", 1).replacen(
            &naming_kbf_deb(platform),
            "",
            1,
        );
        assert_ne!(dropped, w);
        let wanted = format!(
            "kbf package (deb) for {platform} is packaged, but `attest-linux` has no actions/attest-build-provenance step with subject-path dist/{}.deb",
            attested_assets::stem("kbf", platform)
        );
        cases.artifacts(&format!("kbf deb {platform}"), &dropped, &wanted, true);
    }
    let platform = DEBS[0];
    let bundle = format!("dist/kbf-${{GITHUB_SHA}}-{platform}.deb.sigstore.json");
    let wanted = "the step after the provenance step for kbf package (deb) for linux-x86_64 in `attest-linux` does not name its bundle";
    let tarballs = format!("dist/kbf-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json");
    cases.artifacts(
        "the bundle named as the tarball's",
        &broken(&bundle, &tarballs),
        wanted,
        false,
    );
    let installers = format!("dist/kbf-installer-${{GITHUB_SHA}}-{platform}.deb.sigstore.json");
    cases.artifacts(
        "the bundle named as the installer's",
        &broken(&bundle, &installers),
        wanted,
        false,
    );
    let subject = format!(
        "subject-path: dist/{}.deb",
        attested_assets::stem("kbf", platform)
    );
    cases.artifacts(
        "a glob subject",
        &broken(&subject, "subject-path: dist/*.deb"),
        "kbf package (deb) for linux-x86_64 is packaged, but",
        false,
    );
    let id = format!("      - id: provenance-kbf-deb-{platform}\n");
    cases.artifacts(
        "the step skippable",
        &broken(&id, &format!("{id}        if: always()\n")),
        "the provenance step for kbf package (deb) for linux-x86_64 in `attest-linux` is under `if:`",
        false,
    );
    cases.artifacts("no id", &broken(&id, ""), "has no `id:`", false);
    cases.artifacts(
        "the pkg-kbf-asset line removed",
        &broken(
            "cargo xtask pkg-kbf-asset --package k.deb --platform linux-arm64",
            "echo linux-arm64",
        ),
        "`attest-linux` attests dist/kbf-${{ github.sha }}-linux-arm64.deb with a provenance step of its own",
        true,
    );
    cases.done();
}

/// Asserts that the lines that make the `kbf` deb an asset are read apart from the lines that
/// only check a signed deb: a `--verify-only` line is not one, and a line without a platform is
/// an error.
#[test]
fn the_kbf_deb_lines_are_read_apart_from_the_checks() {
    let w = good_artifacts();
    assert_eq!(
        attested_assets::packaged_kbf_debs(&w).expect("readable"),
        DEBS
    );
    let no_asset_lines = w.replace("cargo xtask pkg-kbf-asset --package ", "echo ");
    assert!(
        attested_assets::packaged_kbf_debs(&no_asset_lines)
            .expect("readable")
            .is_empty()
    );
    let no_platform = w.replacen(
        "pkg-kbf-asset --package k.deb --platform linux-arm64",
        "pkg-kbf-asset --package k.deb",
        1,
    );
    let said = attested_assets::packaged_kbf_debs(&no_platform).expect_err("no platform");
    assert!(
        said.contains("a pkg-kbf-asset line without --platform"),
        "{said}"
    );
}

/// Asserts that the native `kbf` deb is held to the same two rules as the other deb: one
/// provenance step of its own with `subject-path` exactly `dist/kbf-native-<commit>-<platform>.deb`
/// and the step that names its bundle `kbf-native-<commit>-<platform>.deb.sigstore.json`, for each
/// architecture, and that a step left behind for a native deb no job makes is named.
#[test]
fn a_native_kbf_package_without_its_own_provenance_step_or_bundle_name_is_named() {
    let mut cases = Cases::default();
    for platform in DEBS {
        let w = good_artifacts();
        let dropped = w
            .replacen(&provenance_native_deb(platform), "", 1)
            .replacen(&naming_native_deb(platform), "", 1);
        assert_ne!(dropped, w);
        let wanted = format!(
            "kbf-native package (deb) for {platform} is packaged, but `attest-linux` has no actions/attest-build-provenance step with subject-path dist/{}.deb",
            attested_assets::stem("kbf-native", platform)
        );
        cases.artifacts(&format!("native deb {platform}"), &dropped, &wanted, true);
    }
    let platform = DEBS[0];
    let bundle = format!("dist/kbf-native-${{GITHUB_SHA}}-{platform}.deb.sigstore.json");
    let wanted = "the step after the provenance step for kbf-native package (deb) for linux-x86_64 in `attest-linux` does not name its bundle";
    let other = format!("dist/kbf-${{GITHUB_SHA}}-{platform}.deb.sigstore.json");
    cases.artifacts(
        "the bundle named as the kbf deb's",
        &broken(&bundle, &other),
        wanted,
        false,
    );
    let subject = format!(
        "subject-path: dist/{}.deb",
        attested_assets::stem("kbf-native", platform)
    );
    cases.artifacts(
        "a glob subject",
        &broken(&subject, "subject-path: dist/*.deb"),
        "kbf-native package (deb) for linux-x86_64 is packaged, but",
        false,
    );
    cases.artifacts(
        "the pkg-kbf-asset --native line removed",
        &broken(
            "cargo xtask pkg-kbf-asset --native --native-version-of k --package n.deb --platform linux-arm64",
            "echo linux-arm64",
        ),
        "`attest-linux` attests dist/kbf-native-${{ github.sha }}-linux-arm64.deb with a provenance step of its own",
        true,
    );
    cases.done();
}

/// Asserts that the native lines and the other deb's are read apart: the other deb's lines name
/// no native platform, the native lines name theirs and are not the other's, and a `--verify-only`
/// line is neither.
#[test]
fn the_native_deb_lines_are_read_apart_from_the_kbf_deb_lines() {
    let w = good_artifacts();
    assert_eq!(
        attested_assets::packaged_native_debs(&w).expect("readable"),
        DEBS
    );
    assert_eq!(
        attested_assets::packaged_kbf_debs(&w).expect("readable"),
        DEBS
    );
    let no_native = w.replace("pkg-kbf-asset --native ", "echo ");
    assert!(
        attested_assets::packaged_native_debs(&no_native)
            .expect("readable")
            .is_empty()
    );
    let no_platform = w.replacen(
        "--native-version-of k --package n.deb --platform linux-arm64",
        "--native-version-of k --package n.deb",
        1,
    );
    let said = attested_assets::packaged_native_debs(&no_platform).expect_err("no platform");
    assert!(
        said.contains("a pkg-kbf-asset line without --platform"),
        "{said}"
    );
}
