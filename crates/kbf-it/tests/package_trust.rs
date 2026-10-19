//! `cargo xtask`'s output as the `kbf-installer` it is made for reads it.
//!
//! xtask and the installer are written apart: `pkg-trust` writes the `installer.conf` of
//! the package's trust directory and `package` writes the tarball a host stages, and the
//! installer parses the first (`Settings`) and unpacks the second (`archive::unpack_one`)
//! on a host. Nothing joins them but this test. Catches:
//!
//! - the generator and the parser drifting: a key added to the conf the installer refuses
//!   (the whole package would then be one no host can load), `SOURCE_REFS` renamed on one
//!   side, or the default branch of the installer (`DEFAULT_REF`) no longer the one a
//!   package built on `main` trusts;
//! - a package built on a feature branch loading as trusting `main` (or the reverse), the
//!   case `pkg-asset` and the verify job exist to refuse: the parsed `source_refs` are
//!   exactly the branch asked for, and nothing else;
//! - `xtask`'s and the installer's rule for a ref disagreeing: xtask refusing a ref the
//!   installer takes (a branch that could never be released) or accepting one the installer
//!   would refuse (a package no host loads);
//! - the tarball `cargo xtask package` writes not being one `unpack_one` takes (one entry
//!   named for the binary, mode, no trailing data), which nothing but a host would find.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;
use kbf_installer::attest::SourceRef;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::settings::{DEFAULT_REF, Settings};

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("package-trust")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn host() -> Layout {
    Layout::new(Root::new("/").expect("absolute"), Platform::Linux)
}

/// A host whose trust directory holds `conf`, root's alone as a bootstrap leaves it.
fn host_with(conf: &[u8]) -> FakeExec {
    let root = Owner { uid: 0, gid: 0 };
    let mut exec = FakeExec::new(0);
    for dir in [
        "/usr",
        "/usr/share",
        "/usr/share/kbf-installer",
        "/usr/share/kbf-installer/trust",
    ] {
        exec.entries.insert(
            HostPath::new(dir).expect("path"),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: root,
            },
        );
    }
    exec.entries.insert(
        host().settings(),
        Entry::File {
            perms: Perms::new(0o644),
            owner: root,
            bytes: conf.to_vec(),
        },
    );
    exec
}

/// The `installer.conf` that `cargo xtask pkg-trust --source-ref <source_ref>` writes.
fn written(name: &str, source_ref: &str) -> Vec<u8> {
    let dir = scratch(name);
    std::fs::write(dir.join("gh-in"), b"gh").expect("gh");
    std::fs::write(dir.join("root-in"), "{}\n").expect("root");
    xtask::pkg_trust::write(
        &dir.join("gh-in"),
        &dir.join("root-in"),
        source_ref,
        &dir.join("trust"),
    )
    .expect("pkg-trust");
    std::fs::read(dir.join("trust/installer.conf")).expect("conf")
}

#[test]
fn the_installer_reads_the_branch_pkg_trust_wrote_and_no_other() {
    // Catches: the generator and the parser drifting, and a package that loads as
    // trusting a branch other than the one it was built on.
    let refs = [
        DEFAULT_REF,
        "refs/heads/feature/arch-v2",
        "refs/heads/release/1.2_x-y",
    ];
    for source_ref in refs {
        let conf = written("branch", source_ref);
        let settings = Settings::load(&host_with(&conf), &host()).expect("the installer loads it");
        let got: Vec<&str> = settings.source_refs.iter().map(SourceRef::as_str).collect();
        assert_eq!(got, [source_ref]);
    }
    // And the default of a host with no file is the ref a package built on main carries.
    let defaults = Settings::defaults(&host());
    let main = written("main", "refs/heads/main");
    let loaded = Settings::load(&host_with(&main), &host()).expect("loads");
    assert_eq!(loaded.source_refs, defaults.source_refs);
    assert_eq!(loaded.run_as, defaults.run_as, "no key beyond SOURCE_REFS");
}

#[test]
fn xtask_and_the_installer_agree_on_which_refs_are_branches() {
    // Catches: pkg-trust refusing a branch the installer takes, or writing a conf for a
    // ref the installer refuses (which `pkg-asset` would then ship).
    for r in [
        "refs/heads/main",
        "refs/heads/feature/arch-v2",
        "refs/heads/a.b_c-d/e",
        "refs/heads/-x",
        "main",
        "refs/tags/v1",
        "refs/heads/",
        "refs/heads/a b",
        "refs/heads/a..b",
        "refs/heads//a",
        "refs/heads/a/",
        "refs/heads//",
        "refs/heads/a\nRUN_AS_UID=0",
        "refs/heads/caf\u{e9}",
        "refs/pull/12/merge",
        "",
    ] {
        assert_eq!(
            xtask::pkg_trust::ref_ok(r),
            SourceRef::new(r).is_ok(),
            "{r:?}: xtask and the installer disagree"
        );
    }
}

#[test]
fn the_tarball_xtask_packages_is_one_the_installer_unpacks() {
    // Catches: `cargo xtask package` writing a tarball the installer's `unpack_one`
    // refuses (another entry name, a directory entry, a second entry, trailing bytes):
    // a release no host could stage.
    let dir = scratch("tarball");
    let bytes = b"\x7fELF the binary";
    let tgz = xtask::package::tarball("kbf-installer", bytes).expect("tarball");
    let tarball = dir.join("kbf-installer.tar.gz");
    std::fs::write(&tarball, &tgz).expect("write");
    let out = dir.join("kbf-installer");
    kbf_installer::archive::unpack_one(
        &tarball,
        "kbf-installer",
        &out,
        1 << 20,
        Duration::from_secs(30),
    )
    .expect("the installer unpacks xtask's tarball");
    assert_eq!(std::fs::read(&out).expect("unpacked"), bytes);
    // And not under another name: the check is of the name, not of the shape alone.
    let other = dir.join("other");
    assert!(
        kbf_installer::archive::unpack_one(
            &tarball,
            "kbf",
            &other,
            1 << 20,
            Duration::from_secs(30)
        )
        .is_err()
    );
}
