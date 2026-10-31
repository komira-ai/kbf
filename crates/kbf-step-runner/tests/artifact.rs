//! The verified artifact: a tree that changed after it was verified never yields a hook.

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use kbf_step_runner::{ArtifactError, Staged, tree_digest, tree_digest_with};
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

/// Catches a staged release that others can change, or reached through a link, being trusted: a
/// tree writable by group, a parent writable by others, and a path with a linked component are
/// `Untrusted`; the same tree closed is accepted.
#[test]
fn the_tree_and_its_parents_must_be_closed_and_unlinked() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("art-place");
    let outer = s.path().join("outer");
    let inner = outer.join("inner");
    fs::create_dir_all(inner.join("bin")).unwrap();
    fs::write(inner.join("bin/hook"), "#!hook\n").unwrap();
    let staged = Staged::seal(&inner).unwrap();
    let hook = Path::new("bin/hook");
    staged.check(hook).unwrap();

    fs::set_permissions(&inner, fs::Permissions::from_mode(0o775)).unwrap();
    let err = staged.check(hook).unwrap_err();
    assert!(matches!(&err, ArtifactError::Untrusted(p) if p == &inner));
    assert!(err.to_string().contains("closed to group and others"));
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();

    fs::set_permissions(&outer, fs::Permissions::from_mode(0o757)).unwrap();
    assert!(matches!(
        staged.check(hook).unwrap_err(),
        ArtifactError::Untrusted(p) if p == outer
    ));
    fs::set_permissions(&outer, fs::Permissions::from_mode(0o755)).unwrap();

    let link = s.path().join("link");
    symlink(&outer, &link).unwrap();
    let through = Staged {
        dir: link.join("inner"),
        digest: staged.digest.clone(),
    };
    assert!(matches!(
        through.check(hook).unwrap_err(),
        ArtifactError::Untrusted(p) if p == link
    ));
    staged.check(hook).unwrap();
}

fn host_owner(meta: &fs::Metadata) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    (meta.uid(), meta.gid())
}

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Catches a hook path that leaves the tree through a linked directory in the middle of it: with
/// `bin` linked to a directory inside or outside the tree, `bin/hook` is not a hook, though the
/// last component is a regular file.
#[test]
fn a_hook_behind_a_linked_directory_is_not_a_hook() {
    let s = Scratch::new("art-mid-link");
    let outside = Scratch::new("art-mid-link-out");
    fs::write(outside.path().join("hook"), "#!out\n").unwrap();
    fs::create_dir_all(s.path().join("real")).unwrap();
    fs::write(s.path().join("real/hook"), "#!in\n").unwrap();
    symlink("real", s.path().join("inside")).unwrap();
    symlink(outside.path(), s.path().join("outside")).unwrap();
    let staged = Staged::seal(s.path()).unwrap();
    staged.check(Path::new("real/hook")).unwrap();
    for hook in ["inside/hook", "outside/hook"] {
        let err = staged.check(Path::new(hook)).unwrap_err();
        assert!(
            matches!(err, ArtifactError::HookMissing(_)),
            "{hook}: {err:?}"
        );
    }
}

/// Catches an owner that is not part of the digest: the same tree read with another user or
/// group id has another digest, so a restore that chowns a file is a change.
#[test]
fn the_owner_of_every_entry_is_in_the_digest() {
    let s = tree("art-owner-digest");
    let p = s.path();
    let same = tree_digest_with(p, &host_owner).unwrap();
    assert_eq!(same, tree_digest(p).unwrap());
    let a = tree_digest_with(p, &|_| (1, 2)).unwrap();
    let b = tree_digest_with(p, &|_| (1, 3)).unwrap();
    let c = tree_digest_with(p, &|_| (2, 2)).unwrap();
    assert!(a != b && a != c && b != c && a != same);
}

/// Catches a foreign owner being trusted: with no trusted owner the tree is refused; an entry
/// inside the tree owned by another user is refused too, though the directories are fine.
#[test]
fn only_the_trusted_owners_may_own_the_tree_and_what_is_in_it() {
    let s = tree("art-foreign");
    let staged = Staged::seal(s.path()).unwrap();
    let hook = Path::new("bin/hook");
    staged.check_as(hook, &[me(), 0], &host_owner).unwrap();
    let err = staged.check_as(hook, &[], &host_owner).unwrap_err();
    assert!(matches!(err, ArtifactError::Untrusted(_)));
    let files_of_another = |meta: &fs::Metadata| {
        let (uid, gid) = host_owner(meta);
        (if meta.is_file() { 4242 } else { uid }, gid)
    };
    let sealed = Staged {
        dir: s.path().to_path_buf(),
        digest: tree_digest_with(s.path(), &files_of_another).unwrap(),
    };
    let err = sealed
        .check_as(hook, &[me(), 0], &files_of_another)
        .unwrap_err();
    assert!(
        matches!(&err, ArtifactError::Untrusted(p) if p.ends_with("VERSION") || p.ends_with("hook")),
        "{err:?}"
    );
}

/// Catches an entry a service could rewrite or that runs with more than its owner's rights: a
/// setuid or setgid file, a file or directory writable by group or others, inside the tree.
#[test]
fn a_tree_with_setuid_or_writable_entries_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    for (name, mode, dir) in [
        ("suid", 0o4755, false),
        ("sgid", 0o2755, false),
        ("gw", 0o664, false),
        ("ow", 0o646, false),
        ("dgw", 0o775, true),
    ] {
        let s = tree(&format!("art-entry-{name}"));
        let target = if dir {
            s.path().join("bin")
        } else {
            s.path().join("VERSION")
        };
        let staged = Staged::seal(s.path()).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
        let err = staged.check(Path::new("bin/hook")).unwrap_err();
        assert!(
            matches!(&err, ArtifactError::Untrusted(_)),
            "{name}: {err:?}"
        );
    }
}

/// Catches a staged directory that is relative or has `..`: the checks of its ancestors would
/// then be made on something else than the tree that is run.
#[test]
fn the_staged_directory_must_be_absolute_and_plain() {
    for dir in ["relative/tree", "/a/../b", ""] {
        let staged = Staged {
            dir: dir.into(),
            digest: "0".repeat(64),
        };
        let err = staged.check(Path::new("hook")).unwrap_err();
        assert!(
            matches!(err, ArtifactError::Untrusted(_)),
            "{dir:?}: {err:?}"
        );
    }
}
