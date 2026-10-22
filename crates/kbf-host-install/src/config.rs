//! The allowlist, `/etc/kbf/host-install.toml`, and the apt source it points at.
//!
//! ```toml
//! [channels.candidate]
//! suite = "candidate"
//! source_list = "/etc/kbf/host-install/candidate.sources"
//! ```
//!
//! The file, and each source file, must be root's alone to write ([`crate::trust`]). A
//! channel is usable only when its source names a `signed-by` key on every entry, as an
//! absolute path to a file that is itself root's alone to write: an unsigned source, a key
//! given as a fingerprint or inline, or a key a user could replace is refused. The source
//! is either a deb822 file (`Types:`, `Signed-By:`) or a one-line apt list
//! (`deb [signed-by=/path] ...`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::outcome::Refusal;
use crate::request::Channel;
use crate::trust::{Trust, TrustError};

/// A channel after its checks: what the plan needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSource {
    /// The suite apt installs from (`-t`).
    pub suite: String,
    /// The only apt source file the install reads.
    pub source_list: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    channels: BTreeMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    suite: String,
    source_list: PathBuf,
}

/// The parsed allowlist.
#[derive(Debug)]
pub struct Allowlist {
    channels: BTreeMap<String, Entry>,
}

impl Allowlist {
    /// Read and parse the allowlist at `path`.
    ///
    /// # Errors
    /// [`Refusal::ConfigUnsafe`] when the file is not root's alone to write,
    /// [`Refusal::ConfigInvalid`] when it is missing or does not parse.
    pub fn load(trust: &Trust, path: &Path) -> Result<Self, Refusal> {
        let text = trust.read(path).map_err(refusal)?;
        let file: File = toml::from_str(&text).map_err(|_| Refusal::ConfigInvalid)?;
        let well_formed = file.channels.iter().all(|(name, entry)| {
            // A channel and a suite are written alike, and a source is a plain absolute path.
            Channel::new(name).is_ok()
                && Channel::new(&entry.suite).is_ok()
                && entry.source_list.is_absolute()
                && entry
                    .source_list
                    .to_str()
                    .is_some_and(|p| !p.chars().any(char::is_control))
        });
        if !well_formed {
            return Err(Refusal::ConfigInvalid);
        }
        Ok(Self {
            channels: file.channels,
        })
    }

    /// The channel `name`, once its source passes the signing checks.
    ///
    /// # Errors
    /// [`Refusal::ChannelNotAllowed`], [`Refusal::ConfigUnsafe`],
    /// [`Refusal::ConfigInvalid`], [`Refusal::ChannelUnsigned`] or
    /// [`Refusal::KeyUntrusted`].
    pub fn channel(&self, trust: &Trust, name: &Channel) -> Result<ChannelSource, Refusal> {
        let entry = self
            .channels
            .get(name.as_str())
            .ok_or(Refusal::ChannelNotAllowed)?;
        let text = trust.read(&entry.source_list).map_err(refusal)?;
        check_signing(trust, &text)?;
        Ok(ChannelSource {
            suite: entry.suite.clone(),
            source_list: entry.source_list.clone(),
        })
    }
}

/// The refusal for a file that could not be trusted: unsafe when someone else could have
/// written it, invalid when it could not be read.
fn refusal(error: TrustError) -> Refusal {
    match error {
        TrustError::NotOwned | TrustError::Writable => Refusal::ConfigUnsafe,
        TrustError::Io(_) | TrustError::NotRegular | TrustError::TooLarge => Refusal::ConfigInvalid,
    }
}

/// What one stanza of a deb822 file has collected so far.
#[derive(Default)]
struct Stanza {
    is_entry: bool,
    signed_by: Option<String>,
}

impl Stanza {
    /// End the stanza: an entry (one with `Types`) is recorded with its key, if any.
    fn finish(&mut self, found: &mut Vec<Option<String>>) {
        let stanza = std::mem::take(self);
        if stanza.is_entry {
            found.push(stanza.signed_by);
        }
    }
}

/// The `signed-by` value of each entry of an apt source file, `None` for an entry with
/// none. Both formats: a one-line entry is `deb[-src] [options] uri suite components`; a
/// deb822 stanza is `Field: value` lines, continued on lines that start with white space.
fn entries(text: &str) -> Vec<Option<String>> {
    let mut found = Vec::new();
    let mut stanza = Stanza::default();
    let mut in_signed_by = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            stanza.finish(&mut found);
            in_signed_by = false;
        } else if line.starts_with('#') {
            continue;
        } else if line.starts_with([' ', '\t']) {
            // A continuation line belongs to the field above it.
            if let Some(value) = stanza.signed_by.as_mut().filter(|_| in_signed_by) {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if matches!(line.split_whitespace().next(), Some("deb" | "deb-src")) {
            stanza.finish(&mut found);
            found.push(one_line_signed_by(line));
            in_signed_by = false;
        } else if let Some((field, value)) = line.split_once(':') {
            in_signed_by = field.eq_ignore_ascii_case("Signed-By");
            stanza.is_entry |= field.eq_ignore_ascii_case("Types");
            if in_signed_by {
                stanza.signed_by = Some(value.trim().to_owned());
            }
        }
    }
    stanza.finish(&mut found);
    found
}

/// The `signed-by=` option of a one-line entry.
fn one_line_signed_by(line: &str) -> Option<String> {
    let rest = line.split_once(char::is_whitespace)?.1.trim_start();
    let options = rest.strip_prefix('[')?.split_once(']')?.0;
    options.split_whitespace().find_map(|option| {
        let (key, value) = option.split_once('=')?;
        key.eq_ignore_ascii_case("signed-by")
            .then(|| value.to_owned())
    })
}

/// Every entry of the source must name keys, and every key must be an absolute path to a
/// file the trusted user alone can write.
fn check_signing(trust: &Trust, text: &str) -> Result<(), Refusal> {
    let found = entries(text);
    if found.is_empty() || found.iter().any(Option::is_none) {
        return Err(Refusal::ChannelUnsigned);
    }
    for value in found.iter().flatten() {
        let keys: Vec<&str> = value
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|k| !k.is_empty())
            .collect();
        let trusted = !keys.is_empty()
            && keys
                .iter()
                .all(|key| key.starts_with('/') && trust.check_file(Path::new(key)).is_ok());
        if !trusted {
            return Err(Refusal::KeyUntrusted);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
