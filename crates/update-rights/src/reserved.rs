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
//! - a `write_paths` entry must also be below one of [`WRITE_ROOTS`]: an allow-list, with the
//!   deny list catching the reserved names inside the roots (the package database, the apt cache);
//! - a `read_paths` entry may be inside a reserved directory: a single file such as the one
//!   declaration under `/etc/updater.d`, or a directory such as `/etc/service`, is read-only
//!   and harmless.

/// The directories no entry may name or contain.
pub const RESERVED: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/etc",
    "/etc/polkit-1",
    "/etc/sudoers.d",
    "/etc/systemd/system",
    "/etc/updater.d",
    "/lib",
    "/lib/systemd/system",
    "/lib64",
    "/root",
    "/run/systemd/generator",
    "/run/systemd/generator.early",
    "/run/systemd/generator.late",
    "/run/systemd/network",
    "/run/systemd/system",
    "/run/systemd/system.control",
    "/run/systemd/transient",
    "/run/tmpfiles.d",
    "/run/udev",
    "/run/updater",
    "/sbin",
    "/usr",
    "/usr/lib/systemd/system",
    "/usr/share/polkit-1",
    "/var/cache/apt",
    "/var/lib/apt",
    "/var/lib/cloud",
    "/var/lib/dpkg",
    "/var/lib/polkit-1",
    "/var/lib/sudo",
    "/var/lib/updater",
    "/var/spool/cron",
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
    #[error(
        "is not below a directory a hook may write in (one component under /var/lib, /var/cache, /run or /opt)"
    )]
    NotAllowed,
    #[error(
        "is not at or below a directory named for the service, which a hook that runs as root needs"
    )]
    NotTheServices,
}

/// The directories a hook may write below, one component down and anything under that: a
/// `write_paths` entry outside them is refused, whatever the deny list says.
pub const WRITE_ROOTS: [&str; 4] = ["/var/lib", "/var/cache", "/run", "/opt"];

/// Whether a hook that runs as root may write `path` for `service`: at or below a directory of a
/// write root whose name is the service's, or the service's followed by `-` and more
/// (`/var/lib/svc`, `/opt/svc/data`, `/var/lib/svc-update`).
pub fn check_service_write(path: &str, service: &str) -> Result<(), Violation> {
    let own = first_below_a_write_root(path).is_some_and(|first| belongs_to(first, service));
    if own {
        Ok(())
    } else {
        Err(Violation::NotTheServices)
    }
}

/// The first component of `path` under the write root it lies in: `svc` for `/var/lib/svc/x`.
pub fn first_below_a_write_root(path: &str) -> Option<&str> {
    WRITE_ROOTS.iter().find_map(|root| {
        let rest = path.strip_prefix(root)?.strip_prefix('/')?;
        rest.split('/').next()
    })
}

/// Whether the directory name `first` is `service`'s: the name, or the name followed by `-` and more.
pub fn belongs_to(first: &str, service: &str) -> bool {
    (first == service)
        | first
            .strip_prefix(service)
            .is_some_and(|more| more.starts_with('-'))
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
    let forbidden = WRITE_FORBIDDEN.iter().find(|dir| is_within(path, dir));
    match (inside, forbidden) {
        (Some(reserved), _) if writes && !in_hooks_root => Err(Violation::Inside(reserved)),
        (_, Some(dir)) if writes => Err(Violation::Inside(dir)),
        // A write path is an allow-list: one component under a write root, and anything below.
        _ if kind == Kind::Write
            && !WRITE_ROOTS
                .iter()
                .any(|root| (path != *root) & is_within(path, root)) =>
        {
            Err(Violation::NotAllowed)
        }
        _ => Ok(()),
    }
}
