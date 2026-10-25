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
//!   case `pkg-kbf-asset` and the verify job exist to refuse: the parsed `source_refs` are
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
        "/usr/share/kbf",
        "/usr/share/kbf/trust",
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
        "refs/heads/team/topic-2",
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

/// A host (a fake) whose directories along `files` are root's, and whose `files` hold the bytes
/// given, as a package leaves them.
fn host_files(files: &[(&str, &[u8])]) -> FakeExec {
    let root = Owner { uid: 0, gid: 0 };
    let mut exec = FakeExec::new(0);
    for (path, bytes) in files {
        let mut dir = String::new();
        for part in path.split('/').skip(1).take(path.split('/').count() - 2) {
            dir = format!("{dir}/{part}");
            exec.entries.insert(
                HostPath::new(&dir).expect("path"),
                Entry::Dir {
                    perms: Perms::new(0o755),
                    owner: root,
                },
            );
        }
        exec.entries.insert(
            HostPath::new(path).expect("path"),
            Entry::File {
                perms: Perms::new(0o644),
                owner: root,
                bytes: bytes.to_vec(),
            },
        );
    }
    exec
}

#[test]
fn the_file_the_package_installs_is_the_file_the_installer_reads() {
    // Catches: the package and the reader of its trust directory naming two places (the
    // package moved without the layout, or the layout without the package), or two names: the
    // directory and each file of it, for the layout a host is given without the fallback.
    let layout = host();
    assert_eq!(layout.trust_dir().as_str(), xtask::pkg_linux::TRUST_DIR);
    let read = [layout.gh(), layout.trusted_root(), layout.settings()];
    let mut read: Vec<&str> = read.iter().map(|p| p.as_str()).collect();
    read.sort_unstable();
    let mut packaged: Vec<String> = xtask::pkg_linux::TRUST_FILES
        .iter()
        .map(|(name, _)| format!("{}/{name}", xtask::pkg_linux::TRUST_DIR))
        .collect();
    packaged.sort();
    assert_eq!(read, packaged);
}

#[test]
fn the_old_trust_directory_is_read_only_when_the_new_one_is_absent() {
    // Catches: a host that still has the files where the `kbf-installer` package put them
    // refused (it would stop verifying the day it is given this build), the old directory read
    // when the `kbf` package's is there too (the conf of a package no longer installed would
    // decide which branch is trusted), and a fallback that decides a different directory for
    // `gh` than for `installer.conf`.
    let (old, new) = (
        "/usr/share/kbf-installer/trust/installer.conf",
        "/usr/share/kbf/trust/installer.conf",
    );
    let (old_conf, new_conf) = (
        b"SOURCE_REFS=refs/heads/from-the-old-package\n".as_slice(),
        b"SOURCE_REFS=refs/heads/from-the-new-package\n".as_slice(),
    );
    let refs = |layout: &Layout, exec: &FakeExec| {
        let settings = Settings::load(exec, layout).expect("loads");
        settings
            .source_refs
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let layout_over = |name: &str, dirs: &[&str]| {
        let root = scratch(name);
        for dir in dirs {
            std::fs::create_dir_all(root.join(dir)).expect("trust directory");
        }
        Layout::new(Root::new(root).expect("absolute"), Platform::Linux).with_trust_fallback()
    };
    // Only the old directory: its conf decides, and so does its gh.
    let layout = layout_over("fallback-old-only", &["usr/share/kbf-installer/trust"]);
    let exec = host_files(&[(old, old_conf)]);
    assert_eq!(refs(&layout, &exec), ["refs/heads/from-the-old-package"]);
    assert_eq!(layout.gh().as_str(), "/usr/share/kbf-installer/trust/gh");
    // Both: the new one decides, though the old conf is there to be read.
    let layout = layout_over(
        "fallback-both",
        &["usr/share/kbf-installer/trust", "usr/share/kbf/trust"],
    );
    let exec = host_files(&[(old, old_conf), (new, new_conf)]);
    assert_eq!(refs(&layout, &exec), ["refs/heads/from-the-new-package"]);
    assert_eq!(layout.gh().as_str(), "/usr/share/kbf/trust/gh");
    // Neither: the defaults, from the new path (no conf to read at either).
    let layout = layout_over("fallback-neither", &[]);
    let exec = host_files(&[]);
    assert_eq!(refs(&layout, &exec), [DEFAULT_REF]);
    assert_eq!(layout.settings().as_str(), new);
    // And a layout that was not asked for the fallback does not read the old conf even where
    // it is the only one.
    let root = scratch("fallback-not-asked");
    std::fs::create_dir_all(root.join("usr/share/kbf-installer/trust")).expect("old directory");
    let plain = Layout::new(Root::new(root).expect("absolute"), Platform::Linux);
    let exec = host_files(&[(old, old_conf)]);
    assert_eq!(refs(&plain, &exec), [DEFAULT_REF]);
}

#[test]
fn xtask_and_the_installer_agree_on_which_refs_are_branches() {
    // Catches: pkg-trust refusing a branch the installer takes, or writing a conf for a
    // ref the installer refuses (which `pkg-kbf-asset` would then ship).
    for r in [
        "refs/heads/main",
        "refs/heads/team/topic-2",
        "refs/heads/a.b_c-d/e",
        "refs/heads/-x",
        "main",
        "refs/tags/v1",
        "refs/heads/",
        "refs/heads/a b",
        "refs/heads/a..b",
        "refs/heads//a",
        "refs/heads/a//b",
        "refs/heads/a/../b",
        "refs/heads/a/./b",
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
