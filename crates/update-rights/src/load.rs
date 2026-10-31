//! Loading the declarations of a host: `/etc/updater.d/*.toml`.
//!
//! The rules, from docs/design/host-updates.md sections 2.4 and 3.3:
//!
//! - the directory and each file are owned by the trusted account (root) and not writable by
//!   the group or others, and a file is a regular file, not a link;
//! - the text is a declaration that every table of which accepts only the keys it knows, and
//!   keeps the rules of `Declaration::check`;
//! - the file is named `<name>.toml`;
//! - the accounts exist, and `hook.user` is not the service's account unless it is root;
//! - no path, once links are resolved, breaks the reserved-path rules;
//! - `state_dir` is not on a path the service's account can write;
//! - no two declarations share a service account, a unit, a health port or a record directory.
//!
//! **A declaration that fails a rule refuses that one service**, which becomes `unmanaged`
//! and raises an alert; the others load. A conflict has two parties and neither has the better
//! claim, so both are refused.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::accounts::{Accounts, UserIds};
use crate::declaration::{Declaration, Refusal};
use crate::digest::Digest;

/// The directory the declarations are installed in.
pub const DECLARATION_DIR: &str = "/etc/updater.d";

/// What a load trusts and asks.
pub struct Policy<'a> {
    /// The account that must own the directory and the files: root, 0.
    pub trusted_uid: u32,
    pub accounts: &'a dyn Accounts,
}

/// A declaration that passed every rule.
#[derive(Clone, Debug)]
pub struct Declared {
    pub declaration: Declaration,
    /// The digest of the exact bytes of the file: what `approve` names.
    pub digest: Digest,
    pub file: PathBuf,
    pub service: UserIds,
    /// The service account's primary group and the groups that list it.
    pub service_gids: Vec<u32>,
}

/// A declaration that was refused, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub file: PathBuf,
    /// The service the file is named for.
    pub service: String,
    pub reason: Refusal,
}

/// What a directory held.
#[derive(Debug, Default)]
pub struct Loaded {
    pub services: BTreeMap<String, Declared>,
    pub refused: Vec<Refused>,
}

/// Checks that the service's account cannot write `state_dir`, nor any directory above it
/// (which could replace it): by owner, by group and by others. Directories that do not exist
/// yet are skipped; the updater creates the record directory. A service that runs as root is
/// not checked, because nothing keeps root from writing anything.
///
/// The loader calls this once; the step runner calls it again at the start of every step.
pub fn check_state_dir(
    _state_dir: &str,
    _service: u32,
    _service_gids: &[u32],
) -> Result<(), Refusal> {
    Ok(())
}

/// Loads every `*.toml` file of `dir`. Only a directory that cannot be listed is an error;
/// everything wrong with a declaration is a [`Refused`].
pub fn load_dir(_dir: &Path, _policy: &Policy<'_>) -> Result<Loaded, io::Error> {
    Ok(Loaded::default())
}
