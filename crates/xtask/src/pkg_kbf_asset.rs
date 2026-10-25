//! `pkg-kbf-asset --package <deb> --platform <platform> --commit <sha> --kbf-tarball <file>
//! --expect-ref refs/heads/<branch> [--gh <program>] (--out <dir> | --verify-only)`: the `kbf`
//! deb as a release asset, `kbf-<commit>-<platform>.deb`, and the checks that tie it to the
//! bytes the attestation covers.
//!
//! The deb is opened ([`crate::pkg_asset::open`]) and refused unless:
//!
//! - its control file says the architecture of `--platform`, the package `kbf`, the
//!   dependencies and suggestions of [`crate::pkg_linux`] and none on `kbf-installer`, and
//!   that it `Replaces` and `Breaks` the `kbf-installer` packages older than the first
//!   version that has none ([`KBF_DEB_REPLACES`], [`KBF_DEB_BREAKS`]): the units, `kbf.slice`,
//!   the sysusers and tmpfiles files, the needrestart drop-in and the trust files were files
//!   of the installer's deb, and dpkg refuses to unpack a package over files another package
//!   owns, and keeps both packages' files apart, unless the control file says so;
//! - `usr/bin/kbf` is byte for byte the `kbf` inside `--kbf-tarball`, the tarball
//!   `build-commit check-asset` has already shown names the commit: the deb carries the
//!   bytes the build identified, not others. It is mode 0755;
//! - it holds the unit of every mode ([`crate::pkg_linux::KBF_UNITS`]) and `kbf.slice` in
//!   `usr/lib/systemd/system`, the sysusers.d and tmpfiles.d files and the needrestart
//!   drop-in, all mode 0644;
//! - its trust directory `usr/share/kbf/trust` holds the pinned `gh` (mode 0755, and byte for
//!   byte `--gh`, the program the job downloaded and checked by SHA-256, when given), the
//!   Sigstore trusted root and an `installer.conf` that is exactly `SOURCE_REFS=` and
//!   `--expect-ref` ([`crate::pkg_asset::trust_problems`]): a deb built on `main` trusts
//!   `main` and a deb built on a feature branch trusts that branch, never the reverse;
//! - it holds no other file.
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
use crate::pkg_linux::{
    DEB_DEPENDS, DEB_SUGGESTS, KBF_DEB_BREAKS, KBF_DEB_REPLACES, KBF_NAME, KBF_PATH, KBF_UNITS,
    NAME, NEEDRESTART_PATH, SLICE, SYSUSERS_PATH, TMPFILES_PATH, TRUST_DIR, TRUST_FILES, UNIT_DIR,
};

/// The file name of the asset.
pub fn file_name(commit: &str, platform: Platform) -> String {
    format!("{KBF_NAME}-{commit}-{}.deb", platform.name())
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub platform: Platform,
    /// The `kbf-<commit>-<platform>.tar.gz` the deb's `kbf` must equal.
    pub kbf_tarball: &'a Path,
    /// The branch the deb must trust: `refs/heads/<branch>`.
    pub expect_ref: &'a str,
    /// The pinned `gh` the deb's must equal, when the job has it.
    pub gh: Option<&'a Path>,
}

/// The value of the control field that starts a line with `name` (such as `Depends: `).
fn field<'a>(control: &'a str, name: &str) -> Option<&'a str> {
    control.lines().find_map(|l| l.strip_prefix(name))
}

/// `text` without its white space: two spellings of a dependency differ in spaces only.
fn squashed(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Whether the comma-separated control field value `list` holds the entry `want`.
fn holds(list: &str, want: &str) -> bool {
    list.split(',').any(|d| squashed(d) == squashed(want))
}

/// Whether the control field value `list` has an entry that names the package `package`,
/// whatever version or architecture the entry asks for (`pkg (>= 1)`, `pkg:any`) and whether
/// it is one of several alternatives (`other | pkg`).
fn names(list: &str, package: &str) -> bool {
    list.split([',', '|'])
        .any(|entry| entry.trim().split(['(', ':', ' ']).next() == Some(package))
}

/// The path of the file `dst` of the package in a data tarball (no leading slash).
fn member_path(dst: &str) -> String {
    dst.trim_start_matches('/').to_owned()
}

/// The paths of the trust files, as a data tarball names them.
fn trust_paths() -> Vec<String> {
    TRUST_FILES
        .map(|(n, _)| member_path(&format!("{TRUST_DIR}/{n}")))
        .to_vec()
}

/// The regular files of the package other than `usr/bin/kbf` and the trust files (which
/// [`pkg_asset::trust_problems`] holds to their modes and contents), as a data tarball names
/// them, each with the mode it has: the units, the slice, and the sysusers, tmpfiles and
/// needrestart files.
fn other_files() -> Vec<(String, u32)> {
    let units = KBF_UNITS.into_iter().chain([SLICE]);
    let units = units.map(|u| (member_path(&format!("{UNIT_DIR}/{u}")), 0o644));
    let single = [SYSUSERS_PATH, TMPFILES_PATH, NEEDRESTART_PATH].map(|p| (member_path(p), 0o644));
    units.chain(single).collect()
}

/// What is wrong with `deb` (the bytes of the package) as the `kbf` package for `c`: one
/// line per problem, none when it meets every check above.
///
/// # Errors
/// A file `c` names cannot be read.
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
    let depends = field(&opened.control, "Depends: ").unwrap_or_default();
    if names(depends, NAME) {
        problems.push(format!(
            "control says Depends {depends:?}, but the kbf package depends on no {NAME:?}"
        ));
    }
    for want in DEB_DEPENDS {
        if !holds(depends, want) {
            problems.push(format!(
                "control says Depends {depends:?}, which does not hold {want:?}"
            ));
        }
    }
    let suggests = field(&opened.control, "Suggests: ").unwrap_or_default();
    for want in DEB_SUGGESTS {
        if !holds(suggests, want) {
            problems.push(format!(
                "control says Suggests {suggests:?}, which does not hold {want:?}"
            ));
        }
    }
    for (name, wanted) in [("Replaces", KBF_DEB_REPLACES), ("Breaks", KBF_DEB_BREAKS)] {
        let got = field(&opened.control, &format!("{name}: ")).unwrap_or_default();
        for want in wanted {
            if !holds(got, want) {
                problems.push(format!(
                    "control says {name} {got:?}, which does not hold {want:?}"
                ));
            }
        }
    }
    let path = KBF_PATH.trim_start_matches('/');
    let binary = pkg_asset::binary_problems(opened.file(path), path, KBF_NAME, c.kbf_tarball)?;
    problems.extend(binary);
    let others = other_files();
    for (path, mode) in &others {
        match opened.file(path) {
            None => problems.push(format!("holds no {path}")),
            Some(m) if m.mode != *mode => {
                problems.push(format!("{path} has mode {:04o}, not {mode:04o}", m.mode));
            }
            Some(_) => {}
        }
    }
    problems.extend(pkg_asset::trust_problems(&opened, c.expect_ref, c.gh)?);
    let trust = trust_paths();
    for m in &opened.files {
        if m.path != path && !trust.contains(&m.path) && !others.iter().any(|(p, _)| *p == m.path) {
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
        "a {} package of the kbf in {}, trusting {}",
        c.platform.name(),
        c.kbf_tarball.display(),
        c.expect_ref
    );
    let name = file_name(commit, c.platform);
    let found = |bytes: &[u8]| problems(bytes, c);
    pkg_asset::publish(deb, (commit, c.platform), &name, mode, found, &said)
}

#[cfg(test)]
mod tests;
