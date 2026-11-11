//! The two text formats `release desired` reads: the release's `SHA256SUMS` and an apt
//! repository's `Packages` index.

use crate::Error;

use super::{PACKAGE, POOL_DIR, SUMS};

/// The digest `sha256sum` wrote for `name` in `text`.
///
/// A line is 64 lowercase hex digits, a space, then a space or `*` (binary mode) and the name.
///
/// # Errors
/// [`Error::Failed`] naming the line, for a line that is not of that form.
pub fn sums_digest(text: &str, name: &str) -> Result<Option<String>, Error> {
    let mut found = None;
    for (at, line) in text.lines().enumerate() {
        let parsed = line
            .split_once(' ')
            .filter(|(digest, _)| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .and_then(|(digest, rest)| Some((digest, rest.strip_prefix([' ', '*'])?)));
        let Some((digest, listed)) = parsed else {
            return Err(Error::Failed(format!(
                "{SUMS} line {} is not `<sha256>  <name>`: {line:?}",
                at + 1
            )));
        };
        if listed == name {
            found = Some(digest.to_owned());
        }
    }
    Ok(found)
}

/// What a `Packages` index says of the `kbf` package of one architecture at one digest.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed {
    /// A stanza lists the `.deb` with that `SHA256`: its `Version`.
    Yes(String),
    /// No stanza does; these are the versions the index lists for the architecture (empty
    /// when it lists none), each with another digest.
    No(Vec<String>),
}

/// The value of the field `key` of `stanza`.
fn field(stanza: &str, key: &str) -> Option<String> {
    stanza
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
        .map(str::to_owned)
}

/// Whether `text` lists the package `kbf` for `arch` with the SHA-256 `digest`.
///
/// A stanza counts when its `Package` is `kbf` and its `Filename` is exactly
/// `pool/kbf/kbf_<Version>_<arch>.deb`, with `<Version>` its own `Version` field: the form the
/// publisher writes.
#[must_use]
pub fn listed(text: &str, arch: &str, digest: &str) -> Listed {
    let mut others = Vec::new();
    for stanza in text.split("\n\n") {
        let (Some(package), Some(version), Some(filename)) = (
            field(stanza, "Package"),
            field(stanza, "Version"),
            field(stanza, "Filename"),
        ) else {
            continue;
        };
        if package != PACKAGE || filename != format!("{POOL_DIR}/{PACKAGE}_{version}_{arch}.deb") {
            continue;
        }
        if field(stanza, "SHA256").as_deref() == Some(digest) {
            return Listed::Yes(version);
        }
        others.push(version);
    }
    Listed::No(others)
}
