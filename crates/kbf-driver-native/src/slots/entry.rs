//! A pool user's `/var/folders` entry, cleaned **as the pool user**.
//!
//! The daemon is root, but a launch daemon has no Full Disk Access, and without it root
//! cannot clear the flags or the ACLs of the entry's `0`, `C`, `T` and `X`, nor list
//! them: the system protects a user's temporary and cache folders from a process that
//! is not the user's own. The user itself can do all of it to the files it owns. So the
//! entry is cleaned by a child that drops to the slot's uid and gid (`setgroups`,
//! `setgid`, `setuid`, in that order) and then runs one of the modes below, or `find`
//! for the ACLs. The modes drop **in the process that is already running**, so the
//! pool user never has to be able to execute the daemon's binary; `find` goes through
//! `__as-user` ([`crate::slots::AS_USER_ARG`]) and a system tool. Nothing here runs a
//! shell, and root applies no flag or ACL change to the entry.
//!
//! - [`clear_flags`] (C6): the ACLs of the uid's entries below the four folders (`find
//!   -x ... -user <uid> -exec chmod -N`), the user flags and the owner's mode of everything
//!   the uid owns inside them ([`CLEAR`]), then the ACLs again. The entry and its four
//!   folders keep the flags and ACLs the system set (`sunlnk` is root's to clear, and
//!   root without Full Disk Access cannot); the check accepts them.
//! - [`restore`] (C10): the four folders' mode, as the uid ([`RESTORE`]); their flags and
//!   ACLs stay as the system has them.
//! - [`empty`] (C7): the entry's `0`, `C`, `T` and `X` emptied and kept, anything else in
//!   the entry removed ([`EMPTY`]).
//! - [`left`] (the check): what an uid-side look finds in the entry afterwards
//!   ([`LOOK`]); root cannot look inside the folders, so it asks the uid.
//!
//! The decisions are plain functions over a [`Run`], so the order of the calls and who
//! each runs as are tested on every platform with a fake; the real [`Run`] is the
//! Mac host's.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;

use kbf_mac_session::sweep;

use super::probe::{self, SKELETON};

/// The argument after argv0 that selects this binary's entry mode:
/// `__entry <mode> <uid> <gid> <dir>`.
pub const ARG: &str = "__entry";
/// The mode that clears the flags and modes below the entry: `__entry clear <uid> <gid> <dir>`.
pub const CLEAR: &str = "clear";
/// The mode that empties the entry: `__entry empty <uid> <gid> <dir>`.
pub const EMPTY: &str = "empty";
/// The mode that gives the four folders their mode back: `__entry restore <uid> <gid> <dir>`.
pub const RESTORE: &str = "restore";
/// The mode that prints what is left in the entry: `__entry look <uid> <gid> <dir>`.
pub const LOOK: &str = "look";

const USAGE: &str = "usage: __entry clear|empty|look|restore <uid> <gid> <dir>";

const FIND: &str = "/usr/bin/find";
const CHMOD: &str = "/bin/chmod";

/// The slot user a tool runs as, and the daemon binary that drops to it.
pub(crate) struct AsUid<'a> {
    /// The daemon binary: it answers [`crate::slots::AS_USER_ARG`] and [`ARG`].
    pub program: &'a Path,
    pub uid: u32,
    pub gid: u32,
}

/// Runs `program` with `args` and says what it printed, or why it failed. The real one
/// is the Mac host's (a plain child, a plain environment); a test's records the calls.
pub(crate) type Run<'a> = dyn FnMut(&Path, &[OsString]) -> Result<String, String> + 'a;

impl AsUid<'_> {
    /// The arguments of `self.program` that drop to the uid and then run `tool` with
    /// `args`.
    pub(crate) fn launch(&self, tool: &OsStr, args: &[OsString]) -> Vec<OsString> {
        let mut all: Vec<OsString> = vec![
            super::AS_USER_ARG.into(),
            self.uid.to_string().into(),
            self.gid.to_string().into(),
            "--".into(),
            tool.to_owned(),
        ];
        all.extend_from_slice(args);
        all
    }

    /// This binary's entry `mode` on `dir`, which drops to the uid itself.
    fn mode(&self, mode: &str, dir: &Path, run: &mut Run<'_>) -> Result<String, String> {
        let args = [
            ARG.into(),
            mode.into(),
            self.uid.to_string().into(),
            self.gid.to_string().into(),
            dir.as_os_str().to_owned(),
        ];
        run(self.program, &args)
    }
}

/// C6: the home's flags, ACLs and modes by `as_root` (it gets the home only, and says the
/// errors it had), the entry's as the uid. The entry is never given to `as_root`: root
/// cannot do it there, and the uid can.
///
/// # Errors
/// A part failed; every part is tried.
pub(crate) fn flags_step(
    me: &AsUid<'_>,
    (home, folders): (&Path, &Path),
    as_root: &mut dyn FnMut(&Path) -> Vec<String>,
    run: &mut Run<'_>,
) -> Result<(), String> {
    let mut failed = as_root(home);
    failed.extend(clear_flags(me, folders, run).err());
    join(failed)
}

/// C7: the home emptied by `as_root` (the home only), then the entry emptied as the uid.
///
/// # Errors
/// A part failed; both are tried.
pub(crate) fn contents_step(
    me: &AsUid<'_>,
    (home, folders): (&Path, &Path),
    as_root: &mut dyn FnMut(&Path) -> Vec<String>,
    run: &mut Run<'_>,
) -> Result<(), String> {
    let mut failed = as_root(home);
    failed.extend(empty(me, folders, run).err());
    join(failed)
}

fn join(failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// `find`'s arguments that remove the ACL of every entry of the uid below
/// `entry`, not the entry itself or its four folders (see below) (never through a link, on one filesystem). A directory is done where `find`
/// meets it, before `find` reads it, so an ACL that denies the listing does not stop the
/// walk; the other entries are done in batches.
pub(crate) fn acl_args(entry: &Path, uid: u32) -> Vec<OsString> {
    let words = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
    let mut args = words(&["-x"]);
    args.push(entry.as_os_str().to_owned());
    // The entry and its four folders keep the ACLs the system gave them; what is in
    // the folders is cleared.
    args.extend(words(&["!", "(", "-path"]));
    args.push(entry.as_os_str().to_owned());
    for folder in SKELETON {
        args.extend(words(&["-o", "-path"]));
        args.push(entry.join(folder).into_os_string());
    }
    args.extend(words(&[")"]));
    // One group holds both branches, so that `-user` applies to each.
    args.extend(words(&["-user", &uid.to_string(), "(", "("]));
    args.extend(words(&["-type", "d", "-exec", CHMOD, "-N", "{}", ";", ")"]));
    args.extend(words(&["-o", "(", "!", "-type", "l", "-exec", CHMOD, "-N"]));
    args.extend(words(&["{}", "+", ")", ")"]));
    args
}

/// C6 for the entry `folders`, all as the uid: the ACLs of the uid's entries removed, the
/// flags and modes below it cleared, and the ACLs removed again. Neither of the two can
/// do what the other lets through: the uid obeys an ACL that denies the listing of a
/// directory (root did not), so the flags below it are out of reach of the sweep until
/// the first removal, and a locked file cannot have its ACL changed until the sweep
/// unlocks it. The first removal is therefore allowed to fail; the sweep and the last
/// removal are not, and every failure of those is named with the entry.
///
/// # Errors
/// The sweep or the last removal failed.
pub(crate) fn clear_flags(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    let find = me.launch(OsStr::new(FIND), &acl_args(folders, me.uid));
    // Whatever this cannot reach is reached after the sweep, and judged then.
    let _ = run(me.program, &find);
    let mut failed = Vec::new();
    if let Err(why) = me.mode(CLEAR, folders, run) {
        failed.push(why);
    }
    if let Err(why) = run(me.program, &find) {
        failed.push(why);
    }
    named(folders, failed)
}

/// C10 for the entry `folders`: the four folders given back their mode as the uid, which
/// owns them. Nothing else is touched: not the entry, not the flags or ACLs the system
/// set.
///
/// # Errors
/// The call failed (a folder is not the uid's, or its mode could not be set).
pub(crate) fn restore(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    named(
        folders,
        me.mode(RESTORE, folders, run).err().into_iter().collect(),
    )
}

/// [`RESTORE`]: each of [`SKELETON`]'s folders in `dir` that exists has its extended
/// attributes taken off (the protected ones stay) and is given mode 0700 where its mode
/// differs, and nothing else; one the uid does not own is an error that
/// names it. Run as the uid. Exit 1 when a folder could not be put right.
fn restore_folders(
    dir: &Path,
    uid: u32,
    chmod: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> sweep::Swept {
    use std::os::unix::fs::MetadataExt as _;
    let mut swept = sweep::Swept::default();
    for name in SKELETON {
        let path = dir.join(name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => meta,
            Ok(_) => {
                swept
                    .errors
                    .push(format!("{}: not a directory", path.display()));
                continue;
            }
            // The system makes it again, as the user, when it is next asked for.
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => continue,
            Err(why) => {
                swept.errors.push(format!("{}: {why}", path.display()));
                continue;
            }
        };
        // What the lease put on the folder; what the system protects stays (it is
        // recorded as the folder's baseline, and the check finds any other that stays).
        if let Err(why) = strip_xattrs(&path) {
            swept.errors.push(format!("{}: {why}", path.display()));
        }
        let (wrong_owner, wrong_mode) = probe::folder_fix((meta.uid(), meta.mode()), (uid, 0o700));
        if wrong_owner {
            swept.errors.push(format!(
                "{}: owned by {}, not the pool user",
                path.display(),
                meta.uid()
            ));
        } else if wrong_mode && let Err(why) = chmod(&path) {
            swept.errors.push(format!("{}: {why}", path.display()));
        }
    }
    swept
}

/// Whether the error `errno` of taking an extended attribute off a folder or listing them
/// is one to leave: the attribute went since the listing, or the system protects it from
/// its owner (the entry's folders carry some).
fn xattr_left(errno: i32) -> bool {
    errno == libc::EPERM || errno == NO_ATTR
}

#[cfg(target_os = "macos")]
const NO_ATTR: i32 = libc::ENOATTR;
#[cfg(not(target_os = "macos"))]
const NO_ATTR: i32 = libc::ENODATA;

/// The extended attributes of the directory `path` (not through a link) taken off, the
/// ones the system protects excepted ([`xattr_left`]).
fn strip_xattrs(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    #[cfg(target_os = "macos")]
    let (list, remove) = (
        |buf: *mut libc::c_char, size| {
            // SAFETY: `c_path` is NUL-terminated; `buf` is null with size 0 or writable for `size`.
            unsafe { libc::listxattr(c_path.as_ptr(), buf, size, libc::XATTR_NOFOLLOW) }
        },
        |name: *const libc::c_char| {
            // SAFETY: both strings are NUL-terminated.
            unsafe { libc::removexattr(c_path.as_ptr(), name, libc::XATTR_NOFOLLOW) }
        },
    );
    #[cfg(not(target_os = "macos"))]
    let (list, remove) = (
        |buf: *mut libc::c_char, size| {
            // SAFETY: `c_path` is NUL-terminated; `buf` is null with size 0 or writable for `size`.
            unsafe { libc::llistxattr(c_path.as_ptr(), buf, size) }
        },
        |name: *const libc::c_char| {
            // SAFETY: both strings are NUL-terminated.
            unsafe { libc::lremovexattr(c_path.as_ptr(), name) }
        },
    );
    let leave = |why: std::io::Error| match why.raw_os_error() {
        Some(errno) if xattr_left(errno) => Ok(()),
        _ => Err(why),
    };
    let size = list(std::ptr::null_mut(), 0);
    let Ok(size) = usize::try_from(size) else {
        return leave(std::io::Error::last_os_error());
    };
    let mut names = vec![0_u8; size];
    let got = list(names.as_mut_ptr().cast(), size);
    let Ok(got) = usize::try_from(got) else {
        return leave(std::io::Error::last_os_error());
    };
    names.truncate(got);
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
        if remove(name.as_ptr()) != 0 {
            leave(std::io::Error::last_os_error())?;
        }
    }
    Ok(())
}

/// C7 for the entry `folders`: emptied as the uid.
///
/// # Errors
/// The call failed.
pub(crate) fn empty(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    named(
        folders,
        me.mode(EMPTY, folders, run).err().into_iter().collect(),
    )
}

/// What the uid finds left in the entry `folders`, one line each.
///
/// # Errors
/// The uid could not look.
pub(crate) fn left(
    me: &AsUid<'_>,
    folders: &Path,
    run: &mut Run<'_>,
) -> Result<Vec<String>, String> {
    let said = me
        .mode(LOOK, folders, run)
        .map_err(|why| format!("{}: {why}", folders.display()))?;
    Ok(said.lines().map(str::to_owned).collect())
}

fn named(folders: &Path, failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        return Ok(());
    }
    Err(format!("{}: {}", folders.display(), failed.join("; ")))
}

/// The entry mode for `args` (the program's own, argv0 first): its exit code when they
/// select it. It drops to the uid and gid it is given with `drop_to` (which checks the
/// ids it ends with), then runs the mode. Errors go to `err`, what [`LOOK`] finds to
/// `out`. Refuses root as the target: the mode exists to do what the pool user can.
/// Exits 2 for arguments it cannot read, 126 when the drop fails.
pub(crate) fn dispatch(
    args: &[OsString],
    drop_to: &mut dyn FnMut(u32, u32) -> Result<(), String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != ARG) {
        return None;
    }
    let [_, _, mode, uid, gid, dir] = args else {
        let _ = writeln!(err, "{USAGE}");
        return Some(2);
    };
    let id = |text: &OsString| text.to_str().and_then(|t| t.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (id(uid).filter(|n| *n != 0), id(gid).filter(|n| *n != 0)) else {
        let _ = writeln!(err, "{USAGE}: the ids are not non-root ids");
        return Some(2);
    };
    let mode = match mode.to_str() {
        Some(mode @ (CLEAR | EMPTY | LOOK | RESTORE)) => mode,
        _ => {
            let _ = writeln!(err, "{ARG}: no mode {mode:?}");
            return Some(2);
        }
    };
    if let Err(why) = drop_to(uid, gid) {
        let _ = writeln!(err, "{ARG}: {why}");
        return Some(126);
    }
    let dir = Path::new(dir);
    let swept = match mode {
        CLEAR => sweep::clear_flags_within(dir, uid, &SKELETON),
        RESTORE => restore_folders(dir, uid, &mut |path| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        }),
        EMPTY => sweep::empty_keeping(dir, uid, &SKELETON),
        _ => return Some(look(dir, out, err)),
    };
    for error in &swept.errors {
        let _ = writeln!(err, "{error}");
    }
    Some(i32::from(!swept.errors.is_empty()))
}

/// [`LOOK`]: a line for each name in `dir` that is not one of [`SKELETON`], and for
/// each of those that is not empty. Exit 1 when something could not be listed.
fn look(dir: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let names = |path: &Path| -> std::io::Result<Vec<String>> {
        // A link in place of a folder is not followed: it is reported, not listed.
        if !std::fs::symlink_metadata(path)?.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        let mut names = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        Ok(names)
    };
    let mut code = 0;
    let top = match names(dir) {
        Ok(top) => top,
        Err(why) => {
            let _ = writeln!(err, "{}: {why}", dir.display());
            return 1;
        }
    };
    for name in top {
        if !SKELETON.contains(&name.as_str()) {
            let _ = writeln!(out, "{}: {name}", dir.display());
            continue;
        }
        let folder = dir.join(&name);
        match names(&folder) {
            Ok(inside) if inside.is_empty() => {}
            Ok(inside) => {
                let _ = writeln!(out, "{}: {inside:?}", folder.display());
            }
            Err(why) => {
                let _ = writeln!(err, "{}: {why}", folder.display());
                code = 1;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests;
