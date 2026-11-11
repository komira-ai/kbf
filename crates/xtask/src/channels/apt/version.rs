//! `channels apt version --debs <dir> --commit <sha> --output <file>`: the version the release's
//! two packages carry, read from their own control files, and written as `package-version=<v>`.
//!
//! The release job `verify` runs it, so the later jobs name the version they publish and check
//! from what the bytes say. The packages must be `kbf-native-<commit>-<platform>.deb` for both
//! platforms, each saying `Package: kbf`, the Debian architecture of its platform, and the same
//! `<major>.<minor>.<patch>` as `Version`. Nothing is written unless every one passes.

use std::path::Path;

use super::state::{PACKAGE, PLATFORMS, asset_name, is_commit};
use super::{control, deb};
use crate::channels::{Error, append_output, is_plain_version};

/// The `apt version` verb: the lines to print.
///
/// # Errors
/// A commit that is not 40 hex digits, a package that is missing or is not one, or one whose
/// control file names another package or architecture, or a version that is not
/// `<major>.<minor>.<patch>`, or two packages that carry different versions.
pub fn version(debs: &Path, commit: &str, output: &Path) -> Result<Vec<String>, Error> {
    if !is_commit(commit) {
        return Err(Error::Usage(format!(
            "--commit {commit:?} is not a full commit (40 lower case hex digits)"
        )));
    }
    let mut versions = Vec::new();
    let mut problems = Vec::new();
    for (arch, platform) in PLATFORMS {
        let name = asset_name(commit, platform);
        let path = debs.join(&name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                problems.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let fields = match deb::control_of(&bytes).and_then(|text| control::parse(&text)) {
            Ok(fields) => fields,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        let said = |field: &str| control::get(&fields, field).unwrap_or_default().to_owned();
        let (package, architecture, version) =
            (said("Package"), said("Architecture"), said("Version"));
        if package != PACKAGE {
            problems.push(format!("{name}: Package is {package:?}, not {PACKAGE:?}"));
        }
        if architecture != arch {
            problems.push(format!(
                "{name}: Architecture is {architecture:?}, not {arch:?}"
            ));
        }
        if is_plain_version(&version) {
            versions.push((name, version));
        } else {
            problems.push(format!(
                "{name}: Version {version:?} is not <major>.<minor>.<patch>"
            ));
        }
    }
    if let Some((first_name, first_version)) = versions.first() {
        for (name, version) in versions.iter().skip(1) {
            if version != first_version {
                problems.push(format!(
                    "{name} is version {version}, but {first_name} is version {first_version}"
                ));
            }
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let version = versions
        .first()
        .map(|(_, version)| version.clone())
        .unwrap_or_default();
    append_output(output, &[format!("package-version={version}")])?;
    Ok(vec![format!("apt version: {version}")])
}

#[cfg(test)]
mod tests;
