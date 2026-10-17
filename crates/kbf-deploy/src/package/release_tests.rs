//! The package's files and the release install.

use std::fs;

use kbf_host::step::Perms;

use super::super::testkit::*;
use super::super::*;
use super::*;

/// Catches trust files read in directory order (a plan must not depend on it), a mode
/// taken from the package (the copy is 0755 for a program and 0644 otherwise, whatever
/// dpkg set), and a directory or link in the trust directory copied or followed.
#[test]
fn the_trust_files_are_sorted_with_modes_from_their_exec_bit() {
    let f = fixture("pkg-release-trust");
    let files = read_trust(&f.env.root).expect("reads");
    let names: Vec<&str> = files.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["gh", "installer.conf", "trusted_root.jsonl"]);
    let perms: Vec<Perms> = files.iter().map(|t| t.perms).collect();
    assert_eq!(
        perms,
        [Perms::new(0o755), Perms::new(0o644), Perms::new(0o644)]
    );
    fs::create_dir(f.exec.at("/usr/lib/kbf/libexec/sub")).expect("dir");
    let e = read_trust(&f.env.root).expect_err("a directory");
    assert!(matches!(e, PackageError::Shipped(_)), "{e}");
}

/// Catches a trust directory that is missing being read as "nothing to copy".
#[test]
fn a_missing_trust_directory_or_release_is_an_error() {
    let f = fixture("pkg-release-missing");
    fs::remove_dir_all(f.exec.at("/usr/lib/kbf/libexec")).expect("rm");
    let e = read_trust(&f.env.root).expect_err("missing");
    assert!(e.to_string().contains("cannot read"), "{e}");
    fs::remove_dir_all(f.exec.at("/usr/lib/kbf/release/units")).expect("rm");
    fs::remove_file(f.exec.at("/usr/lib/kbf/release/kbf")).expect("rm");
    let e = read_shipped(&f.env.root).expect_err("no binary");
    assert!(matches!(e, PackageError::Release(_)), "{e}");
}

/// Catches a manifest that is not JSON being taken, and extra keys being refused (the
/// packaging owns the rest of the file).
#[test]
fn the_manifest_names_the_commit_and_nothing_else_is_read() {
    let f = fixture("pkg-release-manifest");
    let shipped = read_shipped(&f.env.root).expect("reads");
    assert_eq!(shipped.commit.as_str(), COMMIT);
    assert_eq!(shipped.source.binary, b"kbf-1");
    assert_eq!(shipped.source.units.len(), 1);
    write(
        &f.exec,
        "/usr/lib/kbf/release/manifest.json",
        "not json",
        0o644,
    );
    let e = read_shipped(&f.env.root).expect_err("not json");
    assert!(matches!(e, PackageError::Manifest { .. }), "{e}");
}

/// Catches a copy that is not checked: a binary on the host that differs from the shipped
/// one (a short write, a full disk) or is gone must fail the verb, exit 1, naming both
/// sums.
#[test]
fn a_copy_that_differs_or_is_missing_is_a_failure() {
    let f = fixture("pkg-release-verify");
    let shipped = read_shipped(&f.env.root).expect("reads");
    let e = verify_copy(&f.exec, &shipped).expect_err("not copied");
    assert!(e.to_string().contains("sha256 none (no file)"), "{e}");
    assert_eq!(e.exit(), 1);
    write(
        &f.exec,
        &format!("/usr/local/kbf/releases/{COMMIT}/kbf"),
        "kbf-2",
        0o755,
    );
    let e = verify_copy(&f.exec, &shipped).expect_err("differs");
    assert!(
        e.to_string()
            .contains(&hex::encode(Sha256::digest(b"kbf-2"))),
        "{e}"
    );
    write(
        &f.exec,
        &format!("/usr/local/kbf/releases/{COMMIT}/kbf"),
        "kbf-1",
        0o755,
    );
    verify_copy(&f.exec, &shipped).expect("same");
}

/// Catches a release without units getting a `units` directory, and `current` made over a
/// path that is not a link.
#[test]
fn a_release_without_units_has_no_units_directory() {
    let mut f = fixture("pkg-release-no-units");
    fs::remove_dir_all(f.exec.at("/usr/lib/kbf/release/units")).expect("rm");
    f.postinst(Mode::Apply).1.expect("installs");
    assert!(!exists(
        &f.exec,
        &format!("/usr/local/kbf/releases/{COMMIT}/units")
    ));
    assert!(exists(
        &f.exec,
        &format!("/usr/local/kbf/releases/{COMMIT}/kbf")
    ));
}

/// Catches a `current` that is a directory (a hand-made layout) being replaced or
/// followed: it is the updater's or the operator's, and is left.
#[test]
fn a_current_that_is_not_a_link_is_left_alone() {
    let mut f = fixture("pkg-release-current-dir");
    fs::create_dir_all(f.exec.at("/usr/local/kbf/current")).expect("dir");
    let (out, r) = f.postinst(Mode::Apply);
    r.expect("installs");
    assert!(
        out.contains("current: exists, left to the updater"),
        "{out}"
    );
    assert!(f.exec.at("/usr/local/kbf/current").is_dir());
    assert!(f.steps().iter().all(|s| !s.starts_with("link ")));
}

/// Catches a trust file name that is not UTF-8 being dropped from the copy.
#[test]
fn a_trust_file_name_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt;
    let f = fixture("pkg-release-utf8");
    let odd = f
        .exec
        .at("/usr/lib/kbf/libexec")
        .join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    fs::write(odd, "x").expect("write");
    let e = read_trust(&f.env.root).expect_err("refused");
    assert!(matches!(e, PackageError::NotUtf8(_)), "{e}");
}
