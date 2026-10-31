//! The accounts a declaration names, resolved to ids.
//!
//! The loader asks an [`Accounts`] who a name is; [`EtcAccounts`] answers from the text of
//! `/etc/passwd` and `/etc/group`. It does not consult NSS: an account that only a directory
//! service knows is unknown here, and a declaration naming it is refused rather than guessed.

use std::io;
use std::path::Path;

/// The ids of an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserIds {
    pub uid: u32,
    pub gid: u32,
}

/// Who an account name is.
pub trait Accounts {
    /// The ids of the user `name`, `None` if there is no such user.
    fn user(&self, name: &str) -> Option<UserIds>;
    /// The gid of the group `name`.
    fn group(&self, name: &str) -> Option<u32>;
    /// The gids of the groups that list the user `name` as a member (not the primary group).
    fn member_of(&self, name: &str) -> Vec<u32>;
}

/// `/etc/passwd` and `/etc/group` as text.
#[derive(Clone, Debug, Default)]
pub struct EtcAccounts {
    passwd: String,
    group: String,
}

/// The fields of a colon-separated line of `/etc/passwd` or `/etc/group`.
fn fields(line: &str) -> Vec<&str> {
    line.split(':').collect()
}

impl EtcAccounts {
    pub fn new(passwd: &str, group: &str) -> EtcAccounts {
        EtcAccounts {
            passwd: passwd.to_owned(),
            group: group.to_owned(),
        }
    }

    /// Reads the two files.
    pub fn read(passwd: &Path, group: &Path) -> io::Result<EtcAccounts> {
        Ok(EtcAccounts {
            passwd: std::fs::read_to_string(passwd)?,
            group: std::fs::read_to_string(group)?,
        })
    }

    /// The host's own: `/etc/passwd` and `/etc/group`.
    pub fn system() -> io::Result<EtcAccounts> {
        EtcAccounts::read(Path::new("/etc/passwd"), Path::new("/etc/group"))
    }
}

impl Accounts for EtcAccounts {
    fn user(&self, name: &str) -> Option<UserIds> {
        self.passwd.lines().map(fields).find_map(|f| match f[..] {
            [user, _, uid, gid, ..] if user == name => Some(UserIds {
                uid: uid.parse().ok()?,
                gid: gid.parse().ok()?,
            }),
            _ => None,
        })
    }

    fn group(&self, name: &str) -> Option<u32> {
        self.group.lines().map(fields).find_map(|f| match f[..] {
            [group, _, gid, ..] if group == name => gid.parse().ok(),
            _ => None,
        })
    }

    fn member_of(&self, name: &str) -> Vec<u32> {
        self.group
            .lines()
            .map(fields)
            .filter_map(|f| match f[..] {
                [_, _, gid, members] if members.split(',').any(|m| m == name) => gid.parse().ok(),
                _ => None,
            })
            .collect()
    }
}
