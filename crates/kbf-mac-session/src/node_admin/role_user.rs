//! `role-user status|create`: the `_kbf` account the node's daemon and its farm
//! actions run as.
//!
//! What `create` makes, and `status` checks:
//!
//! | what | value |
//! |---|---|
//! | group `_kbf` | `PrimaryGroupID` 470, `RealName` "kbf build node", `Password` `*` |
//! | user `_kbf` | `UniqueID` 470, `PrimaryGroupID` 470, `UserShell` `/usr/bin/false`, `NFSHomeDirectory` `/usr/local/kbf/home`, `Password` `*`, `IsHidden` 1, `RealName` "kbf build node" |
//! | `/usr/local/kbf/home` | a directory, `_kbf:_kbf` 0750 |
//! | `/usr/local/kbf/home/leases` | a directory, `_kbf:_kbf` 0700 |
//! | `/private/var/at/cron.deny`, `at.deny` | each has the line `_kbf` (root:wheel 0644) |
//!
//! `_kbf` is not a member of `admin`, and no other account holds uid or gid 470 (kbf
//! keeps 470 to 479 on its Macs; 470 is in Apple's system range, so a later macOS
//! could hand it to an account of its own). `status` lists each way the Mac differs
//! from the table and exits 0 either way.
//!
//! `create` refuses, before it changes anything, when another account holds uid or
//! gid 470, `_kbf` exists with another uid or gid, or `_kbf` is in `admin`; a
//! `dscl` read that fails for any reason but a missing record or attribute fails the
//! call before any change. It makes
//! the group and the user only when they are missing, then sets `PrimaryGroupID`,
//! `UserShell` and `NFSHomeDirectory` wherever they differ. A missing home is made
//! root's (0700) first, gets its `leases`, and is given to `_kbf` last, so nothing
//! can be planted in it before the `chown`. A root-owned home is one an interrupted
//! `create` left (only root can make one in root's `/usr/local/kbf`, and the daemon
//! never ran in it): it is finished the same way. A home of `_kbf`'s is left alone; a
//! home anyone else owns, a link, or anything but a directory is refused. Running
//! `create` again changes nothing. A refusal over the home or the `cron` and `at`
//! files comes after the account is made; `create` run again after the fix finishes.
//!
//! `cron` and `at` each read `<name>.allow` alone when it exists: `create` refuses
//! one that lists `_kbf`, and otherwise leaves the files alone. Without one,
//! `<name>.deny` must exist (with neither, who may use the tool is not clear, and
//! `create` refuses rather than guess); `create` appends `_kbf` to it through a new
//! file in the same folder, given root:wheel 0644 and renamed over it. Each file
//! `create` reads there must be root's; before it writes there, `/private` and
//! `/private/var` must be root's alone, and `/private/var/at` root's or daemon's (uid 1, as macOS ships it), not
//! writable by group or others, with no access control list.
//!
//! Programs run by absolute path: `/usr/bin/dscl .` for the directory service (each
//! change prints `+ <command>` first) and `/usr/sbin/dseditgroup` for the `admin`
//! check.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::{
    AdminHost, Kind, Meta, Paths, Stop, at, change, meta, query, root_dir, root_file, say,
    write_new_as,
};

const DSCL: &str = "/usr/bin/dscl";
const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";

/// The role user's and its group's name.
pub const ROLE: &str = "_kbf";

/// The role user's uid and its group's gid.
pub const ROLE_ID: u32 = 470;

/// [`ROLE_ID`] as the directory service writes it.
const ID: &str = "470";

const SHELL: &str = "/usr/bin/false";

/// The role user's home on the Mac (below [`AdminHost::root`] in tests).
pub const HOME: &str = "/usr/local/kbf/home";

const REAL_NAME: &str = "kbf build node";

/// The uid of `daemon`, which owns `/private/var/at` on macOS.
const DAEMON_UID: u32 = 1;

/// Group and other write bits.
const GROUP_OTHER_WRITE: u32 = 0o022;

const USERS: &str = "/Users";
const GROUPS: &str = "/Groups";

fn user_record() -> String {
    format!("{USERS}/{ROLE}")
}

fn group_record() -> String {
    format!("{GROUPS}/{ROLE}")
}

fn home(paths: &Paths) -> PathBuf {
    paths.root.join(&HOME[1..])
}

/// `/private/var/at`, where `cron` and `at` keep their allow and deny files.
pub(crate) fn at_dir(paths: &Paths) -> PathBuf {
    paths.root.join("private/var/at")
}

/// What `dscl` prints on stderr when the record is not there.
const RECORD_NOT_FOUND: &str = "DS Error: -14136 ";

/// What `dscl -read` prints on stderr when the record has no such attribute.
const NO_SUCH_KEY: &str = "No such key: ";

/// The value `dscl . -read <record> <attribute>` prints, `None` when the record or the
/// attribute is not there. A value with a space in it is printed on the next line.
///
/// # Errors
/// Failed when `dscl` fails for any other reason: an unreadable record is not a
/// missing one (`create` would make `_kbf` again over one with another uid).
fn attribute(host: &dyn AdminHost, record: &str, name: &str) -> Result<Option<String>, Stop> {
    let output = query(host, DSCL, &[".", "-read", record, name])?;
    if !output.success {
        let stderr = output.stderr.trim();
        if stderr.contains(RECORD_NOT_FOUND) || stderr.starts_with(NO_SUCH_KEY) {
            return Ok(None);
        }
        return Err(Stop::Failed(format!(
            "dscl . -read {record} {name} failed: {stderr}"
        )));
    }
    let prefix = format!("{name}:");
    let mut lines = output
        .stdout
        .lines()
        .skip_while(|line| !line.starts_with(&prefix));
    let first = lines.next().and_then(|line| line.strip_prefix(&prefix));
    let value = match first.map(str::trim) {
        Some("") => lines.next().map(str::trim),
        first => first,
    };
    Ok(value.filter(|value| !value.is_empty()).map(str::to_owned))
}

/// The records under `dir` (`/Users` or `/Groups`) whose `name` is [`ID`], other than
/// [`ROLE`].
fn others_holding(host: &dyn AdminHost, dir: &str, name: &str) -> Result<Vec<String>, Stop> {
    let output = query(host, DSCL, &[".", "-list", dir, name])?;
    if !output.success {
        return Err(Stop::Failed(format!(
            "dscl . -list {dir} {name} failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let record = words.next().unwrap_or_default();
            (record != ROLE && words.any(|value| value == ID)).then(|| record.to_owned())
        })
        .collect())
}

fn in_admin(host: &dyn AdminHost) -> Result<bool, Stop> {
    let output = query(
        host,
        DSEDITGROUP,
        &["-o", "checkmember", "-m", ROLE, "admin"],
    )?;
    Ok(output.success)
}

/// Whether one line of `bytes` is [`ROLE`].
fn lists_role(bytes: &[u8]) -> bool {
    bytes
        .split(|&byte| byte == b'\n')
        .any(|line| line == ROLE.as_bytes())
}

/// Whether `_kbf` may not use `tool` (`cron` or `at`): `tool.allow` decides alone when
/// it exists. A file that cannot be read denies nothing.
fn denied(host: &dyn AdminHost, paths: &Paths, tool: &str) -> Result<bool, Stop> {
    let dir = at_dir(paths);
    let allow = dir.join(format!("{tool}.allow"));
    if meta(host, &allow)?.is_some() {
        return Ok(fs::read(&allow).is_ok_and(|bytes| !lists_role(&bytes)));
    }
    let deny = dir.join(format!("{tool}.deny"));
    Ok(fs::read(deny).is_ok_and(|bytes| lists_role(&bytes)))
}

fn shown(value: Option<&str>) -> &str {
    value.unwrap_or("absent")
}

/// What is at a path, for a refusal: its [`Kind`], or missing.
fn kind_of(found: Option<Meta>) -> String {
    found.map_or_else(|| "missing".to_owned(), |found| found.kind.to_string())
}

/// One line per way the role user differs from the table in the module's
/// documentation.
fn problems(host: &dyn AdminHost, paths: &Paths) -> Result<Vec<String>, Stop> {
    let mut problems = Vec::new();
    for (dir, name, what) in [
        (USERS, "UniqueID", "uid"),
        (GROUPS, "PrimaryGroupID", "gid"),
    ] {
        let others = others_holding(host, dir, name)?;
        if !others.is_empty() {
            problems.push(format!("{what} {ID} is also held by: {}", others.join(" ")));
        }
    }
    let wants = [
        ("group", group_record(), "PrimaryGroupID", ID),
        ("user", user_record(), "UniqueID", ID),
        ("user", user_record(), "PrimaryGroupID", ID),
        ("user", user_record(), "UserShell", SHELL),
        ("user", user_record(), "NFSHomeDirectory", HOME),
    ];
    for (what, record, name, want) in wants {
        let value = attribute(host, &record, name)?;
        if value.as_deref() != Some(want) {
            problems.push(format!(
                "{what} {ROLE}: {name} is '{}', want {want}",
                shown(value.as_deref())
            ));
        }
    }
    if in_admin(host)? {
        problems.push(format!("user {ROLE} is a member of admin"));
    }
    let home = home(paths);
    match meta(host, &home)? {
        Some(found) if found.kind == Kind::Directory && found.uid == ROLE_ID => {}
        found => problems.push(format!(
            "home {}: {}; want a directory owned by {ROLE}",
            home.display(),
            found.map_or_else(
                || "missing".to_owned(),
                |found| format!("{}, owned by uid {}", found.kind, found.uid)
            )
        )),
    }
    let dir = at_dir(paths);
    for (tool, program) in [("cron", "crontab"), ("at", "at")] {
        if !denied(host, paths, tool)? {
            problems.push(format!(
                "user {ROLE} may use {program} (not in {})",
                dir.join(format!("{tool}.deny")).display()
            ));
        }
    }
    Ok(problems)
}

/// `role-user status`.
///
/// # Errors
/// The directory service or a file could not be read.
pub(crate) fn status(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let problems = problems(host, paths)?;
    if problems.is_empty() {
        say(
            out,
            &format!(
                "role user {ROLE}: in place (uid {ID}, gid {ID}, shell {SHELL}, home {HOME}, \
                 not admin, no cron or at)"
            ),
        );
    } else {
        say(out, &format!("role user {ROLE}: NOT in place"));
        for problem in problems {
            say(out, &format!("  {problem}"));
        }
    }
    Ok(())
}

/// `dscl . -create <record> [<name> <value>]`.
fn create_record(
    host: &dyn AdminHost,
    out: &mut dyn Write,
    record: &str,
    pair: Option<(&str, &str)>,
) -> Result<(), Stop> {
    match pair {
        None => change(host, out, DSCL, &[".", "-create", record]),
        Some((name, value)) => change(host, out, DSCL, &[".", "-create", record, name, value]),
    }
}

/// `role-user create`.
///
/// # Errors
/// Refused (see the module's documentation) or a step failed.
pub(crate) fn create(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let (user, group) = (user_record(), group_record());
    for (dir, name, what) in [
        (USERS, "UniqueID", "uid"),
        (GROUPS, "PrimaryGroupID", "gid"),
    ] {
        let others = others_holding(host, dir, name)?;
        if !others.is_empty() {
            return Err(Stop::Refused(format!(
                "{what} {ID} belongs to {}; not taking it",
                others.join(", ")
            )));
        }
    }
    let uid = attribute(host, &user, "UniqueID")?;
    let gid = attribute(host, &group, "PrimaryGroupID")?;
    for (found, what, kind) in [(&uid, "user", "uid"), (&gid, "group", "gid")] {
        if let Some(found) = found
            && found != ID
        {
            return Err(Stop::Refused(format!(
                "{what} {ROLE} exists with {kind} {found}, not {ID}; remove it by hand first"
            )));
        }
    }
    if in_admin(host)? {
        return Err(Stop::Refused(format!(
            "{ROLE} is a member of admin; remove it by hand first"
        )));
    }
    if gid.is_none() {
        create_record(host, out, &group, None)?;
        for pair in [
            ("PrimaryGroupID", ID),
            ("RealName", REAL_NAME),
            ("Password", "*"),
        ] {
            create_record(host, out, &group, Some(pair))?;
        }
    }
    if uid.is_none() {
        create_record(host, out, &user, None)?;
        for pair in [
            ("UniqueID", ID),
            ("Password", "*"),
            ("IsHidden", "1"),
            ("RealName", REAL_NAME),
        ] {
            create_record(host, out, &user, Some(pair))?;
        }
    }
    // The record is ours (it has our id): these fields are the table's on every run.
    for (name, want) in [
        ("PrimaryGroupID", ID),
        ("UserShell", SHELL),
        ("NFSHomeDirectory", HOME),
    ] {
        if attribute(host, &user, name)?.as_deref() != Some(want) {
            create_record(host, out, &user, Some((name, want)))?;
        }
    }
    make_home(host, paths, out)?;
    deny(host, paths, out, "cron")?;
    deny(host, paths, out, "at")?;
    status(host, paths, out)
}

fn make_home(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let home = home(paths);
    match meta(host, &home)? {
        None => {
            // Root's first, so nothing can be planted in it before the chown.
            mkdir(out, &home)?;
            finish_home(host, &home, out)
        }
        Some(found) if found.kind != Kind::Directory => Err(Stop::Refused(format!(
            "{} exists and is not a directory (it is {})",
            home.display(),
            found.kind
        ))),
        Some(found) if found.uid == 0 => {
            say(
                out,
                &format!(
                    "home {}: root-owned, left by an interrupted role-user create; finishing it",
                    home.display()
                ),
            );
            finish_home(host, &home, out)
        }
        Some(found) if found.uid == ROLE_ID => {
            say(
                out,
                &format!(
                    "home {}: in place (its contents are {ROLE}'s; this helper never writes in it)",
                    home.display()
                ),
            );
            Ok(())
        }
        Some(found) => Err(Stop::Refused(format!(
            "{} exists, owned by uid {}, not {ROLE}; move it aside by hand",
            home.display(),
            found.uid
        ))),
    }
}

/// The root-owned `home` gets its `leases`, then becomes `_kbf`'s (the chown last).
fn finish_home(host: &dyn AdminHost, home: &Path, out: &mut dyn Write) -> Result<(), Stop> {
    let leases = home.join("leases");
    if meta(host, &leases)?.is_none() {
        mkdir(out, &leases)?;
    }
    match meta(host, &leases)? {
        Some(found) if found.kind == Kind::Directory => {}
        found => {
            return Err(Stop::Refused(format!(
                "{} is not a directory (it is {})",
                leases.display(),
                kind_of(found)
            )));
        }
    }
    chown(host, out, &leases)?;
    chmod(out, &leases, 0o700)?;
    chmod(out, home, 0o750)?;
    chown(host, out, home)
}

fn mkdir(out: &mut dyn Write, path: &Path) -> Result<(), Stop> {
    say(out, &format!("+ mkdir -m 0700 {}", path.display()));
    at(path, fs::DirBuilder::new().mode(0o700).create(path))
}

fn chown(host: &dyn AdminHost, out: &mut dyn Write, path: &Path) -> Result<(), Stop> {
    say(out, &format!("+ chown {ROLE}:{ROLE} {}", path.display()));
    at(path, host.chown(path, ROLE_ID, ROLE_ID))
}

fn chmod(out: &mut dyn Write, path: &Path, mode: u32) -> Result<(), Stop> {
    say(out, &format!("+ chmod {mode:04o} {}", path.display()));
    at(
        path,
        fs::set_permissions(path, fs::Permissions::from_mode(mode)),
    )
}

/// Puts `_kbf` in `tool.deny` (`cron` or `at`), so it can never schedule a job that
/// outlives the daemon.
fn deny(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write, tool: &str) -> Result<(), Stop> {
    let dir = at_dir(paths);
    let allow = dir.join(format!("{tool}.allow"));
    let deny = dir.join(format!("{tool}.deny"));
    if meta(host, &allow)?.is_some() {
        root_file(host, &allow)?;
        if lists_role(&at(&allow, fs::read(&allow))?) {
            return Err(Stop::Refused(format!(
                "{} lists {ROLE}; remove that line by hand",
                allow.display()
            )));
        }
        return Ok(());
    }
    if meta(host, &deny)?.is_none() {
        return Err(Stop::Refused(format!(
            "neither {} nor {} exists, so it is not clear who may use {tool}; not guessing",
            allow.display(),
            deny.display()
        )));
    }
    root_file(host, &deny)?;
    let mut text = at(&deny, fs::read(&deny))?;
    if lists_role(&text) {
        return Ok(());
    }
    check_at_dir(host, paths)?;
    if text.last().is_some_and(|&last| last != b'\n') {
        text.push(b'\n');
    }
    text.extend_from_slice(format!("{ROLE}\n").as_bytes());
    say(out, &format!("+ add the line {ROLE} to {}", deny.display()));
    write_new_as(host, &dir, &deny, &text, (0, 0), 0o644)
}

/// `/private/var/at` and the folders above it: `/private` and `/private/var` root's
/// alone; `at` root's or daemon's, writable by its owner alone.
///
/// # Errors
/// Refused when one is not.
pub(crate) fn check_at_dir(host: &dyn AdminHost, paths: &Paths) -> Result<(), Stop> {
    for up in ["private", "private/var"] {
        root_dir(host, &paths.root.join(up))?;
    }
    let dir = at_dir(paths);
    let refused = |why: String| Err(Stop::Refused(format!("{} {why}", dir.display())));
    let found = match meta(host, &dir)? {
        Some(found) if found.kind == Kind::Directory => found,
        found => return refused(format!("is not a directory (it is {})", kind_of(found))),
    };
    if found.uid != 0 && found.uid != DAEMON_UID {
        return refused(format!("is owned by uid {}, not root or daemon", found.uid));
    }
    if found.mode & GROUP_OTHER_WRITE != 0 {
        return refused(format!(
            "is writable by group or others (mode {:o})",
            found.mode
        ));
    }
    if at(&dir, host.has_acl(&dir))? {
        return refused(
            "has an access control list; its mode bits do not say who can write it".to_owned(),
        );
    }
    Ok(())
}
