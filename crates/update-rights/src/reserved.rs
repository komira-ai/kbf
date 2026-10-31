//! The reserved-path deny list.
//!
//! A hook gets the paths its declaration names, read-only or writable. A declaration that could
//! name the places that decide who runs what as root (the system directories, sudo, polkit, the
//! unit directories, the updater's own) would grant itself everything, so these paths are
//! refused whatever the approval.
//!
//! The rules, all on the path as written and again on its target when it exists:
//!
//! - no entry of any kind **names** a reserved directory or **contains** one (is an ancestor of
//!   one): `/etc` names, `/` and `/var/lib` contain;
//! - a `write_paths` or `state_dir` entry is not **inside** a reserved directory either, except
//!   under [`HOOKS_ROOT`], where the updater keeps the record directories of the hooks;
//! - a `read_paths` entry may be inside a reserved directory: a single file such as the one
//!   declaration under `/etc/updater.d`, or a directory such as `/etc/tierstore`, is read-only
//!   and harmless.

/// The directories no entry may name or contain.
pub const RESERVED: &[&str] = &[
    "/",
    "/boot",
    "/etc",
    "/etc/polkit-1",
    "/etc/sudoers.d",
    "/etc/systemd/system",
    "/etc/updater.d",
    "/lib/systemd/system",
    "/root",
    "/run/systemd/system",
    "/run/updater",
    "/usr",
    "/usr/lib/systemd/system",
    "/usr/share/polkit-1",
    "/var/lib/updater",
];

/// The trees where nothing is written: a declaration may read them (a hook reads `/proc`) and
/// may not name them, or anything inside, in `write_paths` or as `state_dir`.
pub const WRITE_FORBIDDEN: [&str; 3] = ["/dev", "/proc", "/sys"];

/// The one place inside the reserved directories where a hook may write: the record directories
/// of the hooks (`state_dir`).
pub const HOOKS_ROOT: &str = "/var/lib/updater/hooks";

/// What an entry is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    State,
}

/// Why an entry was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    #[error("names the reserved directory {0}")]
    Names(&'static str),
    #[error("contains the reserved directory {0}")]
    Contains(&'static str),
    #[error("is inside the reserved directory {0}, where nothing may be written")]
    Inside(&'static str),
}

/// Whether `path` is `dir` or lies below it. Both are absolute and in the path grammar.
pub fn is_within(path: &str, dir: &str) -> bool {
    let below = dir.trim_end_matches('/');
    (path == dir)
        | path
            .strip_prefix(below)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Checks one entry against the deny list. `path` is in the path grammar (`check_path`).
pub fn check(kind: Kind, path: &str) -> Result<(), Violation> {
    if let Some(reserved) = RESERVED.iter().find(|r| **r == path) {
        return Err(Violation::Names(reserved));
    }
    if let Some(reserved) = RESERVED.iter().find(|r| is_within(r, path)) {
        return Err(Violation::Contains(reserved));
    }
    let writes = kind != Kind::Read;
    let in_hooks_root = path != HOOKS_ROOT && is_within(path, HOOKS_ROOT);
    // `/` is every path's ancestor, which the rule on the inside is not about.
    let inside = RESERVED.iter().skip(1).find(|r| is_within(path, r));
    match inside {
        Some(reserved) if writes && !in_hooks_root => Err(Violation::Inside(reserved)),
        _ => Ok(()),
    }
}
