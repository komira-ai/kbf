use super::super::testing::deb as fake;
use super::super::testing::{COMMIT, asset, release_files, scratch};
use super::*;

fn put(dir: &Path, platform_arch: &str, version: &str, control_arch: &str) {
    let files = fake::files(control_arch.as_bytes(), b"notice");
    std::fs::write(
        asset(dir, platform_arch),
        fake::build(version, control_arch, &files),
    )
    .expect("write");
}

#[test]
fn the_version_of_a_release_is_what_both_packages_say() {
    // Catches: a package version that is not the one the control files carry, and an output
    // that replaces what GITHUB_OUTPUT holds.
    let dir = scratch("version-ok");
    release_files(&dir, "0.1.7472");
    let out = dir.join("output");
    std::fs::write(&out, "earlier=1\n").expect("seed");
    let said = version(&dir, COMMIT, &out).expect("version");
    assert_eq!(said, ["apt version: 0.1.7472"]);
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "earlier=1\npackage-version=0.1.7472\n"
    );
}

#[test]
fn two_packages_of_different_versions_are_refused_and_nothing_is_written() {
    // Catches: a release whose architectures were built from different commits.
    let dir = scratch("version-differ");
    put(&dir, "amd64", "0.1.1", "amd64");
    put(&dir, "arm64", "0.1.2", "arm64");
    let out = dir.join("output");
    let e = version(&dir, COMMIT, &out).unwrap_err().to_string();
    assert_eq!(
        e,
        format!(
            "kbf-native-{COMMIT}-linux-arm64.deb is version 0.1.2, but kbf-native-{COMMIT}-linux-x86_64.deb is version 0.1.1"
        )
    );
    assert!(!out.exists());
}

#[test]
fn a_package_of_the_wrong_name_architecture_or_version_form_is_refused() {
    // Catches: the arm64 asset holding an amd64 package, another package's file, or a version
    // with a revision or a `+` that the shared repository cannot order.
    let dir = scratch("version-wrong");
    put(&dir, "amd64", "0.1.1+g1234", "arm64");
    let files = fake::files(b"x", b"notice");
    let other = fake::build_for("other", "0.1.1", "arm64", &files);
    std::fs::write(asset(&dir, "arm64"), other).expect("write");
    let e = version(&dir, COMMIT, &dir.join("output"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("linux-x86_64.deb: Architecture is \"arm64\", not \"amd64\""),
        "{e}"
    );
    assert!(
        e.contains("linux-x86_64.deb: Version \"0.1.1+g1234\" is not <major>.<minor>.<patch>"),
        "{e}"
    );
    assert!(
        e.contains("linux-arm64.deb: Package is \"other\", not \"kbf\""),
        "{e}"
    );
}

#[test]
fn a_missing_or_unreadable_package_and_a_bad_commit_are_refused() {
    // Catches: a release with one architecture only, a file that is not a package, and a commit
    // that names no file.
    let dir = scratch("version-missing");
    let out = dir.join("output");
    let e = version(&dir, COMMIT, &out).unwrap_err().to_string();
    assert!(e.contains("linux-x86_64.deb: "), "{e}");
    assert!(e.contains("linux-arm64.deb: "), "{e}");
    std::fs::write(asset(&dir, "amd64"), b"junk").expect("junk");
    let e = version(&dir, COMMIT, &out).unwrap_err().to_string();
    assert!(
        e.contains("linux-x86_64.deb: not a Debian package: no ar magic"),
        "{e}"
    );
    let e = version(&dir, "abc", &out).unwrap_err();
    assert!(matches!(e, Error::Usage(_)), "{e}");
    assert!(!out.exists());
}

#[test]
fn an_output_that_cannot_be_appended_to_fails_after_the_check() {
    // Catches: a version lost silently when GITHUB_OUTPUT cannot be written.
    let dir = scratch("version-output");
    release_files(&dir, "0.1.1");
    let e = version(&dir, COMMIT, &dir).unwrap_err();
    assert!(matches!(e, Error::Spawn { .. }), "{e}");
}
