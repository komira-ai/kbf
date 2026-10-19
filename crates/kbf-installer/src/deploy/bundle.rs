//! The bundle `deploy` is handed on standard input: a tar of the commit's tarballs and
//! their build attestations, each entry named as the release store names it.
//!
//! `<name>-<commit>-<platform>.tar.gz` is a tarball; the same name plus `.attestation` is
//! its build-attestation bundle. Nothing else is accepted: an entry the store would not
//! take is a mistake in whoever packed the bundle, and is refused before anything is
//! uploaded. This is bytes, not trust: the node and the frontend's own update verify the
//! attestation offline (docs/design/pull-updates.md, section 5).

use std::io::Read;

use super::why;
use crate::CommitId;

/// The most a tarball may be (the store's cap).
pub const MAX_TARBALL: u64 = 512 * 1024 * 1024;
/// The most an attestation bundle may be.
pub const MAX_ATTESTATION: u64 = 256 * 1024;

/// The platforms the store knows.
pub const PLATFORMS: [&str; 3] = ["linux-x86_64", "linux-arm64", "darwin-arm64"];
/// The components the store takes.
pub const COMPONENTS: [&str; 2] = ["kbf", "kbf-daemon"];

/// One file to upload.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub platform: String,
    /// `kbf` or `kbf-daemon`, with `.attestation` for the bundle.
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Entry {
    /// The store path for this file.
    pub fn path(&self, commit: &CommitId) -> String {
        format!("/v1/releases/{commit}/{}/{}", self.platform, self.name)
    }
}

/// Read the whole bundle. Every entry must be for `commit`.
pub fn read(mut input: impl Read, commit: &CommitId) -> Result<Vec<Entry>, String> {
    read_from(&mut input, commit)
}

/// [`read`], from one reader type whatever the caller's (so one copy of the code).
fn read_from(input: &mut dyn Read, commit: &CommitId) -> Result<Vec<Entry>, String> {
    let mut archive = tar::Archive::new(input);
    let mut out = Vec::new();
    let entries = archive.entries().map_err(why("not a tar"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("bad tar entry: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(why("bad entry name"))?;
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("an entry has no usable name")?
            .to_owned();
        let (platform, name) = classify(&file, commit)?;
        let cap = if name.ends_with(".attestation") {
            MAX_ATTESTATION
        } else {
            MAX_TARBALL
        };
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("reading {file}: {e}"))?;
        if bytes.len() as u64 > cap {
            return Err(format!("{file} is larger than {cap} bytes"));
        }
        if bytes.is_empty() {
            return Err(format!("{file} is empty"));
        }
        if out
            .iter()
            .any(|e: &Entry| e.platform == platform && e.name == name)
        {
            return Err(format!("{file} appears twice"));
        }
        out.push(Entry {
            platform,
            name,
            bytes,
        });
    }
    if out.is_empty() {
        return Err("the bundle holds no file".to_owned());
    }
    Ok(out)
}

/// `(platform, store name)` for a file name, or why it is refused.
fn classify(file: &str, commit: &CommitId) -> Result<(String, String), String> {
    let (stem, attestation) = match file.strip_suffix(".attestation") {
        Some(stem) => (stem, true),
        None => (file, false),
    };
    let stem = stem
        .strip_suffix(".tar.gz")
        .ok_or_else(|| format!("{file}: not <name>-<commit>-<platform>.tar.gz[.attestation]"))?;
    for component in COMPONENTS {
        let Some(rest) = stem
            .strip_prefix(component)
            .and_then(|r| r.strip_prefix('-'))
        else {
            continue;
        };
        let Some(platform) = rest
            .strip_prefix(commit.as_str())
            .and_then(|r| r.strip_prefix('-'))
        else {
            continue;
        };
        if PLATFORMS.contains(&platform) {
            let name = if attestation {
                format!("{component}.attestation")
            } else {
                component.to_owned()
            };
            return Ok((platform.to_owned(), name));
        }
    }
    Err(format!(
        "{file}: not a kbf or kbf-daemon tarball of this commit on a known platform"
    ))
}
