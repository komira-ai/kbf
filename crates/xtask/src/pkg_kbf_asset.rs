//! `pkg-kbf-asset --package <deb> --platform <platform> --commit <sha> --kbf-tarball <file>
//! (--out <dir> | --verify-only)`: the `kbf` deb as a release asset,
//! `kbf-<commit>-<platform>.deb`, and the checks that tie it to the bytes the attestation
//! covers. (The `kbf-installer` deb is [`crate::pkg_asset`]'s.)
//!
//! The deb is opened as [`crate::pkg_asset`] opens one and refused unless:
//!
//! - its control file says the architecture of `--platform`, the package `kbf`, and a
//!   dependency on `kbf-installer (>= <its own version>)`, because each mode unit runs the
//!   installer's `boot-check` before it starts `kbf`;
//! - `usr/bin/kbf` is byte for byte the `kbf` inside `--kbf-tarball`, the tarball
//!   `build-commit check-asset` has already shown names the commit: the deb carries the
//!   bytes the build identified, not others. It is mode 0755;
//! - it holds the unit of every mode ([`crate::pkg_linux::KBF_UNITS`]) in
//!   `usr/lib/systemd/system`, mode 0644, and no other file.
//!
//! Then, without `--verify-only`, it copies the deb to `<out>/kbf-<commit>-<platform>.deb`
//! and appends its line to `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), refusing a deb
//! already listed. With `--verify-only` it makes the checks on `--package`, which must
//! already be named as the asset (the file the attest job signed, downloaded again by the
//! verify job) and writes nothing.

use std::path::Path;

use crate::Error;
use crate::package::Platform;
use crate::pkg_asset::{self, Mode};
use crate::pkg_linux::{self, KBF_NAME, KBF_PATH, KBF_UNITS, UNIT_DIR};

/// The file name of the asset.
pub fn file_name(commit: &str, platform: Platform) -> String {
    format!("{KBF_NAME}-{commit}-{}.deb", platform.name())
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub platform: Platform,
    /// The `kbf-<commit>-<platform>.tar.gz` the deb's `kbf` must equal.
    pub kbf_tarball: &'a Path,
}

/// The value of the control field that starts a line with `name` (such as `Depends: `).
fn field<'a>(control: &'a str, name: &str) -> Option<&'a str> {
    control.lines().find_map(|l| l.strip_prefix(name))
}

/// `text` without its white space: two spellings of a dependency differ in spaces only.
fn squashed(text: &str) -> String {
    text.split_whitespace().collect()
}

/// The path of the unit `unit` in a data tarball (no leading slash).
fn unit_path(unit: &str) -> String {
    format!("{}/{unit}", UNIT_DIR.trim_start_matches('/'))
}

/// What is wrong with `deb` (the bytes of the package) as the `kbf` package for `c`: one
/// line per problem, none when it meets every check above.
///
/// # Errors
/// The tarball `c` names cannot be read.
pub fn problems(deb: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    let Some(opened) = pkg_asset::open(deb, c.platform, &mut problems) else {
        return Ok(problems);
    };
    let package = field(&opened.control, "Package: ");
    if package != Some(KBF_NAME) {
        problems.push(format!(
            "control says Package {:?}, not {KBF_NAME:?}",
            package.unwrap_or("(none)")
        ));
    }
    let version = field(&opened.control, "Version: ").unwrap_or_default();
    let depends = field(&opened.control, "Depends: ").unwrap_or_default();
    let want = pkg_linux::kbf_depends_deb(version);
    if !depends.split(',').any(|d| squashed(d) == squashed(&want)) {
        problems.push(format!(
            "control says Depends {depends:?}, which does not hold {want:?}"
        ));
    }
    let path = KBF_PATH.trim_start_matches('/');
    let binary = pkg_asset::binary_problems(opened.file(path), path, KBF_NAME, c.kbf_tarball)?;
    problems.extend(binary);
    for unit in KBF_UNITS {
        let path = unit_path(unit);
        let Some(m) = opened.file(&path) else {
            problems.push(format!("holds no {path}"));
            continue;
        };
        if m.mode != 0o644 {
            problems.push(format!("{path} has mode {:04o}, not 0644", m.mode));
        }
    }
    let shipped = pkg_asset::kbf_package_paths();
    for m in &opened.files {
        if !shipped.contains(&m.path) {
            problems.push(format!(
                "holds {}, which the kbf package does not ship",
                m.path
            ));
        }
    }
    Ok(problems)
}

/// The `pkg-kbf-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the deb is refused, or a file cannot be read or written.
pub fn asset(deb: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    let said = format!(
        "a {} package of the kbf in {}",
        c.platform.name(),
        c.kbf_tarball.display()
    );
    let name = file_name(commit, c.platform);
    let found = |bytes: &[u8]| problems(bytes, c);
    pkg_asset::publish(deb, (commit, c.platform), &name, mode, found, &said)
}

#[cfg(test)]
mod tests;
