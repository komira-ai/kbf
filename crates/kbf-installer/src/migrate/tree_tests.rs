use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::*;

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn rules() -> Rules {
    Rules::release(me())
}

/// A scratch directory that is removed when the test ends. The files these tests plant are
/// executable, and the coverage tool takes any executable file under `target/` for a
/// program to read and fails on one that is not.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    crate::migrate::rig::root_umask();
    Scratch(kbf_host::testkit::scratch(&format!("migrate-tree-{name}")))
}

fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, bytes).expect("file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("mode");
}

/// A release as the dev installer leaves one: a binary, a manifest and a units directory.
fn release(dir: &Path) {
    put(&dir.join("kbf"), b"binary", 0o755);
    put(&dir.join("manifest.json"), b"{}", 0o644);
    put(&dir.join("units/kbf.service"), b"[Unit]\n", 0o644);
}

fn open(path: &Path) -> OwnedFd {
    open_root(path).expect("open")
}

fn refused(result: Result<[u8; 32], TreeError>) -> String {
    match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("accepted what the rules refuse"),
    }
}

/// Catches a copy that is not the tree it came from (a file dropped, a name changed, a
/// directory flattened), or that keeps the source's modes: the copy's digest equals the
/// source's, files are 0755 or 0644 and directories 0755 whatever the source said.
///
/// Mutant seen red: `finish` without the `fchmod` (the copy keeps the 0600 it was made with).
#[test]
fn a_copy_has_the_digest_of_its_source_and_fresh_modes() {
    let root = scratch("copy");
    release(&root.join("src"));
    put(&root.join("src/odd"), b"x", 0o600);
    fs::create_dir(root.join("dst")).expect("dst");
    let read = copy(&rules(), &open(&root.join("src")), &open(&root.join("dst"))).expect("copy");
    assert_eq!(
        digest(&rules(), &open(&root.join("dst"))).expect("digest"),
        read
    );
    let mode = |p: &str| {
        fs::metadata(root.join(p))
            .expect("meta")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("dst/kbf"), 0o755);
    assert_eq!(mode("dst/manifest.json"), 0o644);
    assert_eq!(mode("dst/odd"), 0o644);
    assert_eq!(mode("dst/units"), 0o755);
    assert_eq!(
        fs::read(root.join("dst/units/kbf.service")).expect("unit"),
        b"[Unit]\n"
    );
}

/// Catches a digest that ignores content, names, the executable bit or the shape of the
/// tree: each change below must move it. A time or an inode must not.
///
/// Mutant seen red: dropping the content hash from `entry` (the first assertion).
#[test]
fn the_digest_moves_with_content_names_and_the_executable_bit_only() {
    let root = scratch("digest");
    release(&root.join("a"));
    let base = digest(&rules(), &open(&root.join("a"))).expect("digest");
    let of = |name: &str| digest(&rules(), &open(&root.join(name))).expect("digest");
    release(&root.join("same"));
    assert_eq!(of("same"), base, "an equal tree has an equal digest");
    release(&root.join("content"));
    put(&root.join("content/kbf"), b"binarY", 0o755);
    assert_ne!(of("content"), base);
    release(&root.join("exec"));
    fs::set_permissions(root.join("exec/kbf"), fs::Permissions::from_mode(0o644)).expect("mode");
    assert_ne!(of("exec"), base);
    release(&root.join("name"));
    fs::rename(
        root.join("name/manifest.json"),
        root.join("name/manifest.jsom"),
    )
    .expect("rename");
    assert_ne!(of("name"), base);
    release(&root.join("shape"));
    fs::rename(
        root.join("shape/units/kbf.service"),
        root.join("shape/kbf.service"),
    )
    .expect("move");
    assert_ne!(of("shape"), base);
}

/// Catches a copy through a link: the entry is a symlink (to a file outside the tree),
/// and the copy fails instead of reading the target.
///
/// Mutant seen red: `OFlags::NOFOLLOW` removed from `entry` (the secret is copied).
#[test]
fn a_symlink_is_refused_not_followed() {
    let root = scratch("symlink");
    release(&root.join("src"));
    put(&root.join("secret"), b"secret", 0o644);
    symlink(root.join("secret"), root.join("src/link")).expect("link");
    let why = refused(digest(&rules(), &open(&root.join("src"))));
    assert!(why.contains("link"), "{why}");
    fs::create_dir(root.join("dst")).expect("dst");
    assert!(copy(&rules(), &open(&root.join("src")), &open(&root.join("dst"))).is_err());
    assert!(!root.join("dst/link").exists());
}

/// Catches a FIFO that hangs the migration (a blocking open waits for a writer) or is
/// copied: it is refused as not a regular file, at once.
///
/// Mutant seen red: `NONBLOCK` removed from `entry` (the test never returns).
#[test]
fn a_fifo_is_refused_without_blocking() {
    let root = scratch("fifo");
    release(&root.join("src"));
    rustix::fs::mknodat(
        rustix::fs::CWD,
        root.join("src/pipe"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .expect("fifo");
    let why = refused(digest(&rules(), &open(&root.join("src"))));
    assert!(why.contains("not a regular file or a directory"), "{why}");
}

/// Catches a file with a second hard link (the other name may be a path an unprivileged
/// user can still write), a setuid file, and a file or directory group- or
/// other-writable: each is refused with its own reason.
///
/// Mutant seen red: each `judge` branch deleted in turn.
#[test]
fn links_setuid_and_writable_files_are_refused() {
    let root = scratch("modes");
    let case = |name: &str, change: &dyn Fn(&Path)| {
        release(&root.join(name));
        change(&root.join(name));
        refused(digest(&rules(), &open(&root.join(name))))
    };
    let why = case("hard", &|d| {
        fs::hard_link(d.join("kbf"), d.join("kbf2")).expect("link")
    });
    assert!(why.contains("another hard link"), "{why}");
    let why = case("suid", &|d| {
        fs::set_permissions(d.join("kbf"), fs::Permissions::from_mode(0o4755)).expect("mode");
    });
    assert!(why.contains("setuid"), "{why}");
    let why = case("gw", &|d| {
        fs::set_permissions(d.join("manifest.json"), fs::Permissions::from_mode(0o664))
            .expect("mode");
    });
    assert!(why.contains("writable by group or other"), "{why}");
    let why = case("dw", &|d| {
        fs::set_permissions(d.join("units"), fs::Permissions::from_mode(0o757)).expect("mode");
    });
    assert!(why.contains("writable by group or other"), "{why}");
}

/// Catches a tree accepted whatever its owner: with the expected owner another user,
/// the top directory is refused.
///
/// Mutant seen red: the `st_uid` comparison removed from `judge`.
#[test]
fn a_tree_of_another_owner_is_refused() {
    let root = scratch("owner");
    release(&root.join("src"));
    let other = Rules::release(me() + 1);
    let why = refused(digest(&other, &open(&root.join("src"))));
    assert!(why.contains("not owned by the expected user"), "{why}");
}

/// Catches the caps that bound a hostile tree: too many entries, nesting too deep, a
/// file over the size cap are each refused, and a tree inside the caps is not.
///
/// Mutant seen red: `entries > max_entries` made `>=`/removed; depth check removed.
#[test]
fn the_caps_bound_the_tree() {
    let root = scratch("caps");
    release(&root.join("src"));
    let tight = |f: &dyn Fn(&mut Rules)| {
        let mut r = rules();
        f(&mut r);
        r
    };
    let src = open(&root.join("src"));
    assert!(
        digest(&tight(&|r| r.max_entries = 4), &src).is_ok(),
        "the four entries fit"
    );
    let why = refused(digest(&tight(&|r| r.max_entries = 3), &src));
    assert!(why.contains("too many entries"), "{why}");
    let why = refused(digest(&tight(&|r| r.max_depth = 0), &src));
    assert!(why.contains("nested too deep"), "{why}");
    let why = refused(digest(&tight(&|r| r.max_file_bytes = 5), &src));
    assert!(why.contains("size cap"), "{why}");
}

/// Catches a copy that mints the `VERIFIED` marker: a release directory that holds one at
/// its top is refused (only the verify pipeline writes it), while the same name deeper
/// down is an ordinary file.
///
/// Mutant seen red: the `reserved` check removed from `Walk::dir`.
#[test]
fn a_verified_marker_in_the_source_is_refused() {
    let root = scratch("reserved");
    release(&root.join("src"));
    put(&root.join("src/units/VERIFIED"), b"", 0o644);
    assert!(digest(&rules(), &open(&root.join("src"))).is_ok());
    put(&root.join("src/VERIFIED"), b"", 0o644);
    let why = refused(digest(&rules(), &open(&root.join("src"))));
    assert!(
        why.contains("VERIFIED") && why.contains("only a check may write"),
        "{why}"
    );
}

/// Catches file capabilities copied: a file with the denied extended attribute is refused.
/// The attribute under test is a `user.` one, which an unprivileged user can set; the
/// production name (`security.capability`) is the same code path with another string.
#[cfg(target_os = "linux")]
#[test]
fn a_file_with_the_denied_attribute_is_refused() {
    let root = scratch("xattr");
    release(&root.join("src"));
    let file = fs::File::open(root.join("src/kbf")).expect("open");
    rustix::fs::fsetxattr(
        &file,
        "user.kbf-test",
        b"1",
        rustix::fs::XattrFlags::empty(),
    )
    .expect("this file system keeps user extended attributes");
    let mut r = rules();
    r.denied_xattr = "user.kbf-test";
    let why = refused(digest(&r, &open(&root.join("src"))));
    assert!(why.contains("file capabilities"), "{why}");
    assert!(
        digest(&rules(), &open(&root.join("src"))).is_ok(),
        "other names are not denied"
    );
}

/// Catches `read_file` that follows a link or reads without bound: a missing name is
/// `None`, a link and an over-cap file are refused, a regular file is read whole.
#[test]
fn read_file_judges_what_it_reads() {
    let root = scratch("read");
    put(&root.join("d/state.json"), b"{}", 0o600);
    put(&root.join("d/big"), &[0u8; 100], 0o600);
    put(&root.join("outside"), b"x", 0o600);
    symlink(root.join("outside"), root.join("d/link")).expect("link");
    let d = open(&root.join("d"));
    assert_eq!(
        read_file(&rules(), &d, "state.json", 10).expect("read"),
        Some(b"{}".to_vec())
    );
    assert_eq!(read_file(&rules(), &d, "absent", 10).expect("read"), None);
    assert!(read_file(&rules(), &d, "link", 10).is_err());
    let why = read_file(&rules(), &d, "big", 10)
        .expect_err("over the cap")
        .to_string();
    assert!(why.contains("size cap"), "{why}");
    let sub = root.join("d/sub");
    fs::create_dir(&sub).expect("dir");
    let why = read_file(&rules(), &d, "sub", 10)
        .expect_err("a directory")
        .to_string();
    assert!(why.contains("not a regular file"), "{why}");
}

/// Catches a write that can be seen half done or that leaves its temporary name behind:
/// the file is replaced whole with the mode given, and a leftover `.name.new` from an
/// interrupted run is replaced rather than failing the next one.
///
/// Mutant seen red: the `unlinkat` of the leftover removed (EEXIST on the second write).
#[test]
fn replace_file_writes_whole_and_survives_a_leftover() {
    let root = scratch("replace");
    fs::create_dir(root.join("d")).expect("dir");
    put(&root.join("d/.out.new"), b"half", 0o600);
    let d = open(&root.join("d"));
    replace_file(&d, "out", 0o640, b"first").expect("write");
    replace_file(&d, "out", 0o600, b"second").expect("rewrite");
    assert_eq!(fs::read(root.join("d/out")).expect("read"), b"second");
    assert_eq!(
        fs::metadata(root.join("d/out"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!root.join("d/.out.new").exists());
}

/// Catches a removal that follows a link out of the tree, or leaves a part behind: a
/// nested tree and a link are removed, the link's target survives, and a tree nested past
/// the bound is an error rather than a stack overflow.
///
/// Mutant seen red: `SYMLINK_NOFOLLOW` removed from `remove_at` (the target is deleted).
#[test]
fn remove_tree_does_not_follow_links() {
    let root = scratch("remove");
    release(&root.join("d/tree"));
    put(&root.join("keep/precious"), b"x", 0o644);
    symlink(root.join("keep"), root.join("d/tree/units/out")).expect("link");
    symlink(root.join("keep"), root.join("d/solo")).expect("link");
    let d = open(&root.join("d"));
    remove_tree(&d, "tree").expect("remove");
    remove_tree(&d, "solo").expect("remove a link");
    assert!(!root.join("d/tree").exists() && root.join("d/solo").symlink_metadata().is_err());
    assert!(
        root.join("keep/precious").exists(),
        "the link's target survived"
    );
    let mut deep = root.join("d/deep");
    for _ in 0..20 {
        deep = deep.join("x");
    }
    fs::create_dir_all(&deep).expect("deep");
    assert!(remove_tree(&d, "deep").is_err());
    assert!(remove_tree(&d, "absent").is_err());
}

/// Catches the two error sorters mis-sorting: `absent` turns exactly "not found" into
/// `None` (for an `io::Error` and for an `Errno`) and passes every other error on;
/// `except` counts exactly the named errno as done-already and passes the rest on.
#[test]
fn absent_and_except_sort_their_errors() {
    use std::io::{Error, ErrorKind};

    assert_eq!(absent(Ok::<_, Error>(7)).expect("ok"), Some(7));
    assert_eq!(
        absent(Err::<u8, _>(Error::from(ErrorKind::NotFound))).expect("none"),
        None
    );
    assert_eq!(absent(Err::<u8, _>(Errno::NOENT)).expect("none"), None);
    let other = absent(Err::<u8, _>(Error::from(ErrorKind::PermissionDenied)));
    assert_eq!(
        other.expect_err("passed on").kind(),
        ErrorKind::PermissionDenied
    );
    let other = absent(Err::<u8, _>(Errno::ACCESS));
    assert_eq!(
        other.expect_err("passed on").kind(),
        ErrorKind::PermissionDenied
    );

    assert!(except(Ok(()), Errno::EXIST).expect("did it"));
    assert!(!except(Err(Errno::EXIST), Errno::EXIST).expect("was there"));
    let other = except(Err(Errno::ACCESS), Errno::EXIST);
    assert_eq!(
        other.expect_err("passed on").kind(),
        ErrorKind::PermissionDenied
    );
}

/// Catches a check that takes a file for a directory: `expect_dir` and a digest of a plain
/// file are refused as "not a directory".
#[test]
fn a_file_is_not_a_directory() {
    let root = scratch("notdir");
    put(&root.join("f"), b"x", 0o644);
    let parent = open(&root);
    let file = rustix::fs::openat(
        &parent,
        "f",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open");
    let error = expect_dir(&rules(), &file, "f").expect_err("a file");
    assert!(error.to_string().contains("not a directory"), "{error}");
    let error = refused(digest(&rules(), &file));
    assert!(error.contains("not a directory"), "{error}");
}

/// Catches an extended-attribute probe that reads every failure as "no capabilities": only
/// "no such attribute" and "not supported" mean none; a descriptor the call cannot use (one
/// opened for the path alone) is an error.
#[cfg(target_os = "linux")]
#[test]
fn an_attribute_probe_that_fails_for_another_reason_is_an_error() {
    let root = scratch("xattr-fd");
    put(&root.join("f"), b"x", 0o644);
    let parent = open(&root);
    let path_only = rustix::fs::openat(
        &parent,
        "f",
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open");
    assert!(has_xattr(&path_only, "security.capability").is_err());
}

/// Catches a replacement that goes ahead over a leftover it cannot remove: a non-empty
/// directory where the new name goes is an error naming it, and the file is not touched.
#[test]
fn replace_file_stops_on_a_leftover_it_cannot_remove() {
    let root = scratch("replace-stuck");
    put(&root.join("d/out"), b"old", 0o600);
    put(&root.join("d/.out.new/inner"), b"x", 0o600);
    let d = open(&root.join("d"));
    let error = replace_file(&d, "out", 0o600, b"new").expect_err("stuck");
    assert!(error.to_string().contains(".out.new"), "{error}");
    assert_eq!(fs::read(root.join("d/out")).expect("kept"), b"old");
}

/// Catches a removal that takes a name with a NUL in it for another name: the name cannot
/// be passed to the system at all, and that is an error, not a removal of what comes
/// before the NUL.
#[test]
fn a_name_with_a_nul_is_not_removed() {
    let root = scratch("remove-nul");
    put(&root.join("d/a"), b"x", 0o644);
    let d = open(&root.join("d"));
    let error = remove_tree(&d, "a\0b").expect_err("no such name");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(root.join("d/a").exists());
}
