//! The GUI session switch's pieces (S4.2, `session-login` and `session-idle`): the
//! intent record, `/etc/kcpassword`, the login window's `autoLoginUser`, FileVault's
//! state and the userspace restart.
//!
//! - **The intent record** is a root-only file in the helper's state directory that
//!   names the one lease whose user the session switch logs in: `<term>.<seq>` and a
//!   newline. `session-login` writes it before it changes anything else, and
//!   `session-idle` removes it after auto-login is cleared, so whenever auto-login names
//!   a lease user, the record names that lease. The boot reset reads it, and so does
//!   `kbf-mac-provision check`, which fails a lease user's auto-login that no record
//!   names.
//! - **`/etc/kcpassword`** holds the password the login window logs in with,
//!   obfuscated by the fixed XOR key macOS uses ([`kcpassword`]); it is not secret,
//!   which is acceptable only for a random password of a lease user that is deleted
//!   after its lease.
//! - Every program runs through [`Host::command`](crate::helper::Host::command), so
//!   the tests fake each one and nothing is restarted.
//!
//! Files are written whole (a temporary file, `fsync`, `rename`, then the directory's
//! `fsync`) and never through a link.

use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use kbf_types::LeaseId;
use rustix::fs::{CWD, Mode, OFlags};

use crate::helper::Host;
use crate::lease::parse_lease;

/// The programs the switch runs. Absolute, and run with an empty environment.
pub const DEFAULTS: &str = "/usr/bin/defaults";
pub const LAUNCHCTL: &str = "/bin/launchctl";
pub const FDESETUP: &str = "/usr/bin/fdesetup";

/// The login window's preference that names the user to log in at start.
pub const AUTO_LOGIN_USER: &str = "autoLoginUser";

/// The fixed key macOS XORs `/etc/kcpassword` with.
const KC_KEY: [u8; 11] = [
    0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
];
/// `/etc/kcpassword` is a whole number of these blocks.
const KC_BLOCK: usize = 12;

/// Where the switch's state lives.
#[derive(Clone, Debug)]
pub struct SessionPaths {
    /// The intent record.
    pub intent: PathBuf,
    /// `/etc/kcpassword`.
    pub kcpassword: PathBuf,
    /// The login window's preferences domain, as `defaults` takes it
    /// (`/Library/Preferences/com.apple.loginwindow`).
    pub loginwindow: String,
}

impl SessionPaths {
    /// The paths a test uses: everything under `dir`.
    #[cfg(test)]
    pub(crate) fn under(dir: &Path) -> Self {
        Self {
            intent: dir.join("session-intent"),
            kcpassword: dir.join("kcpassword"),
            loginwindow: dir.join("loginwindow").display().to_string(),
        }
    }
}

/// What a program printed, and whether it exited 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `program` with `args` and an empty environment, and waits for it.
///
/// # Errors
/// The program could not be started or waited for.
pub fn run_command(program: &str, args: &[&str]) -> io::Result<CommandOutput> {
    let output = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn failed(what: &str, output: &CommandOutput) -> String {
    format!("{what} failed: {}", output.stderr.trim())
}

fn command(host: &dyn Host, program: &str, args: &[&str]) -> Result<CommandOutput, String> {
    host.command(program, args)
        .map_err(|why| format!("running {program}: {why}"))
}

/// `/etc/kcpassword`'s bytes for `password`: the password, a NUL, zeros up to a whole
/// number of 12-byte blocks (so a password whose length is a multiple of 12 gets a
/// whole block more), every byte XORed with the 11-byte key, repeated.
#[must_use]
pub fn kcpassword(password: &[u8]) -> Vec<u8> {
    let len = (password.len() / KC_BLOCK + 1) * KC_BLOCK;
    let mut bytes = password.to_vec();
    bytes.resize(len, 0);
    for (byte, key) in bytes.iter_mut().zip(KC_KEY.iter().cycle()) {
        *byte ^= key;
    }
    bytes
}

/// The lease the intent record names; `None` when there is none.
///
/// # Errors
/// The record cannot be read (or is a link), or does not hold one lease id.
pub fn read_intent(path: &Path) -> Result<Option<LeaseId>, String> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = match rustix::fs::openat(CWD, path, flags, Mode::empty()) {
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        other => other.map_err(|why| format!("{}: {why}", path.display()))?,
    };
    let mut text = String::new();
    std::fs::File::from(opened)
        .read_to_string(&mut text)
        .map_err(|why| format!("{}: {why}", path.display()))?;
    let lease = text
        .strip_suffix('\n')
        .ok_or_else(|| format!("{}: {text:?} is not one lease id", path.display()))?;
    parse_lease(lease)
        .map(Some)
        .map_err(|why| format!("{}: {why}", path.display()))
}

/// Writes the intent record naming `lease`, durably.
///
/// # Errors
/// The record could not be written.
pub fn write_intent(path: &Path, lease: LeaseId) -> io::Result<()> {
    write_whole(path, format!("{lease}\n").as_bytes())
}

/// Writes `/etc/kcpassword` for `password`, durably, mode 0600.
///
/// # Errors
/// The file could not be written.
pub fn write_kcpassword(path: &Path, password: &[u8]) -> io::Result<()> {
    write_whole(path, &kcpassword(password))
}

/// Removes `path` (a file, or a link itself), durably; `false` if there was none.
///
/// # Errors
/// It could not be removed.
pub fn remove(path: &Path) -> io::Result<bool> {
    match rustix::fs::unlink(path) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(why) => return Err(why.into()),
    }
    sync_parent(path)?;
    Ok(true)
}

/// Writes `bytes` to a new temporary file next to `path` (mode 0600, never through a
/// link), `fsync`s it, renames it over `path` and `fsync`s the directory.
pub(crate) fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    match rustix::fs::unlink(&temporary) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(why) => return Err(why.into()),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::openat(CWD, &temporary, flags, Mode::from_raw_mode(0o600))?;
    let mut file = std::fs::File::from(opened);
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    rustix::fs::rename(&temporary, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir = rustix::fs::openat(CWD, parent, flags, Mode::empty())?;
    Ok(rustix::fs::fsync(&dir)?)
}

/// Refuses unless `fdesetup isactive` says FileVault is off: with it on, macOS never
/// logs anyone in by itself, and a restart would leave the Mac at the unlock screen.
///
/// # Errors
/// FileVault is on, or its state could not be read.
pub fn require_filevault_off(host: &dyn Host) -> Result<(), String> {
    let output = command(host, FDESETUP, &["isactive"])?;
    match output.stdout.trim() {
        "false" => Ok(()),
        "true" => Err("FileVault is on, and macOS logs no one in by itself with it on".to_owned()),
        other => Err(format!(
            "fdesetup isactive answered {other:?} ({}), not true or false",
            output.stderr.trim()
        )),
    }
}

/// Sets the login window's `autoLoginUser` to `name`.
///
/// # Errors
/// `defaults` failed.
pub fn set_auto_login(host: &dyn Host, paths: &SessionPaths, name: &str) -> Result<(), String> {
    let domain = paths.loginwindow.as_str();
    let args = ["write", domain, AUTO_LOGIN_USER, "-string", name];
    let output = command(host, DEFAULTS, &args)?;
    if output.success {
        Ok(())
    } else {
        Err(failed(
            &format!("defaults write {domain} {AUTO_LOGIN_USER}"),
            &output,
        ))
    }
}

/// Clears the login window's `autoLoginUser` and removes `/etc/kcpassword`; neither
/// being there already is fine. Returns whether anything was there to clear.
///
/// `defaults delete` of a key that is not there fails, so a failed delete is checked
/// with `defaults read`: the key is cleared only if that fails too.
///
/// # Errors
/// The key is still set, or the file could not be removed.
pub fn clear_auto_login(host: &dyn Host, paths: &SessionPaths) -> Result<bool, String> {
    let domain = paths.loginwindow.as_str();
    let deleted = command(host, DEFAULTS, &["delete", domain, AUTO_LOGIN_USER])?;
    if !deleted.success {
        let read = command(host, DEFAULTS, &["read", domain, AUTO_LOGIN_USER])?;
        if read.success {
            return Err(failed(
                &format!("defaults delete {domain} {AUTO_LOGIN_USER}"),
                &deleted,
            ));
        }
    }
    let removed = remove(&paths.kcpassword)
        .map_err(|why| format!("removing {}: {why}", paths.kcpassword.display()))?;
    Ok(deleted.success || removed)
}

/// Asks launchd for a userspace restart: every process but the kernel's ends, and the
/// login window starts again, logging in whoever `autoLoginUser` names.
///
/// # Errors
/// `launchctl` refused.
pub fn restart_userspace(host: &dyn Host) -> Result<(), String> {
    let output = command(host, LAUNCHCTL, &["reboot", "userspace"])?;
    if output.success {
        Ok(())
    } else {
        Err(failed("launchctl reboot userspace", &output))
    }
}

#[cfg(test)]
mod tests;
