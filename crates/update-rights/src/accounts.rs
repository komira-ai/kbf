//! The accounts a declaration names, resolved to ids.

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
pub struct EtcAccounts {}

impl EtcAccounts {
    pub fn new(_passwd: &str, _group: &str) -> EtcAccounts {
        EtcAccounts {}
    }

    /// Reads the two files.
    pub fn read(_passwd: &Path, _group: &Path) -> io::Result<EtcAccounts> {
        Ok(EtcAccounts {})
    }

    /// The host's own: `/etc/passwd` and `/etc/group`.
    pub fn system() -> io::Result<EtcAccounts> {
        Ok(EtcAccounts {})
    }
}

impl Accounts for EtcAccounts {
    fn user(&self, _name: &str) -> Option<UserIds> {
        None
    }

    fn group(&self, _name: &str) -> Option<u32> {
        None
    }

    fn member_of(&self, _name: &str) -> Vec<u32> {
        Vec::new()
    }
}
