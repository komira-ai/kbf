//! The standing VM user (`vm_user_uid`): the account `kbf-vm` that a Mac serving macOS
//! guest VMs rests logged in as. A guest can only be installed and started from a user's
//! GUI session, and the only unattended way to have one is automatic login, so the Mac
//! has one hidden, non-administrator account that logs in at boot.
//!
//! - The account: uid `vm_user_uid` (450 to 499), primary group `staff`, shell
//!   `/bin/zsh`, home `kbf-vm` under `/Users` (mode 700, its own), hidden, with a password; in
//!   no group that gives it administrator rights, Remote Login or Screen Sharing.
//! - The password is made here: 32 random bytes in hex, written to a file only root can
//!   read ([`PASSWORD_FILE`], mode 400, in kbf-mac-session's state directory) before it
//!   is set on the account, and handed to `dscl` on its standard input. It is never an
//!   argument of a program and never printed. The file is there for the root helper
//!   (kbf-mac-session): it may have to unlock the account's login keychain at session
//!   start (whether automatic login leaves it locked is not yet measured), and it is
//!   the only plain copy of the password (`/etc/kcpassword` holds it too, but obfuscated
//!   by a fixed key, and only while auto-login is on). A password file that is valid is kept: running `apply`
//!   again never changes the password.
//! - Automatic login as this user is the `autologin` item ([`crate::accounts`]), which
//!   uses [`PASSWORD_FILE`] to write `/etc/kcpassword` and to check it.
//!
//! The item is not applied to an account it does not own: another account with the name
//! `kbf-vm`, or holding the uid, makes `apply` change nothing and say so.

use std::io;

use crate::accounts::{DSCL, DSEDITGROUP, dscl_attr};
use crate::directory::{Batch, Directory};
use crate::items::{Ctx, Lines, Outcome, fail, or, pass, run};
use crate::pool::dir_of;

/// The VM user's name.
pub(crate) const VM_USER: &str = "kbf-vm";
const HOME: &str = concat!("/Users", "/kbf-vm");
/// The VM user's login shell.
pub const SHELL: &str = "/bin/zsh";
/// `staff`, the primary group of an ordinary user.
const STAFF_GID: u32 = 20;
/// kbf-mac-session's state directory: root's, mode 700.
pub(crate) const STATE_DIR: &str = "/var/db/kbf-mac-session";
/// The VM user's password, for the root helper: 64 hex digits, no newline.
pub(crate) const PASSWORD_FILE: &str = "/var/db/kbf-mac-session/vm-user-password";
const STATE_DIR_WANT: &str = "drwx------ 0 0";
const PASSWORD_WANT: &str = "-r-------- 0 0";
/// The groups the VM user must be in none of, with their ids (a member is one that has
/// the group as its primary group, or is listed in it): administrators, `wheel`, and the
/// groups that allow Remote Login and Screen Sharing when the Mac limits them.
const NOT_IN: [(&str, u64); 4] = [
    ("admin", 80),
    ("wheel", 0),
    ("com.apple.access_ssh", 399),
    ("com.apple.access_screensharing", 398),
];
/// The bytes of the password before they are written in hex.
const PASSWORD_BYTES: usize = 32;

/// The fixed key macOS XORs `/etc/kcpassword` with.
const KC_KEY: [u8; 11] = [
    0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
];
/// `/etc/kcpassword` is a whole number of these blocks.
const KC_BLOCK: usize = 12;

/// `/etc/kcpassword`'s bytes for `password`: the password, a NUL, zeros up to a whole
/// number of 12-byte blocks (a password whose length is a multiple of 12 gets a whole
/// block more), every byte XORed with the key, repeated.
pub(crate) fn kcpassword(password: &str) -> Vec<u8> {
    let len = (password.len() / KC_BLOCK + 1) * KC_BLOCK;
    let mut bytes = password.as_bytes().to_vec();
    bytes.resize(len, 0);
    for (byte, key) in bytes.iter_mut().zip(KC_KEY.iter().cycle()) {
        *byte ^= key;
    }
    bytes
}

/// What [`PASSWORD_FILE`] holds.
pub(crate) enum PasswordFile {
    /// There is no file.
    Missing,
    /// Something is there that this process cannot read (it is not root).
    Unreadable,
    /// A link, a directory, or a file that is not 64 hex digits.
    Invalid,
    /// The password.
    Valid(String),
}

/// Reads [`PASSWORD_FILE`]. A link is never followed.
pub(crate) fn password_file(c: &Ctx<'_>) -> PasswordFile {
    match c.mac.lstat(PASSWORD_FILE) {
        None => PasswordFile::Missing,
        Some(s) if s.kind != '-' => PasswordFile::Invalid,
        Some(_) => match c.mac.read(PASSWORD_FILE) {
            None => PasswordFile::Unreadable,
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(text)
                    if text.len() == PASSWORD_BYTES * 2
                        && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
                {
                    PasswordFile::Valid(text)
                }
                _ => PasswordFile::Invalid,
            },
        },
    }
}

/// The directory attributes the VM user must have.
fn attributes(uid: u32) -> [(&'static str, String); 5] {
    [
        ("UniqueID", uid.to_string()),
        ("PrimaryGroupID", STAFF_GID.to_string()),
        ("UserShell", SHELL.to_owned()),
        ("NFSHomeDirectory", HOME.to_owned()),
        ("IsHidden", "1".to_owned()),
    ]
}

/// What `ls -ldn` must show for the VM user's home.
fn home_want(uid: u32) -> String {
    format!("drwx------ {uid} {STAFF_GID}")
}

/// What is wrong with the VM user, its home and its password file.
fn problems(c: &Ctx<'_>, uid: u32) -> Vec<String> {
    let dir = match Directory::read(c) {
        Ok(dir) => dir,
        Err(why) => return vec![why],
    };
    let mut wrong = Vec::new();
    match dir.user(VM_USER) {
        None => wrong.push(format!("no user {VM_USER}")),
        Some(record) => {
            for (attr, want) in attributes(uid) {
                let found = record.get(attr);
                if found != want {
                    wrong.push(format!("{attr}={}, want {want}", or(found, "absent")));
                }
            }
            if !record.has("AuthenticationAuthority") {
                wrong.push("no password (no AuthenticationAuthority)".to_owned());
            }
            for (group, gid) in NOT_IN {
                if dir.is_member(record, group, gid) {
                    wrong.push(format!("in {group}"));
                }
            }
        }
    }
    if let Some(other) = dir.uid_holder(uid.into(), |name| name == VM_USER) {
        wrong.push(format!("uid {uid} belongs to {other}"));
    }
    if let Some(other) = dir.user_aliased(VM_USER) {
        wrong.push(format!("also a name of user {other}"));
    }
    let (found, want) = (c.mac.ls(HOME), home_want(uid));
    if found != want {
        wrong.push(format!("home {HOME} is {found}, want {want}"));
    }
    let found = c.mac.ls(STATE_DIR);
    if found != STATE_DIR_WANT {
        wrong.push(format!("{STATE_DIR} is {found}, want {STATE_DIR_WANT}"));
    }
    let found = c.mac.ls(PASSWORD_FILE);
    if found != PASSWORD_WANT {
        wrong.push(format!("{PASSWORD_FILE} is {found}, want {PASSWORD_WANT}"));
    } else if matches!(password_file(c), PasswordFile::Invalid) {
        wrong.push(format!(
            "{PASSWORD_FILE} does not hold {} hex digits",
            PASSWORD_BYTES * 2
        ));
    }
    wrong
}

/// The account `kbf-vm` exists with uid `vm_user_uid`, primary group `staff`, shell
/// `/bin/zsh`, home `kbf-vm` under `/Users`, hidden and with a password; it is in none of
/// [`NOT_IN`]; no other account holds the uid or answers to the name; the home is its
/// own, mode 700; and the password file is root's, mode 400, 64 hex digits, in a state
/// directory that is root's, mode 700. The password itself is not compared with the
/// account's (a hash cannot be read back): the file is the password `apply` set.
/// Unset `vm_user_uid`: the item does not manage a VM user.
pub(crate) fn vm_user(c: &Ctx<'_>) -> Outcome {
    let Some(uid) = c.profile.vm_user_uid else {
        return pass("not managed");
    };
    let wrong = problems(c, uid);
    if wrong.is_empty() {
        pass(format!(
            "{VM_USER} (uid {uid}, group staff, shell {SHELL}): hidden, standard, password in {PASSWORD_FILE}, home {HOME}"
        ))
    } else {
        fail(wrong.join("; "))
    }
}

/// Why `apply` must not touch the directory, or `None`: another account holds the uid
/// or answers to the name, or a record named `kbf-vm` has another uid (its files are
/// owned by that uid: changing it would hand them over).
fn foreign(dir: &Directory, uid: u32) -> Option<String> {
    if let Some(other) = dir.uid_holder(uid.into(), |name| name == VM_USER) {
        return Some(format!("uid {uid} belongs to {other}"));
    }
    if let Some(other) = dir.user_aliased(VM_USER) {
        return Some(format!("user {other} is also named {VM_USER}"));
    }
    let record = dir.user(VM_USER)?;
    (record.has("UniqueID") && record.number("UniqueID") != Some(uid.into())).then(|| {
        format!(
            "user {VM_USER} has uid {}, not the profile's {uid}: it is not the VM user",
            record.get("UniqueID")
        )
    })
}

/// Makes the state directory root's, mode 700 (a link or a file there is left, and
/// refused).
fn fix_state_dir(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    match c.mac.lstat(STATE_DIR) {
        Some(s) if s.kind != 'd' => Err(io::Error::other(format!(
            "{STATE_DIR} is {}, not a directory: no password written",
            c.mac.ls(STATE_DIR)
        ))),
        found => {
            if found.is_none() || c.mac.ls(STATE_DIR) != STATE_DIR_WANT {
                c.mac.dir(STATE_DIR, 0, 0, 0o700)?;
                out.change(c.key, format!("{STATE_DIR}, root's, mode 700"));
            }
            Ok(())
        }
    }
}

/// The password in the file, or a new one written to it first. `true` when it is new.
fn password(c: &Ctx<'_>, out: &mut Lines) -> io::Result<(String, bool)> {
    if let PasswordFile::Valid(password) = password_file(c) {
        if c.mac.ls(PASSWORD_FILE) != PASSWORD_WANT {
            c.mac.chown(PASSWORD_FILE, 0, 0)?;
            c.mac.chmod(PASSWORD_FILE, 0o400)?;
            out.change(c.key, format!("{PASSWORD_FILE}, root's, mode 400"));
        }
        return Ok((password, false));
    }
    let bytes = c.mac.random(PASSWORD_BYTES)?;
    if bytes.len() != PASSWORD_BYTES {
        return Err(io::Error::other("the random source gave too few bytes"));
    }
    let password = hex::encode(bytes);
    c.mac
        .replace(PASSWORD_FILE, password.as_bytes(), 0, 0, 0o400)?;
    out.change(
        c.key,
        format!("{PASSWORD_FILE}: a new random password, root's, mode 400"),
    );
    Ok((password, true))
}

/// Makes the account what [`vm_user`] wants. Nothing is changed while another account
/// holds the uid or the name ([`foreign`]). The password file is written before the
/// password is set, and the password is fed to `dscl` on its standard input; when
/// `dscl` refuses it, a file made in this run is removed, so the next run makes
/// another. An account that has a password and a valid file keeps its password.
pub(crate) fn fix_vm_user(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let Some(uid) = c.profile.vm_user_uid else {
        return Ok(());
    };
    let dir = Directory::read(c).map_err(io::Error::other)?;
    if let Some(why) = foreign(&dir, uid) {
        return Err(io::Error::other(format!("{why}: {VM_USER} not changed")));
    }
    fix_state_dir(c, out)?;
    let (password, fresh) = password(c, out)?;
    let record = format!("/Users/{VM_USER}");
    let found = dir.user(VM_USER);
    let mut batch = Batch::default();
    let wrong: Vec<_> = attributes(uid)
        .into_iter()
        .filter(|(attr, want)| found.is_none_or(|r| r.get(attr) != want))
        .collect();
    if !wrong.is_empty() {
        if found.is_none() {
            batch.create(&record);
        }
        for (attr, want) in &wrong {
            batch.set(&record, attr, want);
        }
        out.change(c.key, format!("user {VM_USER}, uid {uid}"));
    }
    batch.run(c);
    if fresh || !found.is_some_and(|r| r.has("AuthenticationAuthority")) {
        // On standard input, so that it is in no argument list.
        let command = format!("passwd {record} {password}\n");
        if !c.mac.succeeds_with_input(DSCL, &["."], &command) {
            if fresh {
                let _ = c.mac.remove(PASSWORD_FILE);
            }
            return Err(io::Error::other(format!(
                "dscl did not set the password of {VM_USER}{}",
                if fresh {
                    "; the password file was removed, the next apply makes another"
                } else {
                    ""
                }
            )));
        }
        out.change(c.key, format!("password of {VM_USER} set (not shown)"));
    }
    for (group, gid) in NOT_IN {
        if found.is_some_and(|r| dir.is_member(r, group, gid)) {
            run(
                c,
                DSEDITGROUP,
                &["-o", "edit", "-d", VM_USER, "-t", "user", group],
            );
            out.change(c.key, format!("{VM_USER} removed from {group}"));
        }
    }
    if c.mac.ls(HOME) != home_want(uid) {
        dir_of(
            c,
            out,
            HOME,
            (uid, STAFF_GID, 0o700),
            format!("home {HOME}, mode 700"),
        )?;
    }
    Ok(())
}

/// The check of `autologin` when the login window names the VM user, whose `UniqueID`
/// is `uid` (as dscl printed it); `None` when the profile has no VM user, so that the
/// name is any other account's. It passes when the uid is the profile's and
/// `/etc/kcpassword` is a file that holds the password of [`PASSWORD_FILE`].
pub(crate) fn autologin_vm(c: &Ctx<'_>, uid: &str) -> Option<Outcome> {
    let want = c.profile.vm_user_uid?;
    if uid != want.to_string() {
        return Some(fail(format!(
            "auto-login is {VM_USER} (uid {}), not the profile's VM user (uid {want})",
            or(uid, "none")
        )));
    }
    let kc = c.mac.lstat(crate::accounts::KCPASSWORD);
    if kc.is_none_or(|s| s.kind != '-') {
        return Some(fail(format!(
            "auto-login is {VM_USER}, but /etc/kcpassword is missing or not a file: apply writes it"
        )));
    }
    match password_file(c) {
        PasswordFile::Valid(password) => {
            let held = c.mac.read(crate::accounts::KCPASSWORD);
            if held.as_deref() != Some(kcpassword(&password).as_slice()) {
                return Some(fail(format!(
                    "auto-login is {VM_USER}, but /etc/kcpassword does not hold the password in {PASSWORD_FILE}: apply writes it"
                )));
            }
        }
        PasswordFile::Invalid | PasswordFile::Missing => {
            return Some(fail(format!(
                "auto-login is {VM_USER}, but {PASSWORD_FILE} holds no valid password"
            )));
        }
        // Not root: the file cannot be compared; the `vm_user_uid` item checks its mode.
        PasswordFile::Unreadable => {}
    }
    Some(pass(format!(
        "{VM_USER} (uid {uid}), the standing VM user; /etc/kcpassword holds its password"
    )))
}

/// The fix of `autologin=vm`: writes `/etc/kcpassword` from [`PASSWORD_FILE`] and names
/// the VM user as the login window's user. Refuses while FileVault is not off (it
/// would stop at its unlock screen), while the account is not the profile's, or while
/// there is no valid password file. Takes effect at the next login window: nothing is
/// restarted.
pub(crate) fn fix_enable_autologin(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let Some(uid) = c.profile.vm_user_uid else {
        return Err(io::Error::other("the profile has no vm_user_uid"));
    };
    let status = c.mac.out(crate::items::FDESETUP, &["status"]);
    if status.lines().next() != Some("FileVault is Off.") {
        return Err(io::Error::other(format!(
            "FileVault is not off ({}): automatic login cannot work, so it is not set",
            or(status.lines().next().unwrap_or_default(), "no answer")
        )));
    }
    if dscl_attr(c, &format!("/Users/{VM_USER}"), "UniqueID") != uid.to_string() {
        return Err(io::Error::other(format!(
            "no user {VM_USER} with uid {uid}: apply vm_user_uid first"
        )));
    }
    let PasswordFile::Valid(password) = password_file(c) else {
        return Err(io::Error::other(format!(
            "{PASSWORD_FILE} holds no valid password: apply vm_user_uid first"
        )));
    };
    let wanted = kcpassword(&password);
    let kc = crate::accounts::KCPASSWORD;
    if c.mac.lstat(kc).is_none_or(|s| s.kind != '-') || c.mac.read(kc).as_deref() != Some(&wanted) {
        c.mac.replace(kc, &wanted, 0, 0, 0o600)?;
        out.change(c.key, "wrote /etc/kcpassword for kbf-vm (mode 600)");
    }
    if crate::items::defaults_read(c, crate::accounts::LOGINWINDOW, "autoLoginUser") != VM_USER {
        let domain = c.mac.path(crate::accounts::LOGINWINDOW);
        run(
            c,
            crate::items::DEFAULTS,
            &["write", &domain, "autoLoginUser", "-string", VM_USER],
        );
        out.change(
            c.key,
            "auto-login is kbf-vm (from the next login window; nothing is restarted)",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::kcpassword;

    /// Catches: `/etc/kcpassword` obfuscated with another key, block size or padding,
    /// which macOS would read as another password. The bytes are worked out by hand
    /// from the published scheme (XOR with the 11-byte key, repeated, over the password,
    /// a NUL and zeros to 12 bytes), not by this function.
    #[test]
    fn kcpassword_is_the_password_xored_with_the_key_in_whole_blocks() {
        assert_eq!(
            kcpassword("a"),
            [
                0x1C, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F, 0x7D
            ]
        );
        // Twelve characters leave no room for the NUL: a whole block more.
        assert_eq!(kcpassword("abcdefghijkl").len(), 24);
        assert_eq!(kcpassword(&"x".repeat(64)).len(), 72);
    }
}
