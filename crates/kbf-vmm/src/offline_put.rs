//! The writer `image setup` uses to put payload files into a guest's Data volume.
//!
//! It runs as host root on a volume the guest wrote to earlier, so the guest controls
//! every path component under the volume root: a link planted at a parent directory or at
//! the file itself, made immutable (`chflags uchg`) so it cannot be removed, would send a
//! path-following write or chmod onto the host. Here no step follows a link: each parent
//! directory is examined with `lstat` and a link or file in the way is an error; what is
//! at the destination is removed (a removal that fails is an error); the file is created
//! with `O_EXCL | O_NOFOLLOW`; mode and owner are set on the open descriptor.
//!
//! The walk uses path strings (`lstat`, then `mkdir` or `create_new`), not a descriptor
//! held open per directory, so it relies on the guest being powered off with the volume
//! attached host-side: nothing else changes the tree between a check and its use. The
//! last hop is still closed against a race by `O_EXCL | O_NOFOLLOW`. A payload path that
//! ends in `/` or has an empty or `.` component is refused, and the remover works on the
//! parent directory plus the final name, so a trailing slash can never make `lstat` or a
//! removal resolve through a link.
//!
//! Plain Unix, so it builds and is tested on Linux as well as macOS.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::system::OfflineFile;

/// The two things a test must be able to steer: how a leftover is removed, and who owns
/// what is created.
pub struct Env<'a> {
    /// Removes what is at the path without following a link (see [`remove_path`]).
    pub remove: &'a dyn Fn(&Path) -> io::Result<()>,
    /// `(uid, gid)` given to every directory and file created; `None` leaves the creator's.
    pub owner: Option<(u32, u32)>,
}

/// Removes whatever is at `path` without following a link: a directory goes with its
/// contents, a path that is not there is not an error.
///
/// # Errors
/// The path cannot be examined or removed (a file the guest made immutable fails here).
pub fn remove_path(path: &Path) -> io::Result<()> {
    // The parent plus the final name: `a/f/` and `a/f` name the same entry, and a
    // trailing slash must not make the kernel resolve a link at `f`.
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} does not end in a file name", path.display()),
        ));
    };
    let path = parent.join(name);
    let path = path.as_path();
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

/// Sets mode and owner on an open descriptor.
fn settle(file: &std::fs::File, mode: u32, owner: Option<(u32, u32)>) -> io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::fchown(file, Some(uid), Some(gid))?;
    }
    Ok(())
}

/// Makes the directory `at` (mode 0755 whatever the umask is, owner per `env`), and fixes
/// mode and owner on the directory itself, opened without following a link.
fn make_dir(at: &Path, env: &Env<'_>) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(at)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(at)
        })
        .and_then(|d| settle(&d, 0o755, env.owner))
        .map_err(|e| format!("create {}: {e}", at.display()))
}

/// Makes sure every directory on the way to `rel` (a path under `root`) is a real
/// directory, creating the missing ones. A link or a file in the way is an error.
fn ensure_parents(root: &Path, rel: &Path, env: &Env<'_>) -> Result<(), String> {
    let mut at = root.to_owned();
    for c in rel.components() {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} on the guest volume is not a plain directory (a link or a file); refusing to write through it",
                    at.display()
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => make_dir(&at, env)?,
            Err(e) => return Err(format!("stat {}: {e}", at.display())),
        }
    }
    Ok(())
}

/// The path of `f` as a relative path of plain names; anything else (`..`, a root, a
/// trailing `/`, an empty or `.` component) is an error.
fn relative(f: &OfflineFile) -> Result<PathBuf, String> {
    let rel = Path::new(&f.path);
    // `Path::components` drops a trailing slash, `//` and `.`, so check the string too.
    if rel.as_os_str().is_empty()
        || f.path.split('/').any(|s| s.is_empty() || s == ".")
        || rel.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!(
            "payload path {:?} is not a relative path of plain names",
            f.path
        ));
    }
    Ok(rel.to_owned())
}

/// Writes `f` under `root` and reports what was written (read from the open file).
///
/// # Errors
/// A link or file is in the way of a directory, the leftover at the destination cannot be
/// removed, or a create, write, chmod or chown failed; the message names the path.
pub fn put(root: &Path, f: &OfflineFile, env: &Env<'_>) -> Result<Value, String> {
    let rel = relative(f)?;
    ensure_parents(root, rel.parent().unwrap_or(Path::new("")), env)?;
    let path = root.join(&rel);
    (env.remove)(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    let meta = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| {
            file.write_all(&f.contents)?;
            settle(&file, f.mode, env.owner)?;
            file.metadata()
        })
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(json!({
        "path": f.path,
        "uid": meta.uid(),
        "gid": meta.gid(),
        "mode": format!("{:o}", meta.mode() & 0o7777),
        "bytes": meta.len(),
    }))
}

/// Writes every file under `root` as root (`0:0`).
///
/// # Errors
/// The first file that [`put`] refuses.
pub fn put_all(root: &Path, files: &[OfflineFile]) -> Result<Vec<Value>, String> {
    let env = Env {
        remove: &remove_path,
        owner: Some((0, 0)),
    };
    files.iter().map(|f| put(root, f, &env)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::tmp;
    use std::os::unix::fs::symlink;

    fn me() -> (u32, u32) {
        // SAFETY: getuid and getgid cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn file(path: &str, mode: u32) -> OfflineFile {
        OfflineFile {
            path: path.to_owned(),
            contents: b"payload".to_vec(),
            mode,
        }
    }

    /// These tests need permission checks that root bypasses; as root say so and skip.
    fn unless_root(body: impl FnOnce()) {
        // SAFETY: geteuid cannot fail.
        run_unless(unsafe { libc::geteuid() } == 0, body);
    }

    fn run_unless(root: bool, body: impl FnOnce()) {
        if root {
            eprintln!(
                "skipped: running as root, which bypasses the permission checks this test needs"
            );
        } else {
            body();
        }
    }

    /// Catches the root skip running a test body as root, or skipping it for a normal user.
    #[test]
    fn the_root_skip_skips_only_as_root() {
        let mut ran = Vec::new();
        run_unless(true, || ran.push("root"));
        run_unless(false, || ran.push("user"));
        assert_eq!(ran, ["user"]);
    }

    fn real_env() -> Env<'static> {
        Env {
            remove: &remove_path,
            owner: Some(me()),
        }
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).expect("stat").mode() & 0o7777
    }

    /// Catches a writer that drops a payload file, writes the wrong bytes, or reports a
    /// path, size or mode other than what is on disk.
    #[test]
    fn every_payload_file_is_written_and_reported() {
        let root = tmp("put-normal");
        let files = [
            file("Library/LaunchDaemons/a.plist", 0o644),
            file("usr/local/bin/tool", 0o755),
        ];
        let env = real_env();
        let out: Vec<Value> = files
            .iter()
            .map(|f| put(&root, f, &env).expect("put"))
            .collect();
        assert_eq!(out[0]["mode"], "644");
        assert_eq!(out[1]["mode"], "755");
        assert_eq!(out[1]["bytes"], 7);
        assert_eq!(out[1]["path"], "usr/local/bin/tool");
        assert_eq!(
            std::fs::read(root.join("usr/local/bin/tool")).expect("read"),
            b"payload"
        );
        assert_eq!(mode_of(&root.join("usr/local/bin")), 0o755);
        assert_eq!(mode_of(&root.join("Library")), 0o755);
    }

    /// Catches a writer that follows a link planted at the file: the outside file must be
    /// unchanged, in content and mode, and the error must name the path.
    #[test]
    fn a_link_at_the_file_is_replaced_and_its_target_untouched() {
        let root = tmp("put-link-file");
        let outside_dir = tmp("put-link-file-outside");
        let outside = outside_dir.join("victim");
        std::fs::write(&outside, "host data").expect("victim");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).expect("m");
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        symlink(&outside, root.join("etc/f")).expect("link");
        put(&root, &file("etc/f", 0o755), &real_env()).expect("put");
        assert_eq!(std::fs::read_to_string(&outside).expect("r"), "host data");
        assert_eq!(mode_of(&outside), 0o600);
        let m = std::fs::symlink_metadata(root.join("etc/f")).expect("stat");
        assert!(m.is_file(), "a regular file, not a link");
    }

    /// Catches a link at the file that cannot be removed (the guest made it immutable)
    /// being written through: setup must fail naming the path, outside file untouched.
    #[test]
    fn an_unremovable_leftover_fails_naming_the_path_and_touches_nothing() {
        let root = tmp("put-stuck");
        let outside_dir = tmp("put-stuck-outside");
        let outside = outside_dir.join("victim");
        std::fs::write(&outside, "host data").expect("victim");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).expect("m");
        symlink(&outside, root.join("f")).expect("link");
        let stuck = |_: &Path| Err(io::Error::from_raw_os_error(libc::EPERM));
        let env = Env {
            remove: &stuck,
            owner: None,
        };
        let e = put(&root, &file("f", 0o755), &env).expect_err("must fail");
        assert!(e.contains(&root.join("f").display().to_string()), "{e}");
        assert!(e.starts_with("remove "), "{e}");
        assert_eq!(std::fs::read_to_string(&outside).expect("r"), "host data");
        assert_eq!(mode_of(&outside), 0o600);
        assert!(
            std::fs::symlink_metadata(root.join("f"))
                .expect("s")
                .file_type()
                .is_symlink()
        );
    }

    /// Catches a writer that creates files through a link planted at a parent directory,
    /// at the first or a deeper level, or through a plain file in the way.
    #[test]
    fn a_link_or_file_at_a_parent_is_refused_and_the_target_untouched() {
        let root = tmp("put-link-dir");
        let outside = tmp("put-link-dir-outside");
        symlink(&outside, root.join("Library")).expect("link");
        let e = put(
            &root,
            &file("Library/LaunchDaemons/a.plist", 0o644),
            &real_env(),
        )
        .expect_err("link refused");
        assert!(
            e.contains(&root.join("Library").display().to_string()),
            "{e}"
        );
        assert_eq!(std::fs::read_dir(&outside).expect("ls").count(), 0);

        std::fs::create_dir_all(root.join("usr")).expect("usr");
        symlink(&outside, root.join("usr/local")).expect("deep link");
        let e = put(&root, &file("usr/local/bin/t", 0o755), &real_env()).expect_err("deep");
        assert!(
            e.contains(&root.join("usr/local").display().to_string()),
            "{e}"
        );
        assert_eq!(std::fs::read_dir(&outside).expect("ls").count(), 0);

        std::fs::write(root.join("var"), "file").expect("file");
        let e = put(&root, &file("var/x", 0o644), &real_env()).expect_err("file");
        assert!(e.contains(&root.join("var").display().to_string()), "{e}");
    }

    /// Catches a destination directory (with contents) left in place instead of being
    /// replaced by the file, and the new file not getting the requested mode and owner.
    #[test]
    fn ownership_is_set_and_a_directory_at_the_destination_is_replaced() {
        let root = tmp("put-owner");
        std::fs::create_dir_all(root.join("d/f/inner")).expect("dir");
        put(&root, &file("d/f", 0o640), &real_env()).expect("put");
        let m = std::fs::symlink_metadata(root.join("d/f")).expect("s");
        assert!(m.is_file());
        assert_eq!((m.uid(), m.gid()), me());
        assert_eq!(m.mode() & 0o7777, 0o640);
    }

    /// Catches payload paths that climb out of the volume or are absolute or empty.
    #[test]
    fn paths_that_are_not_plain_relative_names_are_refused() {
        let root = tmp("put-badpath");
        for p in [
            "../x",
            "/etc/x",
            "a/../../x",
            "",
            "f/",
            "a//b",
            "./a",
            "a/./b",
            ".",
        ] {
            let e = put(&root, &file(p, 0o644), &real_env()).expect_err(p);
            assert!(e.contains("not a relative path"), "{e}");
        }
    }

    /// Catches a trailing slash resolving through a planted link: `f/` with `f` a link to
    /// a directory outside must be refused (before the fix `remove_dir_all` ran through the
    /// link), and the remover itself must treat `f/` as the entry `f`.
    #[test]
    fn a_trailing_slash_never_resolves_through_a_link() {
        let root = tmp("put-slash");
        let outside = tmp("put-slash-outside");
        std::fs::write(outside.join("keep"), "x").expect("keep");
        symlink(&outside, root.join("f")).expect("link");
        let e = put(&root, &file("f/", 0o644), &real_env()).expect_err("refused");
        assert!(e.contains("not a relative path"), "{e}");
        assert!(outside.join("keep").exists());
        remove_path(&root.join("f/")).expect("removes the link itself");
        assert!(outside.join("keep").exists());
        assert!(std::fs::symlink_metadata(root.join("f")).is_err());
        remove_path(Path::new("/")).expect_err("no final name");
        remove_path(&root.join("g/..")).expect_err("no final name");
    }

    /// Catches a stat or create failure being swallowed: a parent that cannot be searched,
    /// and a destination that cannot be created.
    #[test]
    fn unreadable_parents_and_unwritable_directories_are_errors() {
        unless_root(|| {
            let root = tmp("put-eacces");
            std::fs::create_dir_all(root.join("locked/sub")).expect("dirs");
            std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
                .expect("lock");
            let stat = put(&root, &file("locked/sub/f", 0o644), &real_env()).expect_err("stat");
            assert!(stat.starts_with("stat "), "{stat}");
            std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o555))
                .expect("ro");
            let create = put(&root, &file("locked/f", 0o644), &real_env()).expect_err("create");
            assert!(create.contains("locked/f"), "{create}");
            let mkdir =
                put(&root, &file("locked/newdir/f", 0o644), &real_env()).expect_err("mkdir");
            assert!(mkdir.starts_with("create "), "{mkdir}");
            std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
                .expect("unlock");
        });
    }

    /// Catches the production remover following a link or treating a missing path as an
    /// error.
    #[test]
    fn the_real_remover_does_not_follow_links_and_accepts_a_missing_path() {
        let root = tmp("put-remove");
        let outside = tmp("put-remove-outside");
        std::fs::write(outside.join("keep"), "x").expect("keep");
        symlink(&outside, root.join("l")).expect("link");
        remove_path(&root.join("l")).expect("rm link");
        assert!(outside.join("keep").exists());
        remove_path(&root.join("absent")).expect("missing is fine");
        std::fs::write(root.join("plain"), "x").expect("plain");
        remove_path(&root.join("plain/inner")).expect_err("not a directory is an error");
        std::fs::write(root.join("f"), "x").expect("file");
        remove_path(&root.join("f")).expect("rm file");
        assert!(!root.join("f").exists());
    }

    /// Catches a failed mode or owner change on the file being ignored (a non-root caller
    /// cannot give a file to root).
    #[test]
    fn a_refused_owner_change_on_the_file_is_an_error() {
        unless_root(|| {
            let root = tmp("put-chown");
            let env = Env {
                remove: &remove_path,
                owner: Some((0, 0)),
            };
            let e = put(&root, &file("f", 0o644), &env).expect_err("chown refused");
            assert!(e.starts_with("write "), "{e}");
            assert!(e.contains(&root.join("f").display().to_string()), "{e}");
        });
    }

    /// Catches `put_all` not asking for root ownership (a non-root run is refused, naming the path).
    #[test]
    fn put_all_gives_files_to_root_and_names_the_path_when_refused() {
        unless_root(|| {
            let root = tmp("put-all");
            // The owner is 0:0, which a test run as a normal user cannot give away to.
            let e = put_all(&root, &[file("a/x", 0o644)]).expect_err("not root");
            assert!(e.contains(&root.join("a").display().to_string()), "{e}");
        });
    }
}
