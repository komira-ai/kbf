//! `pkg-macos-asset --package <pkg> --commit <sha> --binary-tarball <file> --expect-ref
//! refs/heads/<branch> [--gh <program>] (--out <dir> | --verify-only)`: the Mac `.pkg` of
//! `kbf-installer` ([`crate::pkg`]) as a release asset,
//! `kbf-installer-<commit>-darwin-arm64.pkg`, and the checks that tie it to the bytes the
//! attestation covers. It is [`crate::pkg_asset`] for the other package format.
//!
//! The package is opened ([`crate::pkg::open`]) and refused unless it holds exactly these
//! files, and the directories above them, and nothing else (no script either):
//!
//! - `usr/local/kbf-installer/bin/kbf-installer` (0755), byte for byte the binary inside
//!   `--binary-tarball`, the tarball `build-commit check-asset` and `darwin-asset` have
//!   already shown names the commit and is signed: the package carries the bytes the
//!   build identified, not others;
//! - `usr/local/kbf-installer/trust/` with `gh` (0755, non-empty, and byte for byte
//!   `--gh`, the pinned program the job downloaded and checked, when given),
//!   `trusted_root.jsonl` (JSON lines) and `installer.conf`, exactly `SOURCE_REFS=` and
//!   `--expect-ref` ([`crate::pkg_trust::conf`]): a package built on `main` trusts `main`
//!   and one built on a feature branch trusts that branch, never the reverse;
//! - `Library/LaunchDaemons/com.komira.kbf-installer.plist` (0644) that passes
//!   [`crate::pkg::gate_problems`]: the job does not run before `installer.toml` exists;
//! - the state directory `private/var/db/kbf-installer` (0700) and
//!   `Library/Logs/kbf-installer` (0755), empty; every other directory 0755, so nothing on
//!   the way to the tool is writable by group or other;
//!
//! and its `PackageInfo` names the identifier [`crate::pkg::IDENTIFIER`] and a version
//! ending `+<the first 12 digits of the commit>`, what `kbf-installer --version` prints.
//!
//! Then, without `--verify-only`, it copies the package to `<out>/kbf-installer-<commit>-
//! darwin-arm64.pkg` and appends its line to `<out>/SHA256SUMS.darwin-arm64`
//! ([`crate::sums`]), refusing a package already listed. With `--verify-only` it makes the
//! checks on `--package`, which must already be named as the asset (the file the attest
//! job signed, downloaded again by the verify job) and writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::pkg::open::{Installed, members, payload};
use crate::pkg::{self, BINARY, LOG_DIR, PLIST, STATE_DIR, STATE_MODE, TRUST_DIR};
use crate::pkg_asset::{self, Mode};
use crate::pkg_linux::{NAME, TRUST_FILES};
use crate::{Error, build_commit, pkg_trust, sums};

/// The inputs of the checks.
pub struct Check<'a> {
    /// The `kbf-installer-<commit>-darwin-arm64.tar.gz` the package's binary must equal.
    pub binary_tarball: &'a Path,
    /// The branch the package must trust: `refs/heads/<branch>`.
    pub expect_ref: &'a str,
    /// The pinned `gh` the package's must equal, when the job has it.
    pub gh: Option<&'a Path>,
}

/// The file name of the asset.
pub fn file_name(commit: &str) -> String {
    format!("{NAME}-{commit}-{}.pkg", Platform::DarwinArm64.name())
}

/// The directories of the package: those above each file it installs, and the state and
/// log directories with theirs.
fn dirs() -> BTreeSet<String> {
    [BINARY, PLIST, STATE_DIR, LOG_DIR]
        .iter()
        .flat_map(|p| ancestors(p))
        .chain([STATE_DIR.to_owned(), LOG_DIR.to_owned()])
        .chain(ancestors(&format!("{TRUST_DIR}/x")))
        .collect()
}

/// The mode of the directory `path` of the package: [`STATE_DIR`] is closed to everyone
/// but root, the rest are 0755.
fn dir_mode(path: &str) -> u32 {
    if path == STATE_DIR { STATE_MODE } else { 0o755 }
}

/// The directories above `path`, outermost first.
fn ancestors(path: &str) -> Vec<String> {
    path.match_indices('/')
        .map(|(at, _)| path[..at].to_owned())
        .collect()
}

/// What is wrong with `pkg` (the bytes of the package) as the `kbf-installer` package
/// for `c` and `commit`: one line per problem, none when it meets every check above.
///
/// # Errors
/// A file `c` names cannot be read.
pub fn problems(pkg_bytes: &[u8], commit: &str, c: &Check) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    if !pkg_trust::ref_ok(c.expect_ref) {
        problems.push(format!(
            "--expect-ref {:?} is not refs/heads/<branch>",
            c.expect_ref
        ));
    }
    let parts = match members(pkg_bytes) {
        Ok(parts) => parts,
        Err(e) => return Ok(vec![e]),
    };
    let names: Vec<&str> = parts.keys().map(String::as_str).collect();
    if names != ["Bom", "PackageInfo", "Payload"] {
        problems.push(format!(
            "holds {names:?}, not exactly Bom, PackageInfo and Payload (no script runs at install)"
        ));
    }
    let info = parts
        .get("PackageInfo")
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    if !info.contains(&format!("identifier=\"{}\"", pkg::IDENTIFIER)) {
        problems.push(format!("PackageInfo does not name {}", pkg::IDENTIFIER));
    }
    // The `version` of `<pkg-info>`: not the XML declaration's, not `format-version`.
    let version = info
        .split_once("<pkg-info")
        .and_then(|(_, tag)| tag.split(" version=\"").nth(1))
        .and_then(|rest| rest.split('"').next());
    let ends = format!("+{}", commit.get(..12).unwrap_or(commit));
    if !version.is_some_and(|v| v.ends_with(&ends)) {
        problems.push(format!(
            "PackageInfo's version {:?} does not end {ends}: it is not the version of the binary of this commit",
            version.unwrap_or("(none)")
        ));
    }
    let files = match parts.get("Payload").map(|p| payload(p)) {
        Some(Ok(files)) => files,
        Some(Err(e)) => {
            problems.push(format!("Payload: {e}"));
            return Ok(problems);
        }
        None => return Ok(problems),
    };
    payload_problems(&files, c, &mut problems)?;
    Ok(problems)
}

/// The checks of the files and directories of the payload.
fn payload_problems(
    files: &BTreeMap<String, Installed>,
    c: &Check,
    problems: &mut Vec<String>,
) -> Result<(), Error> {
    let file = |path: &str| match files.get(path) {
        Some(Installed::File { mode, bytes }) => Some((*mode, bytes)),
        _ => None,
    };
    let mut wanted: Vec<String> = vec![BINARY.to_owned(), PLIST.to_owned()];
    wanted.extend(TRUST_FILES.iter().map(|(n, _)| format!("{TRUST_DIR}/{n}")));
    // The binary the attestation covers is the one in the package.
    let tarball = read(c.binary_tarball)?;
    let shipped = pkg_asset::members(&tarball)
        .ok()
        .and_then(|m| m.into_iter().find(|m| m.path == NAME));
    match (file(BINARY), shipped) {
        (None, _) => problems.push(format!("holds no {BINARY}")),
        (Some(_), None) => problems.push(format!("{} holds no {NAME}", c.binary_tarball.display())),
        (Some((mode, bytes)), Some(in_tarball)) => {
            if *bytes != in_tarball.bytes {
                problems.push(format!(
                    "{BINARY} (sha256:{}) is not the binary of {} (sha256:{})",
                    Sha256::of(bytes),
                    c.binary_tarball.display(),
                    Sha256::of(&in_tarball.bytes)
                ));
            }
            if mode != 0o755 {
                problems.push(format!("{BINARY} has mode {mode:04o}, not 0755"));
            }
        }
    }
    match file(PLIST) {
        None => problems.push(format!("holds no {PLIST}")),
        Some((mode, bytes)) => {
            if mode != 0o644 {
                problems.push(format!("{PLIST} has mode {mode:04o}, not 0644"));
            }
            problems.extend(
                pkg::gate_problems(&String::from_utf8_lossy(bytes))
                    .into_iter()
                    .map(|p| format!("{PLIST}: {p}")),
            );
        }
    }
    for (name, mode) in TRUST_FILES {
        let path = format!("{TRUST_DIR}/{name}");
        let Some((got, bytes)) = file(&path) else {
            problems.push(format!("holds no {path}"));
            continue;
        };
        if got != mode {
            problems.push(format!("{path} has mode {got:04o}, not {mode:04o}"));
        }
        match name {
            "installer.conf" => {
                let text = String::from_utf8_lossy(bytes);
                match pkg_trust::conf_ref(&text) {
                    Some(r) if r == c.expect_ref => {}
                    Some(r) => problems.push(format!(
                        "{path} trusts {r}, but this package is attested for {}",
                        c.expect_ref
                    )),
                    None => problems.push(format!(
                        "{path} is not exactly `SOURCE_REFS=refs/heads/<branch>` and a newline: {text:?}"
                    )),
                }
            }
            "trusted_root.jsonl" => problems.extend(
                pkg_trust::root_problems(bytes)
                    .into_iter()
                    .map(|p| format!("{path} {p}")),
            ),
            _ => {
                if bytes.is_empty() {
                    problems.push(format!("{path} is empty"));
                }
                if let Some(gh) = c.gh
                    && *bytes != read(gh)?
                {
                    problems.push(format!(
                        "{path} is not the pinned gh ({}) the job downloaded",
                        gh.display()
                    ));
                }
            }
        }
    }
    for (path, node) in files {
        match node {
            Installed::File { .. } if !wanted.contains(path) => {
                problems.push(format!("holds {path}, which the package does not install"));
            }
            Installed::Dir { .. } if !dirs().contains(path) => problems.push(format!(
                "holds the directory {path}, which the package does not install"
            )),
            Installed::Dir { mode } if *mode != dir_mode(path) => problems.push(format!(
                "{path} has mode {mode:04o}, not {:04o}",
                dir_mode(path)
            )),
            Installed::Dir { .. } | Installed::File { .. } => {}
        }
    }
    for dir in [STATE_DIR, LOG_DIR] {
        let held = files.keys().any(|p| p.starts_with(&format!("{dir}/")));
        if held {
            problems.push(format!("{dir} is to be empty: setup and the job fill it"));
        }
        if !matches!(files.get(dir), Some(Installed::Dir { .. })) {
            problems.push(format!("holds no directory {dir}"));
        }
    }
    Ok(())
}

/// The `pkg-macos-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the package is refused, or a file cannot be read or written.
pub fn asset(pkg_path: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let name = file_name(commit);
    let bytes = read(pkg_path)?;
    let found = problems(&bytes, commit, c)?;
    if !found.is_empty() {
        return Err(Error::Refused(
            found
                .into_iter()
                .map(|p| format!("{}: {p}", pkg_path.display()))
                .collect(),
        ));
    }
    let sha = Sha256::of(&bytes);
    match mode {
        Mode::VerifyOnly => {
            if pkg_path.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
                return Err(Error::Refused(vec![format!(
                    "{} is not named {name}: --verify-only checks the asset itself",
                    pkg_path.display()
                )]));
            }
            Ok(vec![format!(
                "{name}: a package of the binary in {}, trusting {}",
                c.binary_tarball.display(),
                c.expect_ref
            )])
        }
        Mode::Add(out) => {
            let (sums_path, mut lines) = open_sums(out, Platform::DarwinArm64)?;
            if let Some(l) = lines.iter().find(|l| l.name == name) {
                return Err(Error::Failed(format!(
                    "{} already lists {}",
                    sums_path.display(),
                    l.name
                )));
            }
            write_new(&out.join(&name), &bytes)?;
            lines.push(sums::Line {
                sha256: sha,
                name: name.clone(),
            });
            let text = write_sums(&sums_path, &lines)?;
            let mut printed = vec![format!("packaged {name}:")];
            printed.extend(text.lines().map(str::to_owned));
            Ok(printed)
        }
    }
}

#[cfg(test)]
mod tests;
