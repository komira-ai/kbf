//! The updater's service account (`updater_user`): `_kbf-updater`, the account the macOS
//! kbf-updater agent's LaunchDaemon runs as, and the two directories that agent writes.
//!
//! - The account: user and group of one id (`updater_id`, 200 to 499; unset, `apply`
//!   takes the highest id below 500 that is free), shell `/usr/bin/false`, home
//!   `/var/empty`, hidden, no password. It is made as the role account is.
//! - [`STATE_DIR`] holds the agent's key, certificate and CA: the account's, mode 700.
//!   [`LOG_DIR`] holds its logs: the account's, mode 755.
//!
//! Unset `updater_user`, the item does not manage the account. `apply` changes nothing
//! while another user or group holds the id.

use std::io;

use crate::directory::{Batch, Directory, Record};
use crate::items::{Ctx, Lines, Outcome, fail, or, pass};
use crate::pool::dir_of;
use crate::profile::UPDATER_IDS;

/// The only name `updater_user` may have: the `UserName` of the agent's property list.
pub(crate) const NAME: &str = "_kbf-updater";
/// The agent's `--state-dir`.
pub(crate) const STATE_DIR: &str = "/var/db/kbf-updater";
/// The directory of the property list's `StandardOutPath` and `StandardErrorPath`.
pub(crate) const LOG_DIR: &str = "/Library/Logs/kbf-updater";
/// The directories, each with its mode and what `ls -ldn` shows of that mode.
const DIRS: [(&str, u32, &str); 2] = [
    (STATE_DIR, 0o700, "rwx------"),
    (LOG_DIR, 0o755, "rwxr-xr-x"),
];

/// What `ls -ldn` shows for a directory of `id` with the permissions `bits`.
fn dir_want(id: u32, bits: &str) -> String {
    format!("d{bits} {id} {id}")
}

/// The attributes the account must have, for id `id`.
fn attributes(id: u32) -> [(&'static str, String); 6] {
    [
        ("UniqueID", id.to_string()),
        ("PrimaryGroupID", id.to_string()),
        ("UserShell", "/usr/bin/false".to_owned()),
        ("NFSHomeDirectory", "/var/empty".to_owned()),
        ("IsHidden", "1".to_owned()),
        ("Password", "*".to_owned()),
    ]
}

/// The id the account has, or should have: `updater_id`, else the id of the account
/// that is there (when it is a usable one), else `None`.
fn id_of(c: &Ctx<'_>, record: Option<&Record>) -> Option<u32> {
    c.profile.updater_id.or_else(|| {
        record
            .and_then(|r| u32::try_from(r.number("UniqueID")?).ok())
            .filter(|id| usable(c, *id))
    })
}

/// Whether `id` is in the range and is neither the role account's nor the VM user's.
fn usable(c: &Ctx<'_>, id: u32) -> bool {
    UPDATER_IDS.contains(&id) && id != c.profile.role_id && Some(id) != c.profile.vm_user_uid
}

/// What is wrong with the account, its group and its directories.
fn problems(c: &Ctx<'_>, dir: &Directory) -> (Option<u32>, Vec<String>) {
    let record = dir.user(NAME);
    let mut wrong = Vec::new();
    let Some(record) = record else {
        return (c.profile.updater_id, vec![format!("no user {NAME}")]);
    };
    let Some(id) = id_of(c, Some(record)) else {
        let found = record.get("UniqueID");
        wrong.push(format!(
            "uid {} is not an id from 200 to 499 other than role_id and vm_user_uid",
            or(found, "absent")
        ));
        return (None, wrong);
    };
    for (attr, want) in attributes(id) {
        let found = record.get(attr);
        if found != want {
            wrong.push(format!("{attr}={}, want {want}", or(found, "absent")));
        }
    }
    if record.has("AuthenticationAuthority") {
        wrong.push("has a password (AuthenticationAuthority)".to_owned());
    }
    match dir.group(NAME) {
        None => wrong.push(format!("no group {NAME}")),
        Some(g) if g.number("PrimaryGroupID") != Some(id.into()) => wrong.push(format!(
            "group {NAME} has gid {}, want {id}",
            or(g.get("PrimaryGroupID"), "absent")
        )),
        Some(_) => {}
    }
    if let Some(other) = dir.uid_holder(id.into(), |name| name == NAME) {
        wrong.push(format!("uid {id} belongs to {other}"));
    }
    if let Some(other) = dir.gid_holder(id.into(), NAME) {
        wrong.push(format!("gid {id} belongs to {other}"));
    }
    for (path, mode, bits) in DIRS {
        let (found, want) = (c.mac.ls(path), dir_want(id, bits));
        if found != want {
            wrong.push(format!("{path} is {found}, want {want} (mode {mode:o})"));
        }
    }
    (Some(id), wrong)
}

/// `_kbf-updater` exists with user and group of one id, shell `/usr/bin/false`, home
/// `/var/empty`, hidden and with no password; no other user or group holds the id; and
/// [`STATE_DIR`] (mode 700) and [`LOG_DIR`] (mode 755) are the account's.
/// Unset `updater_user`: the item does not manage the account.
pub(crate) fn updater_user(c: &Ctx<'_>) -> Outcome {
    if c.value().is_empty() {
        return pass("not managed");
    }
    let dir = match Directory::read(c) {
        Ok(dir) => dir,
        Err(why) => return fail(why),
    };
    let (id, wrong) = problems(c, &dir);
    match id {
        Some(id) if wrong.is_empty() => pass(format!(
            "{NAME} (uid and gid {id}, hidden, no shell, no password): {STATE_DIR} mode 700, {LOG_DIR} mode 755"
        )),
        _ => fail(wrong.join("; ")),
    }
}

/// The id `apply` gives a new account when the profile fixes none: the highest id below
/// 500 that no user or group holds and that is not `role_id` or `vm_user_uid`.
fn new_id(c: &Ctx<'_>, dir: &Directory) -> Option<u32> {
    UPDATER_IDS.rev().find(|id| {
        usable(c, *id)
            && dir.uid_holder((*id).into(), |_| false).is_none()
            && dir.gid_holder((*id).into(), "").is_none()
    })
}

/// Makes the account, its group and the two directories what [`updater_user`] wants.
/// Nothing is changed while another user or group holds the id.
pub(crate) fn fix_updater_user(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let dir = Directory::read(c).map_err(io::Error::other)?;
    let id = match (id_of(c, dir.user(NAME)), dir.user(NAME)) {
        (Some(id), _) => id,
        (None, Some(record)) => {
            return Err(io::Error::other(format!(
                "{NAME} has uid {}, not an id from 200 to 499 other than role_id and vm_user_uid: not changed",
                record.get("UniqueID")
            )));
        }
        (None, None) => new_id(c, &dir)
            .ok_or_else(|| io::Error::other(format!("no free id from 200 to 499 for {NAME}")))?,
    };
    if let Some(other) = dir.uid_holder(id.into(), |name| name == NAME) {
        return Err(io::Error::other(format!(
            "uid {id} belongs to {other}: {NAME} not changed"
        )));
    }
    if let Some(other) = dir.gid_holder(id.into(), NAME) {
        return Err(io::Error::other(format!(
            "gid {id} belongs to {other}: {NAME} not changed"
        )));
    }
    let (user, group) = (format!("/Users/{NAME}"), format!("/Groups/{NAME}"));
    let mut batch = Batch::default();
    let group_ok = dir
        .group(NAME)
        .is_some_and(|g| g.number("PrimaryGroupID") == Some(id.into()));
    if !group_ok {
        if dir.group(NAME).is_none() {
            batch.create(&group);
        }
        batch.set(&group, "PrimaryGroupID", &id.to_string());
        batch.set(&group, "Password", "*");
    }
    let record = dir.user(NAME);
    if record.is_none() {
        batch.create(&user);
    }
    let wrong: Vec<_> = attributes(id)
        .into_iter()
        .filter(|(attr, want)| record.is_none_or(|r| r.get(attr) != want))
        .collect();
    for (attr, want) in &wrong {
        batch.set(&user, attr, want);
    }
    if record.is_some_and(|r| r.has("AuthenticationAuthority")) {
        batch.delete(&user, "AuthenticationAuthority");
    }
    if !batch.is_empty() {
        out.change(c.key, format!("account and group {NAME}, id {id}"));
    }
    if !batch.run(c) {
        return Err(io::Error::other(format!(
            "dscl refused a change to {NAME}; no directory made"
        )));
    }
    for (path, mode, bits) in DIRS {
        if c.mac.ls(path) != dir_want(id, bits) {
            dir_of(
                c,
                out,
                path,
                (id, id, mode),
                format!("{path}, {NAME}'s, mode {mode:o}"),
            )?;
        }
    }
    Ok(())
}
