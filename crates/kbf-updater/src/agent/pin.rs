//! The pin: root's hold file, which `kbf-host-install activate --pin` writes and
//! `kbf-host-install unpin` removes (`docs/deploy/host-install.md`, "The emergency command").
//!
//! While the hold file is there the agent runs no order for `kbf`, whatever the coordinator
//! desires, and every heartbeat says `pinned` with the commit the file names
//! ([`crate::wire::PinReport`]). The services besides `kbf` are not pinned: the hold names a
//! `kbf` release. The file is looked at for every heartbeat and again before every order, so a
//! pin written between the two still holds the order, and an unpin is followed from the next
//! heartbeat on. An update already running when the pin is written runs to its end: the pin
//! holds the orders that have not started, and never cuts one that has.
//!
//! The hold file is the helper's, at a fixed path whatever the agent's flags say:
//! [`LINUX_PIN`] on Linux, [`MAC_PIN`] on a Mac (the packaged agent's state directory, which
//! is the agent's account's). Since that account owns the directory, only a regular file
//! **owned by root and not writable by its group or by others** is a pin:
//!
//! - no file: not pinned;
//! - a link, anything but a regular file, a file another account owns, or one others may
//!   write: not a pin (the agent's own account could have put it there or written it), and the
//!   agent says why;
//! - root's regular file: pinned, with the commit it names (`{"commit": "<40 hex>", ...}`, other
//!   keys ignored). One that cannot be read, is too long, or names no commit pins the host all
//!   the same, with no commit: root put it there to hold the host;
//! - a name that cannot be looked at (the directory cannot be searched, an I/O error): pinned,
//!   with no commit. That the file is not there, or not root's, is never assumed.
//!
//! A forced order of the coordinator that names the pinned commit is to clear the pin; that
//! order does not exist yet, so until it does every order is held while the file is there.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::PathBuf;

use serde::Deserialize;

use crate::wire::{Commit, PinReport};

/// The hold file on Linux: the helper writes it there (`kbf-host-install`'s `LINUX_HOLD`).
pub const LINUX_PIN: &str = "/var/lib/kbf-updater/pin.json";

/// The hold file on a Mac: the helper writes it there (`kbf-host-install`'s `MAC_HOLD`).
pub const MAC_PIN: &str = "/var/db/kbf-updater/pin.json";

/// The account a hold file must be owned by in production: root.
pub const ROOT: u32 = 0;

/// The most bytes of a hold file that are read.
const PIN_MAX: u64 = 4096;

/// What the hold file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// No hold file: the agent follows the coordinator.
    Absent,
    /// The host is pinned. `problem` says why the commit is not known, when it is not.
    Pinned {
        report: PinReport,
        problem: Option<String>,
    },
    /// A file is there, but it is no pin, for this reason.
    NotAPin(String),
}

impl Look {
    /// The pin the heartbeat reports, if the host is pinned.
    #[must_use]
    pub fn report(&self) -> Option<&PinReport> {
        match self {
            Self::Pinned { report, .. } => Some(report),
            Self::Absent | Self::NotAPin(_) => None,
        }
    }

    /// The line the agent logs when it first sees this.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::Absent => "not pinned: following the coordinator".to_owned(),
            Self::Pinned {
                report,
                problem: None,
            } => match &report.commit {
                Some(commit) => format!("pinned to {commit}: no order for kbf runs"),
                None => "pinned: no order for kbf runs".to_owned(),
            },
            Self::Pinned {
                problem: Some(why), ..
            } => format!("pinned, the commit unknown ({why}): no order for kbf runs"),
            Self::NotAPin(why) => format!("the hold file is not a pin: {why}"),
        }
    }
}

/// The hold file, and the account a pin must be owned by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinFile {
    path: PathBuf,
    owner: u32,
}

/// The keys of a hold file the agent reads.
#[derive(Deserialize)]
struct Hold {
    commit: String,
}

impl PinFile {
    /// The hold file `path`, a pin only when `owner` owns it ([`ROOT`] in production).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, owner: u32) -> Self {
        Self {
            path: path.into(),
            owner,
        }
    }

    /// The helper's hold file on the operating system `os` (as Rust names it: `macos` is a Mac,
    /// anything else is taken for Linux), owned by root.
    #[must_use]
    pub fn of_platform(os: &str) -> Self {
        let path = if os == "macos" { MAC_PIN } else { LINUX_PIN };
        Self::new(path, ROOT)
    }

    /// What the hold file says now.
    #[must_use]
    pub fn look(&self) -> Look {
        let at = |why: &dyn std::fmt::Display| format!("{}: {why}", self.path.display());
        let unknown = |problem: String| Look::Pinned {
            report: PinReport { commit: None },
            problem: Some(problem),
        };
        // O_NOFOLLOW: a link is never followed; O_NONBLOCK: a pipe put there is never waited
        // on. The file opened is judged by its own metadata.
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path);
        let file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Look::Absent,
            // Not opened: a link, a socket, a file this account may not read, or a name that
            // cannot be looked at. Root's regular file is a pin whose commit cannot be known,
            // and so is a name whose file cannot be told: it may be root's.
            Err(e) => {
                return match std::fs::symlink_metadata(&self.path) {
                    Ok(meta) => self
                        .untrusted(&meta)
                        .map_or_else(|| unknown(at(&e)), Look::NotAPin),
                    Err(gone) if gone.kind() == ErrorKind::NotFound => Look::Absent,
                    Err(_) => unknown(at(&e)),
                };
            }
        };
        match file.metadata().map(|meta| self.untrusted(&meta)) {
            Ok(Some(why)) => return Look::NotAPin(why),
            Ok(None) => {}
            Err(e) => return unknown(at(&e)),
        }
        match read_commit(file) {
            Ok(commit) => Look::Pinned {
                report: PinReport {
                    commit: Some(commit),
                },
                problem: None,
            },
            Err(why) => unknown(at(&why)),
        }
    }

    /// Why a file with `meta` is not a pin, if it is not.
    fn untrusted(&self, meta: &std::fs::Metadata) -> Option<String> {
        let kind = meta.file_type();
        let what = if kind.is_symlink() {
            "a link"
        } else if kind.is_dir() {
            "a directory"
        } else if !kind.is_file() {
            "not a regular file"
        } else if meta.uid() != self.owner {
            return Some(format!(
                "{} is owned by uid {}, not {}",
                self.path.display(),
                meta.uid(),
                self.owner
            ));
        } else if meta.mode() & 0o022 != 0 {
            "writable by its group or by others"
        } else {
            return None;
        };
        Some(format!("{} is {what}", self.path.display()))
    }
}

/// The commit root's hold file `file` names, or why it cannot be known.
fn read_commit(file: File) -> Result<Commit, String> {
    let mut text = Vec::new();
    file.take(PIN_MAX + 1)
        .read_to_end(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > PIN_MAX {
        return Err("it is too long".to_owned());
    }
    let hold: Hold = serde_json::from_slice(&text).map_err(|e| e.to_string())?;
    Commit::parse(&hold.commit).ok_or_else(|| "its commit is not 40 hex digits".to_owned())
}
