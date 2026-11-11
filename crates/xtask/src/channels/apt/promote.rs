//! `channels apt promote --testing <dir> --stable <dir> --out <dir> --version <V> --sums <file>
//! --commit <sha> --origin <t> --label <t> --description <t> [--keep <n>]`: the entries of a
//! release that `testing` lists, merged into the index of `stable`.
//!
//! The release was published into `testing` first, its install checks ran against that, and now
//! the same bytes are made available in `stable`: nothing is built and no package is written (the
//! pool holds the files; `apt write --no-pool` puts the index only). What is checked, before
//! anything is written:
//!
//! - `testing` lists each package of the version, under the name the pool has, and with the
//!   digest the release's `SHA256SUMS` lists for its asset `kbf-native-<commit>-<platform>.deb`: the entry that is promoted is the
//!   one the release was verified against, not whatever the suite holds;
//! - `stable` has no newer version of the package (the channel does not move down), and no entry
//!   at this version with other bytes (the merge of `apt index`, which refuses both).
//!
//! `<dir>` of `--testing` and `--stable` are what `apt read --suite testing` and `--suite stable`
//! wrote. The new unsigned index of `stable` goes to `<out>/dists/stable/` and the ETags of
//! `stable` it was built from to `<out>/etags.txt`; `apt sign` and `apt write --suite stable
//! --no-pool` follow. Promoting a version that `stable` lists already changes no entry.

use std::path::Path;

use super::build::{Current, read_index, write_index};
use super::index::{self, ReleaseInfo, Stanza};
use super::state::{ETAGS_FILE, POOL_DIR, Suite};
use super::state::{PLATFORMS, asset_name, file_name, is_commit};
use super::{control, package_names};
use crate::channels::Error;
use crate::channels::sums::{self, SUMS};

/// What `apt promote` is asked.
pub struct Request<'a> {
    /// The directory `apt read --suite testing` wrote.
    pub testing: &'a Path,
    /// The directory `apt read --suite stable` wrote.
    pub stable: &'a Path,
    /// Where the new unsigned index and `etags.txt` are written.
    pub out: &'a Path,
    /// The release's full commit, which names its package files in `SHA256SUMS`.
    pub commit: &'a str,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
    /// The release's `SHA256SUMS`.
    pub sums: &'a Path,
    /// The `Origin` of a first `Release`.
    pub origin: &'a str,
    /// The `Label` of a first `Release`.
    pub label: &'a str,
    /// The `Description` of a first `Release`.
    pub description: &'a str,
    /// The `Date` of the `Release`, seconds since the epoch.
    pub date: u64,
    /// How many versions of a package and architecture the index keeps.
    pub keep: usize,
}

/// The stanza `stanzas` lists for the pool file `name`.
fn listed<'a>(stanzas: &'a [Stanza], name: &str) -> Option<&'a Stanza> {
    let filename = format!("{POOL_DIR}/{name}");
    stanzas.iter().find(|s| {
        control::parse(&s.text).is_ok_and(|f| control::get(&f, "Filename") == Some(&filename))
    })
}

/// The `apt promote` verb: the lines to print.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, no `etags.txt` in `--stable`, a `SHA256SUMS`
/// that cannot be read or lists no such package, a package `testing` does not list or lists with a
/// digest other than the sums', a `stable` that is newer or lists the version with other bytes, or
/// a file that cannot be written.
pub fn promote(r: &Request) -> Result<Vec<String>, Error> {
    package_names(r.version)?;
    if !is_commit(r.commit) {
        return Err(Error::Usage(format!(
            "--commit {:?} is not a full commit (40 lower case hex digits)",
            r.commit
        )));
    }
    let etags_in = r.stable.join(ETAGS_FILE);
    let etags = std::fs::read_to_string(&etags_in).map_err(|_| {
        Error::Failed(format!(
            "{} is missing: run `apt read --suite stable` first",
            etags_in.display()
        ))
    })?;
    let verified = sums::read_sums(r.sums)?;
    let testing = read_index(r.testing, Suite::Testing)?;
    let mut added = Vec::new();
    for (arch, platform) in PLATFORMS {
        let name = &file_name(r.version, arch);
        let asset = asset_name(r.commit, platform);
        let line = verified
            .iter()
            .find(|l| l.name == asset)
            .ok_or_else(|| Error::Failed(format!("{SUMS} lists no {asset}: nothing to promote")))?;
        let stanza = listed(&testing.stanzas, name).ok_or_else(|| {
            Error::Failed(format!(
                "testing does not list {name}: the release was not published there"
            ))
        })?;
        if stanza.sha256 != line.sha256.to_string() {
            return Err(Error::Failed(format!(
                "testing lists {name} with sha256 {}, but {SUMS} lists {} for {asset}: the bytes that were checked are not the ones in testing",
                stanza.sha256, line.sha256
            )));
        }
        added.push(stanza.clone());
    }
    let Current {
        mut archs,
        stanzas: existing,
        release,
    } = read_index(r.stable, Suite::Stable)?;
    archs.extend(testing.archs);
    archs.sort();
    archs.dedup();
    let merged = index::merge(&existing, &added, r.keep)?;
    let info = index::keep_shared_fields(
        release.as_deref(),
        ReleaseInfo {
            origin: Some(r.origin.to_owned()),
            label: Some(r.label.to_owned()),
            description: Some(r.description.to_owned()),
            date: r.date,
        },
    )?;
    write_index(r.out, Suite::Stable, &info, &merged.stanzas, &archs, &etags)?;
    Ok(vec![format!(
        "apt promote: added {}, dropped {}, listed {}",
        merged.added,
        merged.dropped,
        merged.stanzas.len()
    )])
}

#[cfg(test)]
mod tests;
