//! The text of the macOS Seatbelt profiles that `sandbox-exec` applies, shared by the Mac daemon's
//! native driver (`kbf-driver-native`) and the task runner (`kbf-task`).
//!
//! Nothing here calls the system: every function builds or checks text from plain values, so the
//! rules are tested on Linux. What a profile does when `sandbox-exec` applies it is shown by the
//! tests that run on a Mac (`crates/kbf-driver-native/tests/`, `crates/kbf-task/tests/seatbelt.rs`).
//!
//! # What the rules here do not cover
//!
//! - [`DENY_NETWORK_RULES`] denies every network operation but to loopback and to Unix sockets.
//!   Any Unix socket stays allowed, so a local service that talks to the network on a process's
//!   behalf is reachable: the plainest is the system resolver (`mDNSResponder`), so DNS lookups
//!   still go out and a name a process looks up can carry data off the node.
//! - A connect to the node's own non-loopback address is still allowed under these rules (seen on
//!   the macOS runner by `kbf-driver-native/tests/network.rs`), so a service listening on the
//!   node's LAN address is reachable.
//! - A file deny ([`Profile::deny_read`]) names the file by its path, as the sandbox compares
//!   paths, so the caller passes the real path (no symlink in it; a path that does not exist cannot
//!   be resolved and is written as given, so a symlink in its parent is not followed). The profile
//!   denies reading and writing the file, and renaming or removing each directory above it, so the
//!   file cannot be given another name by a rename, an unlink or a hard link, or by renaming a
//!   directory above it (each shown to be refused by `kbf-task`'s tests on a Mac). A deny on one
//!   letter case of a name also matches another (the same test, on the macOS runner's volume). It
//!   stops the file being read, not a copy of its content that already sits in an argument, an
//!   environment variable or a file that is not named.
//! - `(allow default)` leaves the process every operation the profile does not name, among them
//!   `mach-lookup` of helper daemons, which can act for it outside the profile, and any other
//!   service on loopback or on a Unix socket on the node.
//! - A process of the same user can look at the task runner's own arguments and process: the
//!   Linux hardening against it (`protect_this_process`) does not exist on macOS. A credential
//!   given as a flag value is in those arguments.
//! - `DYLD_*` variables are not a way round the profile: `sandbox-exec` is a platform binary
//!   protected by SIP, which ignores them.

use std::fmt;
use std::path::{Path, PathBuf};

/// Where macOS keeps `sandbox-exec`.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The text of [`DENY_NETWORK_RULES`] as a literal, for `concat!` (a constant cannot be an
/// argument of it): `(deny network*)`, then an allow each for loopback, inbound loopback and Unix
/// sockets. A later rule wins in a profile, so each allow narrows the deny before it.
#[macro_export]
macro_rules! deny_network_rules {
    () => {
        concat!(
            "(deny network*)\n",
            "(allow network-inbound (local ip \"localhost:*\"))\n",
            "(allow network* (remote ip \"localhost:*\"))\n",
            "(allow network* (remote unix-socket))\n"
        )
    };
}

/// A profile's network denial: no network operation but to loopback and Unix sockets (the profile
/// Bazel's macOS sandbox uses). See the module documentation for what stays reachable.
pub const DENY_NETWORK_RULES: &str = deny_network_rules!();

/// The rules every profile carries (after the network rules): no lookup of a
/// CoreDevice mach service or XPC service (`com.apple.CoreDevice.CoreDeviceService`,
/// which `devicectl` asks for everything, `com.apple.CoreDevice.remotepairingd`,
/// `com.apple.CoreDevice.CoreDeviceDDIUpdaterService` and the rest of the prefix), none
/// of `remoted`'s (`com.apple.remoted`, its `.coredevice` and `.control` services and
/// the rest of the prefix: RemoteXPC, the tunnel CoreDevice talks to a device through),
/// and no connect to `usbmuxd`'s socket, which the sandbox names by its real path
/// (`/var/run` is a link to `/private/var/run`; a rule on the link's path matches
/// nothing). `kbf-driver-native/tests/devices.rs` shows each refused on the macOS runner, and
/// reached without the sandbox, `remotepairingd`, `.coredevice` and `.control` among them; the
/// driver's tests check that each mach rule is a `global-name-prefix` covering every
/// name listed here. Their place in the profile does
/// not decide: with the `usbmuxd` rule before the network rules' allow for every
/// Unix socket (a defect), the runner still refused the connect. A lease that books a
/// device is to get a profile without these (`docs/design/ios-devices.md`, section
/// 6.1); none does yet.
pub const NO_DEVICE_RULES: &str = "(deny mach-lookup (global-name-prefix \"com.apple.CoreDevice.\"))\n\
(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))\n\
(deny network-outbound (literal \"/private/var/run/usbmuxd\"))\n";

/// A path that cannot be written into a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathError {
    path: PathBuf,
    why: &'static str,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} cannot be named in a sandbox profile: {}",
            self.path.display(),
            self.why
        )
    }
}

impl std::error::Error for PathError {}

/// `path` as a profile string literal: the text between double quotes, and the quotes.
///
/// A path is refused, never escaped, when it holds a `"` or a `\` (either would end the string
/// early, or start an escape, and let the rest be read as rules), or a control character, or is
/// not UTF-8. `(`, `)` and spaces are plain characters inside the quotes and pass.
///
/// # Errors
/// [`PathError`] for such a path.
pub fn quoted(path: &Path) -> Result<String, PathError> {
    let refuse = |why| PathError {
        path: path.to_owned(),
        why,
    };
    let text = path.to_str().ok_or_else(|| refuse("not UTF-8"))?;
    if text.contains(['"', '\\']) {
        return Err(refuse("holds a double quote or a backslash"));
    }
    if text.chars().any(char::is_control) {
        return Err(refuse("holds a control character"));
    }
    Ok(format!("\"{text}\""))
}

/// What a profile built by [`Profile::render`] does beyond the allow-everything default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// Deny every network operation but to loopback and Unix sockets ([`DENY_NETWORK_RULES`]).
    /// When false the profile has no network clause at all.
    pub deny_network: bool,
    /// Files (each by its absolute real path) the process may not read: one deny each. A file
    /// that holds a credential or a presigned URL.
    pub deny_read: Vec<PathBuf>,
}

impl Profile {
    /// The profile text: allow everything but handing work to `launchd` (`job-creation`,
    /// `lsopen`) and writing a preference through `cfprefsd`, then the network denial if asked,
    /// then [`NO_DEVICE_RULES`], then, per file, `(deny file-read* (literal "<path>"))` and the same for
    /// `file-write*` (which covers a rename, an unlink and a hard link of the file, each of which
    /// would otherwise give the same file a second name the read deny does not name), and one
    /// `file-write-unlink` deny for each directory above it, so the directory cannot be renamed
    /// away with the file in it. The denies are last: a later rule wins.
    ///
    /// Writes are not limited by this profile (the driver's profiles limit them to a lease
    /// directory; this one does not), so a process may write wherever its user can.
    ///
    /// # Errors
    /// [`PathError`] for a path that is not absolute, or that [`quoted`] refuses.
    pub fn render(&self) -> Result<String, PathError> {
        let mut text = String::from(
            "(version 1)\n(allow default)\n(deny job-creation)\n(deny lsopen)\n(deny user-preference-write)\n",
        );
        if self.deny_network {
            text.push_str(DENY_NETWORK_RULES);
        }
        text.push_str(NO_DEVICE_RULES);
        for path in &self.deny_read {
            if !path.is_absolute() {
                return Err(PathError {
                    path: path.clone(),
                    why: "not an absolute path",
                });
            }
            let literal = quoted(path)?;
            // Renaming a directory above the file gives it another path as well: no ancestor of
            // it may be renamed or removed (a child of an ancestor still may).
            for ancestor in path.ancestors().skip(1).filter(|a| a.parent().is_some()) {
                let rule = format!("(deny file-write-unlink (literal {}))\n", quoted(ancestor)?);
                if !text.contains(&rule) {
                    text.push_str(&rule);
                }
            }
            for operation in ["file-read*", "file-write*"] {
                text.push_str("(deny ");
                text.push_str(operation);
                text.push_str(" (literal ");
                text.push_str(&literal);
                text.push_str("))\n");
            }
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests;
