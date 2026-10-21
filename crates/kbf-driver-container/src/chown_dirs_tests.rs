//! Unit tests of the `__chown-dirs` mode: what the walk visits, and its answers.

use std::os::unix::fs::MetadataExt as _;

use super::*;

fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::unit_dir(&format!("chown-dirs-{name}"))
}

/// Catches a walk that changes a file's or a symlink's owner (every cached inode
/// would be re-owned: the reason for the mode), follows a symlink to a directory out
/// of the tree, or misses a directory at any depth.
#[test]
fn the_walk_visits_every_directory_and_nothing_else() {
    let dir = scratch("walk");
    let outside = scratch("walk-outside");
    std::fs::create_dir_all(dir.join("a/b/c")).expect("dirs");
    std::fs::create_dir_all(dir.join("d")).expect("dirs");
    std::fs::write(dir.join("a/file"), b"x").expect("file");
    std::fs::write(dir.join("a/b/c/deep"), b"x").expect("file");
    std::os::unix::fs::symlink(&outside, dir.join("a/link")).expect("symlink");
    let mut seen = Vec::new();
    walk(&dir, &mut |fd| {
        seen.push(rustix::fs::fstat(fd)?.st_ino);
        Ok(())
    })
    .expect("walked");
    seen.sort_unstable();
    let mut want: Vec<u64> = ["", "a", "a/b", "a/b/c", "d"]
        .iter()
        .map(|p| std::fs::metadata(dir.join(p)).expect("dir").ino())
        .collect();
    want.sort_unstable();
    assert_eq!(seen, want);
    let failed = walk(&dir, &mut |_| Err(std::io::Error::other("refused")));
    assert!(failed.is_err());
}

/// Catches arguments the mode cannot read taken as some owner (a missing gid, a name
/// for an id, a path for an owner) and a walk that failed reported as success: the
/// daemon's own ids pass, which an unprivileged process may give. (`kbf-node`'s
/// binary test runs the mode as a process, before the flags are parsed.)
#[test]
fn the_mode_changes_owners_or_says_why_not() {
    let dir = scratch("mode");
    std::fs::create_dir(dir.join("sub")).expect("dir");
    let ids = format!(
        "{}:{}",
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw()
    );
    let path = dir.to_path_buf().into_os_string();
    assert_eq!(run(&[ids.clone().into(), path.clone()]), 0);
    for bad in [
        vec![ids.clone().into()],
        vec!["1".into(), path.clone()],
        vec!["x:1".into(), path.clone()],
        vec!["1:x".into(), path.clone()],
        vec![OsString::from_vec(vec![0xff, b':', b'1']), path.clone()],
    ] {
        assert_eq!(run(&bad), 2, "{bad:?}");
    }
    assert_eq!(run(&[ids.into(), dir.join("absent").into_os_string()]), 1);
    assert_eq!(dispatch(), None, "the test harness ran in the mode");
}

/// Catches the whole-tree walk missing an entry (a file, a symlink, a device) at any
/// depth, handing a directory to the entry callback, or following a symlink out of the
/// tree: the callback sees each non-directory name once, with its directory's
/// descriptor, and nothing outside.
#[test]
fn the_whole_walk_visits_every_entry_and_follows_nothing() {
    let dir = scratch("entries");
    let outside = scratch("entries-outside");
    std::fs::write(outside.join("secret"), b"x").expect("outside file");
    std::fs::create_dir_all(dir.join("a/b")).expect("dirs");
    std::fs::write(dir.join("top"), b"x").expect("file");
    std::fs::write(dir.join("a/file"), b"x").expect("file");
    std::fs::write(dir.join("a/b/deep"), b"x").expect("file");
    std::os::unix::fs::symlink(&outside, dir.join("a/link")).expect("symlink to a directory");
    std::os::unix::fs::symlink(outside.join("secret"), dir.join("a/b/flink")).expect("symlink");
    let mut dirs = 0;
    let mut names = Vec::new();
    walk_entries(
        &dir,
        &mut |_| {
            dirs += 1;
            Ok(())
        },
        &mut |parent, name| {
            // The descriptor is the directory the name is in: stat by it.
            rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            names.push(name.to_string_lossy().into_owned());
            Ok(())
        },
    )
    .expect("walked");
    names.sort();
    assert_eq!(names, ["deep", "file", "flink", "link", "top"]);
    assert_eq!(dirs, 3, "the root, a and a/b");
}

/// Catches `--whole` changing the owner of what a symlink points to (the symlink
/// itself is given, `AT_SYMLINK_NOFOLLOW`), and the mode reporting success for a tree
/// it could not walk. The test's own ids pass, which an unprivileged process may give.
#[test]
fn the_whole_mode_gives_entries_and_says_why_not() {
    let dir = scratch("whole");
    std::fs::create_dir(dir.join("sub")).expect("dir");
    std::fs::write(dir.join("sub/f"), b"x").expect("file");
    std::os::unix::fs::symlink("/nonexistent/target", dir.join("dangling")).expect("symlink");
    let ids = format!(
        "{}:{}",
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw()
    );
    let tree = dir.to_path_buf().into_os_string();
    // A dangling link would fail a `chown` that follows it.
    assert_eq!(
        run(&[ids.clone().into(), "-".into(), WHOLE.into(), tree.clone()]),
        0
    );
    // Both parts in one call.
    assert_eq!(
        run(&[
            ids.clone().into(),
            tree.clone(),
            WHOLE.into(),
            tree.clone(),
            tree.clone()
        ]),
        0
    );
    let absent = dir.join("absent").into_os_string();
    assert_eq!(
        run(&[ids.clone().into(), "-".into(), WHOLE.into(), absent]),
        1
    );
    for bad in [
        vec![ids.clone().into(), "-".into(), WHOLE.into()],
        vec![
            ids.clone().into(),
            "-".into(),
            "--other".into(),
            tree.clone(),
        ],
        vec![ids.clone().into(), "-".into(), tree.clone()],
    ] {
        assert_eq!(run(&bad), 2, "{bad:?}");
    }
    // `-` alone changes nothing and is not an error.
    assert_eq!(run(&[ids.into(), "-".into()]), 0);
}
