//! `channels apt index --current <dir> --out <dir> --debs <dir> --commit <sha> --version <V> --origin
//! <t> --label <t> --description <t> [--keep <n>]`: the packages merged into the index
//! `apt read` copied to `<current>`.
//!
//! The index is shared with other products. Their entries are kept as they are, the architectures
//! they list are kept (an index written without one would drop its packages), and the `Origin`,
//! `Label` and `Description` of the `Release` that exists stay: the three flags apply to the
//! first `Release` only. A version older than the newest listed for kbf is refused, and so
//! is the same version with other bytes. Everything is computed before anything is written; the
//! new unsigned index goes to `<out>/dists/<suite>/` and the ETags it was built from to
//! `<out>/etags.txt`. The `Date` is the clock's, read by the verb.

use std::path::Path;

use super::control;
use super::index::{self, ReleaseInfo, Stanza};
use super::package_files;
use super::state::{self, ETAGS_FILE, POOL_DIR, Suite};
use crate::channels::Error;

/// How many versions of a package and architecture the index keeps, by default.
pub const KEEP: usize = 25;

/// What `apt index` is asked.
pub struct Request<'a> {
    /// The suite of the repository.
    pub suite: Suite,
    /// The directory `apt read` wrote.
    pub current: &'a Path,
    /// Where the new unsigned index and `etags.txt` are written.
    pub out: &'a Path,
    /// The release's package files (the `dist` directory).
    pub debs: &'a Path,
    /// The release's full commit, which names its package files.
    pub commit: &'a str,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
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

/// The directories `binary-<arch>` below `main` in `dists`, as architectures.
fn listed_archs(main: &Path) -> Vec<String> {
    std::fs::read_dir(main)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("binary-").map(str::to_owned)
        })
        .collect()
}

pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let what = || format!("write {}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(Error::spawn(what()))?;
    std::fs::write(path, bytes).map_err(Error::spawn(what()))
}

/// An index as `apt read` copied it.
pub(super) struct Current {
    /// The architectures it lists together with the defaults.
    pub archs: Vec<String>,
    /// Every stanza of their `Packages`.
    pub stanzas: Vec<Stanza>,
    /// The text of its `Release`, when it has one.
    pub release: Option<String>,
}

/// The index of `suite` in `dir` (what `apt read` wrote): the architectures it lists together with
/// the defaults, every stanza of their `Packages`, and the text of its `Release` when it has one.
///
/// # Errors
/// A `Packages` file that is not one.
pub(super) fn read_index(dir: &Path, suite: Suite) -> Result<Current, Error> {
    let root = dir.join("dists").join(suite.name());
    let main = root.join("main");
    let mut archs: Vec<String> = state::DEFAULT_ARCHS
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    archs.extend(listed_archs(&main));
    archs.sort();
    archs.dedup();
    let mut existing = Vec::new();
    for arch in &archs {
        let path = main.join(format!("binary-{arch}")).join("Packages");
        if let Ok(text) = std::fs::read_to_string(&path) {
            existing.extend(
                index::parse_packages(&text)
                    .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?,
            );
        }
    }
    Ok(Current {
        archs,
        stanzas: existing,
        release: std::fs::read_to_string(root.join("Release")).ok(),
    })
}

/// Writes the new unsigned index of `suite` to `out`, and the ETags it was built from. Everything
/// is computed before anything is written.
///
/// # Errors
/// An index that cannot be built, or a file that cannot be written.
pub(super) fn write_index(
    out: &Path,
    suite: Suite,
    info: &ReleaseInfo,
    stanzas: &[Stanza],
    archs: &[String],
    etags: &str,
) -> Result<(), Error> {
    let tree = index::tree(suite, info, stanzas, archs)?;
    for file in &tree {
        write(&out.join(&file.path), &file.data)?;
    }
    // The new index was built from exactly the objects whose ETags these are.
    write(&out.join(ETAGS_FILE), etags.as_bytes())
}

/// The `apt index` verb: the lines to print.
///
/// # Errors
/// No `etags.txt` in `current`, a package that is missing or that says other than its name says,
/// an index that cannot be read or merged, or a file that cannot be written.
pub fn build(r: &Request) -> Result<Vec<String>, Error> {
    let etags_in = r.current.join(ETAGS_FILE);
    let etags = std::fs::read_to_string(&etags_in).map_err(|_| {
        Error::Failed(format!(
            "{} is missing: run `apt read` first",
            etags_in.display()
        ))
    })?;
    let mut added: Vec<Stanza> = Vec::new();
    for (name, path) in package_files(r.debs, r.commit, r.version)? {
        let bytes =
            std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
        let stanza = index::stanza_of(&bytes).map_err(|e| Error::Failed(format!("{name}: {e}")))?;
        // The package's own control file decides the entry's file name; it must be this file.
        let fields = control::parse(&stanza.text)?;
        let filename = control::get(&fields, "Filename").unwrap_or_default();
        if filename != format!("{POOL_DIR}/{name}") {
            return Err(Error::Failed(format!(
                "{name} is the package {filename}: its control file does not say the name it has"
            )));
        }
        added.push(stanza);
    }
    let Current {
        archs,
        stanzas: existing,
        release,
    } = read_index(r.current, r.suite)?;
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
    write_index(r.out, r.suite, &info, &merged.stanzas, &archs, &etags)?;
    Ok(vec![format!(
        "apt index: added {}, dropped {}, listed {}",
        merged.added,
        merged.dropped,
        merged.stanzas.len()
    )])
}

#[cfg(test)]
mod tests;
