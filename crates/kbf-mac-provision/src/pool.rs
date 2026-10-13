//! The lease-user pool and the volume it lives on (docs/design/macos-end-state.md,
//! step 2): users `kbf-pool-<i>` for `i < pool_users`, which a root kbf-daemon will
//! run actions as, one per slot; and the dedicated APFS volume (`kbf_volume`) that
//! holds their homes, the scratch root and the cache.
//!
//! Nothing runs as a pool user yet: kbf-daemon still runs as the role account (see
//! `daemon_user` in [`crate::launchd`]).

use std::io;

use crate::accounts::{DSCL, DSEDITGROUP, dscl_attr, has_authority, holder, is_member};
use crate::items::{Ctx, Lines, Outcome, fail, or, pass, run};

/// The pool's primary group.
const GROUP: &str = "kbfpool";
const SHELL: &str = "/usr/bin/false";
const DISKUTIL: &str = "/usr/sbin/diskutil";
const MDUTIL: &str = "/usr/bin/mdutil";
const TMUTIL: &str = "/usr/bin/tmutil";
/// A file at a volume's root that keeps Spotlight from indexing it.
const NEVER_INDEX: &str = ".metadata_never_index";
pub(crate) const ROOT_DIR: &str = "drwxr-xr-x 0 0";

/// The value of `name: value` in `diskutil info`'s output, or empty.
fn field<'a>(info: &'a str, name: &str) -> &'a str {
    info.lines()
        .find_map(|l| l.trim_start().strip_prefix(name)?.strip_prefix(':'))
        .map_or("", str::trim)
}

/// Why `kbf_volume` is not a mounted APFS volume, or `None` when it is: `diskutil
/// info` gives its own path as the mount point (a directory on another volume gives
/// that volume's, or none), and an APFS personality.
fn not_mounted(c: &Ctx<'_>) -> Option<String> {
    let volume = c.profile.volume.as_str();
    let info = c.mac.out(DISKUTIL, &["info", volume]);
    let (mount, personality) = (
        field(&info, "Mount Point"),
        field(&info, "File System Personality"),
    );
    if mount != volume {
        Some(format!(
            "{volume} is not a mounted volume ({})",
            or(info.trim(), "diskutil says nothing")
        ))
    } else if !personality.contains("APFS") {
        Some(format!(
            "{volume} is {}, not APFS",
            or(personality, "of no personality")
        ))
    } else {
        None
    }
}

/// Whether `kbf_volume` is a mounted APFS volume, so that a directory made under it is
/// on it (and not on the system volume, under the mount point).
pub(crate) fn volume_mounted(c: &Ctx<'_>) -> bool {
    not_mounted(c).is_none()
}

/// The error a fix returns when it would make `what` under an unmounted volume.
pub(crate) fn unmounted(c: &Ctx<'_>, what: &str) -> io::Error {
    io::Error::other(format!(
        "{} is not a mounted APFS volume: {what}",
        c.profile.volume
    ))
}

/// The marker file at the volume's root.
fn marker(c: &Ctx<'_>) -> String {
    format!("{}/{NEVER_INDEX}", c.profile.volume)
}

/// The marker file is not a regular file.
fn no_marker(c: &Ctx<'_>) -> bool {
    c.mac.lstat(&marker(c)).is_none_or(|s| s.kind != '-')
}

/// `mdutil -s` does not say indexing is disabled.
fn indexed(c: &Ctx<'_>) -> bool {
    !c.mac
        .out(MDUTIL, &["-s", &c.profile.volume])
        .contains("Indexing disabled.")
}

/// `tmutil isexcluded` does not say the volume is excluded.
fn backed_up(c: &Ctx<'_>) -> bool {
    !c.mac
        .out(TMUTIL, &["isexcluded", &c.profile.volume])
        .starts_with("[Excluded]")
}

/// The volume is a mounted APFS volume, Spotlight does not index it (indexing off, and
/// the marker file at its root) and Time Machine excludes it.
pub(crate) fn kbf_volume(c: &Ctx<'_>) -> Outcome {
    if let Some(why) = not_mounted(c) {
        return fail(format!(
            "{why}: make it with diskutil apfs addVolume (apply does not make volumes)"
        ));
    }
    let volume = c.profile.volume.as_str();
    let mut wrong = Vec::new();
    if no_marker(c) {
        wrong.push(format!("no {}", marker(c)));
    }
    if indexed(c) {
        wrong.push("Spotlight indexing is on".to_owned());
    }
    if backed_up(c) {
        wrong.push("not excluded from Time Machine".to_owned());
    }
    if wrong.is_empty() {
        pass(format!(
            "{volume}: APFS, Spotlight off, excluded from Time Machine"
        ))
    } else {
        fail(format!("{volume}: {}", wrong.join(", ")))
    }
}

/// Writes the marker file, turns indexing off and excludes the volume from Time
/// Machine, each only if it is not so. Makes no volume.
pub(crate) fn fix_kbf_volume(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    if !volume_mounted(c) {
        return Err(unmounted(
            c,
            "make it with diskutil apfs addVolume (apply does not make volumes)",
        ));
    }
    let volume = c.profile.volume.as_str();
    if no_marker(c) {
        let marker = marker(c);
        c.mac.new_file(&marker, 0, 0, 0o644)?;
        out.change(c.key, format!("wrote {marker}"));
    }
    if indexed(c) {
        run(c, MDUTIL, &["-i", "off", volume]);
        out.change(c.key, "Spotlight indexing off (mdutil -i off)");
    }
    if backed_up(c) {
        run(c, TMUTIL, &["addexclusion", volume]);
        out.change(c.key, "excluded from Time Machine (tmutil addexclusion)");
    }
    Ok(())
}

/// The directory that holds the pool's homes.
fn homes(c: &Ctx<'_>) -> String {
    format!("{}/home", c.profile.volume)
}

/// Pool user `i`: its name, uid and home.
struct PoolUser {
    name: String,
    uid: u32,
    home: String,
}

fn pool_user(c: &Ctx<'_>, i: u32) -> PoolUser {
    let name = format!("kbf-pool-{i}");
    PoolUser {
        home: format!("{}/{name}", homes(c)),
        uid: c.profile.pool_first_uid + i,
        name,
    }
}

/// The pool's users, in order.
fn pool_users(c: &Ctx<'_>) -> Vec<PoolUser> {
    (0..c.profile.pool_users).map(|i| pool_user(c, i)).collect()
}

/// The directory attributes a pool user must have.
fn attributes(c: &Ctx<'_>, u: &PoolUser) -> [(&'static str, String); 6] {
    [
        ("UniqueID", u.uid.to_string()),
        ("PrimaryGroupID", c.profile.pool_first_uid.to_string()),
        ("UserShell", SHELL.to_owned()),
        ("NFSHomeDirectory", u.home.clone()),
        ("IsHidden", "1".to_owned()),
        ("Password", "*".to_owned()),
    ]
}

/// The attributes of `u` that differ: `(name, found, want)`.
fn wrong_attributes(c: &Ctx<'_>, u: &PoolUser) -> Vec<(&'static str, String, String)> {
    let record = format!("/Users/{}", u.name);
    attributes(c, u)
        .into_iter()
        .filter_map(|(attr, want)| {
            let found = dscl_attr(c, &record, attr);
            (found != want).then_some((attr, found, want))
        })
        .collect()
}

/// What `ls -ldn` must show for the home of `u`.
fn home_want(c: &Ctx<'_>, u: &PoolUser) -> String {
    format!("drwx------ {} {}", u.uid, c.profile.pool_first_uid)
}

/// What is wrong with pool user `u`, each prefixed with its name.
fn user_problems(c: &Ctx<'_>, u: &PoolUser) -> Vec<String> {
    let mut wrong: Vec<String> = wrong_attributes(c, u)
        .into_iter()
        .map(|(attr, found, want)| format!("{attr}={}, want {want}", or(&found, "absent")))
        .collect();
    let record = format!("/Users/{}", u.name);
    if has_authority(c, &record) {
        wrong.push("an AuthenticationAuthority".to_owned());
    }
    if is_member(c, &u.name, "admin") {
        wrong.push("an administrator".to_owned());
    }
    if is_member(c, &u.name, "wheel") {
        wrong.push("in wheel".to_owned());
    }
    if let Some(other) = holder(c, "/Users", "UniqueID", u.uid.into(), &u.name) {
        wrong.push(format!("uid {} belongs to {other}", u.uid));
    }
    let (found, want) = (c.mac.ls(&u.home), home_want(c, u));
    if found != want {
        wrong.push(format!("home {} is {found}, want {want}", u.home));
    }
    wrong
        .into_iter()
        .map(|w| format!("{}: {w}", u.name))
        .collect()
}

/// The group's id and its holder, the homes directory and every pool user.
fn problems(c: &Ctx<'_>) -> Vec<String> {
    let gid = c.profile.pool_first_uid;
    let mut wrong = Vec::new();
    let found = dscl_attr(c, &format!("/Groups/{GROUP}"), "PrimaryGroupID");
    if found != gid.to_string() {
        wrong.push(format!(
            "group {GROUP}: PrimaryGroupID={}, want {gid}",
            or(&found, "absent")
        ));
    }
    if let Some(other) = holder(c, "/Groups", "PrimaryGroupID", gid.into(), GROUP) {
        wrong.push(format!("gid {gid} belongs to {other}"));
    }
    let homes = homes(c);
    let found = c.mac.ls(&homes);
    if found != ROOT_DIR {
        wrong.push(format!("{homes} is {found}, want {ROOT_DIR}"));
    }
    for u in pool_users(c) {
        wrong.extend(user_problems(c, &u));
    }
    wrong
}

/// Every pool user exists with its fixed uid, primary group `kbfpool` (gid
/// `pool_first_uid`), shell `/usr/bin/false`, its home on the volume, hidden, and no
/// password; none is an administrator or in `wheel`; no other user holds its uid and
/// no other group the gid; each home is the user's, mode 700, in a directory of root's.
pub(crate) fn pool_users_item(c: &Ctx<'_>) -> Outcome {
    let wrong = problems(c);
    if !wrong.is_empty() {
        return fail(wrong.join("; "));
    }
    let p = c.profile;
    let last = p.pool_users - 1;
    pass(format!(
        "{} users kbf-pool-0..kbf-pool-{last} (uids {}-{}, group {GROUP} {}): hidden, standard, shell {SHELL}, no password, homes in {}",
        p.pool_users,
        p.pool_first_uid,
        p.pool_first_uid + last,
        p.pool_first_uid,
        homes(c)
    ))
}

/// Makes the group and each pool user what [`pool_users_item`] wants: sets the
/// attributes that differ, removes a password hash and the memberships of `admin` and
/// `wheel`, and makes the homes, only on the mounted volume. A user whose uid another
/// user holds is left; nothing is changed while another group holds the gid. What is at
/// a home's path and is not a directory is left.
pub(crate) fn fix_pool_users(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let gid = c.profile.pool_first_uid;
    if let Some(other) = holder(c, "/Groups", "PrimaryGroupID", gid.into(), GROUP) {
        return Err(io::Error::other(format!(
            "gid {gid} belongs to {other}: no pool user changed"
        )));
    }
    let group = format!("/Groups/{GROUP}");
    if dscl_attr(c, &group, "PrimaryGroupID") != gid.to_string() {
        run(c, DSCL, &[".", "-create", &group]);
        run(
            c,
            DSCL,
            &[".", "-create", &group, "PrimaryGroupID", &gid.to_string()],
        );
        run(c, DSCL, &[".", "-create", &group, "Password", "*"]);
        out.change(c.key, format!("group {GROUP}, gid {gid}"));
    }
    let mounted = volume_mounted(c);
    let homes = homes(c);
    if mounted && c.mac.ls(&homes) != ROOT_DIR {
        dir_of(
            c,
            out,
            &homes,
            (0, 0, 0o755),
            format!("made {homes}, root's, mode 755"),
        )?;
    }
    for u in pool_users(c) {
        fix_user(c, out, &u, mounted)?;
    }
    if mounted {
        Ok(())
    } else {
        Err(unmounted(c, "no home made"))
    }
}

/// Makes `path` a directory of `(uid, gid, mode)`, saying `what`; leaves what is there
/// when it is not a directory (a link is never followed).
pub(crate) fn dir_of(
    c: &Ctx<'_>,
    out: &mut Lines,
    path: &str,
    (uid, gid, mode): (u32, u32, u32),
    what: String,
) -> io::Result<()> {
    if c.mac.lstat(path).is_some_and(|s| s.kind != 'd') {
        out.note(c.key, format!("{path} is not a directory; apply leaves it"));
        return Ok(());
    }
    c.mac.dir(path, uid, gid, mode)?;
    out.change(c.key, what);
    Ok(())
}

fn fix_user(c: &Ctx<'_>, out: &mut Lines, u: &PoolUser, mounted: bool) -> io::Result<()> {
    if let Some(other) = holder(c, "/Users", "UniqueID", u.uid.into(), &u.name) {
        out.note(
            c.key,
            format!("uid {} belongs to {other}: {} left", u.uid, u.name),
        );
        return Ok(());
    }
    let record = format!("/Users/{}", u.name);
    let wrong = wrong_attributes(c, u);
    if !wrong.is_empty() {
        run(c, DSCL, &[".", "-create", &record]);
        for (attr, _, want) in &wrong {
            run(c, DSCL, &[".", "-create", &record, attr, want]);
        }
        out.change(c.key, format!("user {}, uid {}", u.name, u.uid));
    }
    if has_authority(c, &record) {
        run(
            c,
            DSCL,
            &[".", "-delete", &record, "AuthenticationAuthority"],
        );
        out.change(
            c.key,
            format!("{}: AuthenticationAuthority removed", u.name),
        );
    }
    for group in ["admin", "wheel"] {
        if is_member(c, &u.name, group) {
            run(
                c,
                DSEDITGROUP,
                &["-o", "edit", "-d", &u.name, "-t", "user", group],
            );
            out.change(c.key, format!("{} removed from {group}", u.name));
        }
    }
    if mounted && c.mac.ls(&u.home) != home_want(c, u) {
        let gid = c.profile.pool_first_uid;
        dir_of(
            c,
            out,
            &u.home,
            (u.uid, gid, 0o700),
            format!("home {}, mode 700", u.home),
        )?;
    }
    Ok(())
}
