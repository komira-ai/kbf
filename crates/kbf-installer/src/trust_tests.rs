use kbf_host::exec::Entry;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;
use crate::fixtures::trusted_host;
use crate::layout;

fn gh() -> HostPath {
    layout::path(crate::fixtures::GH_PATH)
}

fn dir(mode: u32, uid: u32) -> Entry {
    Entry::Dir {
        perms: Perms::new(mode),
        owner: Owner { uid, gid: 0 },
    }
}

/// The path the refusal names and its code.
fn refused(exec: &FakeExec) -> (String, &'static str) {
    let err = trusted_chain(exec, &gh(), Kind::File).expect_err("refused");
    let message = err.to_string();
    let (named, _) = message.split_once(" is not trusted").expect("names a path");
    (named.to_owned(), err.code())
}

/// Catches a chain that is refused when it is the host's, or accepted when anything on
/// it is wrong: a root-owned tree of 0755 directories and a 0755 file passes.
#[test]
fn a_root_owned_chain_closed_to_writes_is_trusted() {
    let exec = trusted_host();
    trusted_chain(&exec, &gh(), Kind::File).expect("trusted");
    trusted_chain(
        &exec,
        &crate::layout::path(crate::fixtures::TRUST_DIR),
        Kind::Dir,
    )
    .expect("a directory");
}

/// Catches each way a path on the trust list can be replaced by someone else, named by
/// the path that is wrong (not the one asked about): `gh` itself, and each directory
/// above it, up to the top-level one, for an owner that is not root and a mode group or
/// others can write (0775, 0757 and a sticky 1777 are all refused).
///
/// Mutants seen red: the group-write bit left out of the mask (0775 passes); the walk
/// stopping at the first directory (a bad `/usr` passes); the owner not checked.
#[test]
fn a_path_or_directory_others_could_replace_is_refused_naming_it() {
    let path = [
        (crate::fixtures::GH_PATH, true),
        (crate::fixtures::TRUST_DIR, false),
        ("/usr/share/kbf-installer", false),
        ("/usr/share", false),
        ("/usr", false),
    ];
    for (p, is_file) in path {
        for (mode, uid, code) in [
            (0o775, 0, "untrusted-writable"),
            (0o757, 0, "untrusted-writable"),
            (0o1777, 0, "untrusted-writable"),
            (0o755, 501, "untrusted-owner"),
        ] {
            let mut exec = trusted_host();
            let entry = if is_file {
                Entry::File {
                    perms: Perms::new(mode),
                    owner: Owner { uid, gid: 0 },
                    bytes: Vec::new(),
                }
            } else {
                dir(mode, uid)
            };
            exec.entries.insert(HostPath::new(p).expect("path"), entry);
            let (named, got) = refused(&exec);
            assert_eq!((named.as_str(), got), (p, code), "{p} {mode:o} {uid}");
        }
    }
}

/// Catches a link accepted in place of a file or of a directory above it (a link to a
/// root-owned file is still a path whose target someone may be able to change), a path
/// of the wrong kind, a missing path, and a stat that fails.
#[test]
fn a_link_the_wrong_kind_a_missing_path_and_a_failed_stat_are_refused() {
    let mut exec = trusted_host();
    exec.entries.insert(
        gh(),
        Entry::Link {
            target: "gh-2.102.0".into(),
        },
    );
    assert_eq!(
        refused(&exec),
        (crate::fixtures::GH_PATH.to_owned(), "untrusted-link")
    );

    let mut exec = trusted_host();
    exec.entries.insert(
        HostPath::new("/usr/share").expect("path"),
        Entry::Link {
            target: "private/usr/share".into(),
        },
    );
    assert_eq!(refused(&exec), ("/usr/share".to_owned(), "untrusted-link"));

    let mut exec = trusted_host();
    exec.entries.insert(gh(), dir(0o755, 0));
    assert_eq!(
        refused(&exec),
        (crate::fixtures::GH_PATH.to_owned(), "untrusted-kind")
    );
    let message = trusted_chain(&exec, &gh(), Kind::File)
        .expect_err("wrong kind")
        .to_string();
    assert_eq!(
        message,
        format!(
            "{} is not trusted: it is a directory, not a file",
            crate::fixtures::GH_PATH
        )
    );

    let mut exec = trusted_host();
    exec.entries.remove(&gh());
    assert_eq!(
        refused(&exec),
        (crate::fixtures::GH_PATH.to_owned(), "untrusted-missing")
    );
    let mut exec = trusted_host();
    exec.entries.remove(&HostPath::new("/usr").expect("path"));
    assert_eq!(refused(&exec), ("/usr".to_owned(), "untrusted-missing"));

    // The fake cannot read this one.
    let unreadable = HostPath::new("/unreadable").expect("path");
    let err = trusted_chain(&trusted_host(), &unreadable, Kind::File).expect_err("unreadable");
    assert_eq!(err.code(), "untrusted-unreadable");
}

/// Catches the parent of a path being taken wrong: a top-level path has none, anything
/// else has the directory it is in.
#[test]
fn the_parent_of_a_path_is_its_directory_and_a_top_level_path_has_none() {
    let p = |s: &str| HostPath::new(s).expect("path");
    assert_eq!(parent(&p("/usr/local/kbf")), Some(p("/usr/local")));
    assert_eq!(parent(&p("/usr/local")), Some(p("/usr")));
    assert_eq!(parent(&p("/usr")), None);
}
