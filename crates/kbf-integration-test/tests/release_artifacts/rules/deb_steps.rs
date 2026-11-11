//! The steps of the two Debian packages in the artifacts workflow the rules are checked on: the
//! `kbf` deb made an asset, and the native `kbf` package configured, built and made one in each
//! Linux job; and the verify job's check of each.

/// The step that configures the native package of `platform` (no host agent, the version of
/// the job's `kbf`) and the one that builds it.
pub(super) fn native_config_and_build(platform: &str) -> (String, String) {
    (
        format!(
            "      - name: Native config\n        run: cargo xtask pkg-config --kbf \"target/$TARGET/release/kbf\" --units u --trust t --sysusers s --tmpfiles f --no-host-agent --platform {platform} --native-version-of \"target/$TARGET/release/kbf\" --out target/pkg-native\n"
        ),
        "      - name: Native build\n        run: cargo xtask pkg-build --nfpm n --config target/pkg-native/nfpm.yaml --out target/pkg-native/out\n".to_owned(),
    )
}

/// The step that makes the native deb of `platform` an asset.
pub(super) fn native_asset_step(platform: &str) -> String {
    format!(
        "      - name: Native asset\n        run: cargo xtask pkg-kbf-asset --native --native-version-of \"target/$TARGET/release/kbf\" --package target/pkg-native/out --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$PKG_SOURCE_REF\" --gh \"$RUNNER_TEMP/tools/gh\" --out dist\n"
    )
}

/// The three steps that make the native deb: configure, build, asset.
pub(super) fn native_steps(platform: &str) -> String {
    let (config, build) = native_config_and_build(platform);
    format!("{config}{build}{}", native_asset_step(platform))
}

/// The verify job's check of the signed native `kbf` deb of `platform`.
pub(super) fn verify_native_step(platform: &str) -> String {
    format!(
        "      - name: Native package {platform}\n        run: cargo xtask pkg-kbf-asset --verify-only --native --package \"dist/kbf-native-$GITHUB_SHA-{platform}.deb\" --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$GITHUB_REF\"\n"
    )
}

/// The step that makes the `kbf` deb of `platform` an asset.
pub(super) fn kbf_asset_step(platform: &str) -> String {
    format!(
        "      - name: Kbf asset\n        run: cargo xtask pkg-kbf-asset --package target/pkg/out --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$PKG_SOURCE_REF\" --gh \"$RUNNER_TEMP/tools/gh\" --out dist\n"
    )
}

/// The verify job's check of the signed `kbf` deb of `platform`.
pub(super) fn verify_kbf_package_step(platform: &str) -> String {
    format!(
        "      - name: Kbf package {platform}\n        run: cargo xtask pkg-kbf-asset --verify-only --package \"dist/kbf-$GITHUB_SHA-{platform}.deb\" --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$GITHUB_REF\"\n"
    )
}
