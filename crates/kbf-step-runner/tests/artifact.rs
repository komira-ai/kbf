//! The verified artifact: a tree that changed after it was verified never yields a hook.

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use kbf_step_runner::{ArtifactError, Staged, tree_digest};
use support::{Rig, Scratch};

fn tree(name: &str) -> Scratch {
    let s = Scratch::new(name);
    fs::create_dir_all(s.path().join("bin")).unwrap();
    fs::write(s.path().join("bin/hook"), "#!hook\n").unwrap();
    fs::write(s.path().join("VERSION"), "2.0.0\n").unwrap();
    symlink("bin/hook", s.path().join("link")).unwrap();
    s
}

/// Catches a digest that is not a function of the tree: the same tree twice gives one digest,
/// and it is 64 lower-case hex digits.
#[test]
fn the_digest_of_a_tree_is_stable_hex() {
    let s = tree("art-stable");
    let a = tree_digest(s.path()).unwrap();
    assert_eq!(a, tree_digest(s.path()).unwrap());
    assert_eq!(a.len(), 64);
    assert!(
        a.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

/// Catches a digest that misses a kind of change: content, mode, an added or removed file, a
/// rename, a link retargeted, an empty directory added.
#[test]
fn every_kind_of_change_moves_the_digest() {
    let s = tree("art-moves");
    let p = s.path();
    let base = tree_digest(p).unwrap();
    let mut seen = vec![base.clone()];
    let mut step = |change: &dyn Fn()| {
        change();
        let d = tree_digest(p).unwrap();
        assert!(
            !seen.contains(&d),
            "a change left the digest at a value it had before"
        );
        seen.push(d);
    };
    step(&|| fs::write(p.join("VERSION"), "2.0.1\n").unwrap());
    step(&|| fs::set_permissions(p.join("bin/hook"), fs::Permissions::from_mode(0o755)).unwrap());
    step(&|| fs::write(p.join("extra"), "").unwrap());
    step(&|| fs::rename(p.join("extra"), p.join("extra2")).unwrap());
    step(&|| {
        fs::remove_file(p.join("link")).unwrap();
        symlink("VERSION", p.join("link")).unwrap();
    });
    step(&|| fs::create_dir(p.join("empty")).unwrap());
    step(&|| fs::remove_file(p.join("extra2")).unwrap());
}

/// Catches a framing that lets two different trees hash alike: a file `ab` holding `c` and a file
/// `a` holding `bc` are different trees.
#[test]
fn names_and_contents_cannot_run_together() {
    let (x, y) = (Scratch::new("art-frame-x"), Scratch::new("art-frame-y"));
    fs::write(x.path().join("ab"), "c").unwrap();
    fs::write(y.path().join("a"), "bc").unwrap();
    assert_ne!(
        tree_digest(x.path()).unwrap(),
        tree_digest(y.path()).unwrap()
    );
}

/// Catches a check that returns a hook of a tree it never compared: sealed, it yields the path;
/// altered by one byte, it is `Changed` with both digests named.
#[test]
fn a_tree_altered_after_sealing_is_refused() {
    let s = tree("art-altered");
    let staged = Staged::seal(s.path()).unwrap();
    assert_eq!(
        staged.check(Path::new("bin/hook")).unwrap(),
        s.path().join("bin/hook")
    );
    fs::write(s.path().join("bin/hook"), "#!hook!\n").unwrap();
    let err = staged.check(Path::new("bin/hook")).unwrap_err();
    let ArtifactError::Changed { expected, found } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(expected, &staged.digest);
    assert_ne!(found, expected);
    assert!(err.to_string().contains("changed after it was verified"));
}

/// Catches a hook path that can leave the tree: absolute, `..`, empty, a directory, a link, a
/// missing file.
#[test]
fn only_a_plain_relative_regular_file_is_a_hook() {
    let s = tree("art-hook");
    let staged = Staged::seal(s.path()).unwrap();
    for bad in ["/usr/bin/hook", "../x", "", "bin/../bin/hook"] {
        let err = staged.check(Path::new(bad)).unwrap_err();
        assert!(
            matches!(err, ArtifactError::BadHookPath(_)),
            "{bad}: {err:?}"
        );
        assert!(err.to_string().contains("relative path"));
    }
    for missing in ["bin", "link", "nothing"] {
        let err = staged.check(Path::new(missing)).unwrap_err();
        assert!(
            matches!(err, ArtifactError::HookMissing(_)),
            "{missing}: {err:?}"
        );
        assert!(err.to_string().contains("not a regular file"));
    }
}

/// Catches a digest that quietly skips what it cannot hash: a FIFO in the tree is refused by
/// name, and a tree that is not there is an I/O error.
#[test]
fn what_cannot_be_hashed_is_refused() {
    let s = tree("art-fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        s.path().join("pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let err = tree_digest(s.path()).unwrap_err();
    assert!(matches!(&err, ArtifactError::Unsupported(p) if p == Path::new("pipe")));
    assert!(err.to_string().contains("neither a file"));
    let err = tree_digest(&s.path().join("absent")).unwrap_err();
    assert!(matches!(err, ArtifactError::Io(_)));
    assert!(err.to_string().contains("cannot read the staged release"));
    assert!(Staged::seal(&s.path().join("absent")).is_err());
}

/// Catches the rig's two releases hashing alike (which would hide a swap of one for the other).
#[test]
fn the_rigs_two_releases_differ() {
    let rig = Rig::new("art-rig");
    assert_ne!(
        tree_digest(&rig.new_dir).unwrap(),
        tree_digest(&rig.old_dir).unwrap()
    );
}
