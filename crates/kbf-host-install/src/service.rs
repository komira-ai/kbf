//! A service the helper installs releases of: kbf, and the other services a root-owned
//! configuration describes.
//!
//! kbf's own descriptor is built in ([`Descriptor::kbf`]) and is what every request without
//! a service has always meant. Any other service is described by a `[services.<name>]` table
//! of the same file as the channels ([`crate::mac::channel`]):
//!
//! ```toml
//! [services.js-agent]
//! artifact = "js-agent"            # the start of its release files' names
//! binary = "js-agent"              # the file of the tarball that is the program
//! tree = "js-agent"                # its releases are in /var/lib/<tree> (a Mac: /usr/local/<tree>)
//! units = ["js-agent.service"]     # restarted on a switch and on a revert
//! ```
//!
//! Every key is required, and none has a default. A descriptor can name only what is its
//! own: the tree is the service's name or starts with it and a dash, and so is the base of
//! each unit, so a service cannot be made to restart `sshd.service`, to write under another
//! service's tree, or to take over kbf's (`kbf` and the names that start with `kbf-` are
//! kbf's).

use serde::Deserialize;

use crate::mac::release::is_artifact_prefix;
use crate::request::{BadCommand, Unit};

/// The service every request without a service means.
pub const KBF: &str = "kbf";

/// The tree of kbf's own releases (`kbf_installer::layout::DEFAULT_TREE`; a test holds the
/// two together).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const KBF_TREE: &str = "kbf-installer";

/// The longest service name, in bytes.
pub const MAX_NAME_LEN: usize = 32;

/// A service name: `[a-z][a-z0-9-]{0,31}`, not ending in `-`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceName(String);

impl ServiceName {
    /// Check `text` against the service grammar.
    ///
    /// # Errors
    /// [`BadCommand`] when it is not in the grammar.
    pub fn new(text: &str) -> Result<Self, BadCommand> {
        let mut bytes = text.bytes();
        let sound = text.len() <= MAX_NAME_LEN
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !text.ends_with('-');
        if sound {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadCommand)
        }
    }

    /// kbf.
    pub fn kbf() -> Self {
        Self(KBF.to_owned())
    }

    /// Whether this is kbf.
    pub fn is_kbf(&self) -> bool {
        self.0 == KBF
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The `[services.<name>]` table, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    artifact: String,
    binary: String,
    tree: String,
    units: Vec<String>,
}

/// What the helper needs to know of a service to install its releases. (The Linux backend
/// reads most of it.)
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Descriptor {
    pub name: ServiceName,
    /// The start of the names of the service's release files (`<artifact>-<commit>-...`).
    pub artifact: String,
    /// The file of the release tarball that is the program, and the file of the release
    /// directory it is installed as.
    pub binary: String,
    /// The name of the tree the service's releases, links, state and lock are in.
    pub tree: String,
    /// The units restarted on a switch and on a revert.
    pub units: Vec<Unit>,
}

impl Descriptor {
    /// kbf's own: its release files are `kbf-<commit>-...`, the program is `kbf`, the tree is
    /// the installer's, and the units are the ones the helper was started with.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn kbf(units: Vec<Unit>) -> Self {
        Self {
            name: ServiceName::kbf(),
            artifact: KBF.to_owned(),
            binary: KBF.to_owned(),
            tree: KBF_TREE.to_owned(),
            units,
        }
    }

    /// The descriptor `entry` makes for the service `name`.
    ///
    /// # Errors
    /// Which part is not allowed, in words.
    pub fn from_entry(name: &str, entry: &Entry) -> Result<Self, String> {
        let service = ServiceName::new(name)
            .map_err(|BadCommand| format!("{name:?} is not a service name"))?;
        if service.is_kbf() || name.starts_with("kbf-") {
            return Err(format!(
                "{name:?} is kbf's: a service of its own has another name"
            ));
        }
        if !is_artifact_prefix(&entry.artifact) {
            return Err(format!(
                "{name}: {:?} is not an artifact name",
                entry.artifact
            ));
        }
        if !is_member(&entry.binary) {
            return Err(format!("{name}: {:?} is not a file name", entry.binary));
        }
        if !is_tree_of(name, &entry.tree) {
            return Err(format!(
                "{name}: the tree {:?} must be {name} or start with {name}-",
                entry.tree
            ));
        }
        if entry.units.is_empty() {
            return Err(format!("{name}: no unit to restart"));
        }
        let units = entry
            .units
            .iter()
            .map(|text| {
                Unit::owned_by(name, text)
                    .map_err(|BadCommand| format!("{name}: {text:?} is not one of its units"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: service,
            artifact: entry.artifact.clone(),
            binary: entry.binary.clone(),
            tree: entry.tree.clone(),
            units,
        })
    }
}

/// Whether `tree` is a tree name `name` may use: its own name, or that and a dash and more.
fn is_tree_of(name: &str, tree: &str) -> bool {
    let own = tree == name
        || tree
            .strip_prefix(name)
            .is_some_and(|rest| rest.strip_prefix('-').is_some_and(|more| !more.is_empty()));
    own && tree.len() <= 64
        && tree
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !tree.ends_with('-')
}

/// Whether `text` is a plain file name of a release: letters, digits and `. _ -`, a letter or
/// digit first, at most 64 bytes, and not the marker the pipeline writes into a release.
fn is_member(text: &str) -> bool {
    text.len() <= 64
        && text != crate::mac::release::VERIFIED_MARKER
        && text.starts_with(|c: char| c.is_ascii_alphanumeric())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(test)]
mod tests;
