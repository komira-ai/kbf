//! `pkg-asset --package <deb> --platform <platform> --commit <sha> --binary-tarball <file>
//! --expect-ref refs/heads/<branch> [--gh <program>] (--out <dir> | --verify-only)`: the
//! `kbf-installer` deb as a release asset, `kbf-installer-<commit>-<platform>.deb`, and
//! the checks that tie it to the bytes the attestation covers.
//!
//! The deb is opened (an `ar` archive of `control.tar.gz` and `data.tar.gz`, as nfpm
//! writes it with gzip) and refused unless:
//!
//! - its control file says the architecture of `--platform` (`amd64` or `arm64`);
//! - `usr/bin/kbf-installer` is byte for byte the binary inside `--binary-tarball`, the
//!   tarball `build-commit check-asset` has already shown names the commit: the deb
//!   carries the bytes the build identified, not others;
//! - `usr/share/kbf-installer/trust/installer.conf` is exactly `SOURCE_REFS=` and
//!   `--expect-ref` ([`crate::pkg_trust::conf`]): a deb built on `main` trusts `main`
//!   and a deb built on a feature branch trusts that branch, never the reverse;
//! - the trust directory holds `gh` (mode 0755, and byte for byte `--gh`, the pinned
//!   program the job downloaded and checked, when given), `installer.conf` and a
//!   `trusted_root.jsonl` that is JSON lines ([`crate::pkg_trust::root_problems`]),
//!   with the modes of [`crate::pkg_linux::TRUST_FILES`].
//!
//! Then, without `--verify-only`, it copies the deb to `<out>/kbf-installer-<commit>-
//! <platform>.deb` and appends its line to `<out>/SHA256SUMS.<platform>`
//! ([`crate::sums`]), refusing a deb already listed. With `--verify-only` it makes the
//! checks on `--package`, which must already be named as the asset (the file the attest
//! job signed, downloaded again by the verify job) and writes nothing.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::pkg_linux::{self, TRUST_DIR, TRUST_FILES};
use crate::{Error, build_commit, pkg_trust, sums};

/// The binary's path in the deb, and in the data tarball (no leading slash).
const BINARY: &str = "usr/bin/kbf-installer";

/// The package file `package` names: the file itself, or, for a directory, the one file
/// in it whose name ends `.<ext>` (what nfpm left in `pkg-build --out`, under a name made
/// of the version, which a workflow step would need a pipe to spell).
///
/// # Errors
/// A directory with none or several such files, or a path that is neither.
pub fn sole(package: &Path, ext: &str) -> Result<PathBuf, Error> {
    if !package.is_dir() {
        return Ok(package.to_owned());
    }
    let suffix = format!(".{ext}");
    let mut found: Vec<PathBuf> = std::fs::read_dir(package)
        .map_err(Error::spawn(format!("read {}", package.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.to_string_lossy().ends_with(&suffix))
        .collect();
    match (found.pop(), found.is_empty()) {
        (Some(only), true) => Ok(only),
        _ => Err(Error::Failed(format!(
            "expected one {suffix} file in {}",
            package.display()
        ))),
    }
}

/// The package version of `binary`: the `<version>+<12 of the commit>` its `--version`
/// prints after its name (`kbf-installer 0.1.0+0123456789ab`), the version `pkg-config`
/// gives the package so that the package, the binary and the commit name each other.
///
/// # Errors
/// The binary does not run, or prints anything else.
pub fn binary_version(binary: &Path) -> Result<String, Error> {
    let out = pkg_linux::spawn(std::process::Command::new(binary).arg("--version"))?;
    version_of(out.ok, &out.stdout).ok_or_else(|| {
        Error::Failed(format!(
            "{} --version printed {:?}, not `{} <version>+<commit>`",
            binary.display(),
            out.stdout.trim(),
            pkg_linux::NAME
        ))
    })
}

/// The version in the `--version` output `stdout` of a program that exited with success
/// `ok`: the second word of exactly `kbf-installer <version>+<commit>`.
fn version_of(ok: bool, stdout: &str) -> Option<String> {
    let words: Vec<&str> = stdout.split_whitespace().collect();
    match (ok, words.as_slice()) {
        (true, [name, version]) if *name == pkg_linux::NAME && version.contains('+') => {
            Some((*version).to_owned())
        }
        _ => None,
    }
}

/// The file name of the asset.
pub fn file_name(commit: &str, platform: Platform) -> String {
    format!("{}-{commit}-{}.deb", pkg_linux::NAME, platform.name())
}

/// The `Architecture` a deb for `platform` carries.
fn architecture(platform: Platform) -> &'static str {
    match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
    }
}

/// One regular file of a tar: its path without a leading `./` or `/`, its mode, bytes.
pub(crate) struct Member {
    pub(crate) path: String,
    pub(crate) mode: u32,
    pub(crate) bytes: Vec<u8>,
}

/// What a tar reader's error says of the archive.
fn not_a_tar(e: std::io::Error) -> String {
    format!("not a tar: {e}")
}

/// The regular files of the gzip-compressed tar `tgz`.
pub(crate) fn members(tgz: &[u8]) -> Result<Vec<Member>, String> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .read_to_end(&mut tar)
        .map_err(|e| format!("not a gzip stream: {e}"))?;
    let mut archive = tar::Archive::new(&tar[..]);
    let mut found = Vec::new();
    for entry in archive.entries().map_err(not_a_tar)? {
        let mut entry = entry.map_err(not_a_tar)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let mut bytes = Vec::new();
        let mode = entry
            .header()
            .mode()
            .and_then(|mode| entry.read_to_end(&mut bytes).map(|_| mode))
            .map_err(|e| format!("{path}: {e}"))?;
        let path = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_owned();
        found.push(Member {
            path,
            mode: mode & 0o777,
            bytes,
        });
    }
    Ok(found)
}

/// The members of an `ar` archive (a deb): name without the trailing `/`, and bytes.
fn ar_members(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let rest = bytes
        .strip_prefix(b"!<arch>\n")
        .ok_or("not an ar archive (a deb starts `!<arch>`)")?;
    let mut rest = rest;
    let mut found = Vec::new();
    while !rest.is_empty() {
        let (header, after) = rest
            .split_at_checked(60)
            .ok_or("a truncated ar member header")?;
        if &header[58..] != b"`\n" {
            return Err("an ar member header without its terminator".to_owned());
        }
        let name = String::from_utf8_lossy(&header[..16]).trim_end().to_owned();
        let size: usize = String::from_utf8_lossy(&header[48..58])
            .trim_end()
            .parse()
            .map_err(|_| format!("{name}: an ar member with no size"))?;
        let (body, after) = after
            .split_at_checked(size)
            .ok_or_else(|| format!("{name}: an ar member cut short"))?;
        found.push((name.trim_end_matches('/').to_owned(), body.to_vec()));
        // Members are padded to an even offset.
        rest = after
            .strip_prefix(b"\n")
            .filter(|_| size % 2 == 1)
            .unwrap_or(after);
    }
    Ok(found)
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub platform: Platform,
    /// The `kbf-installer-<commit>-<platform>.tar.gz` the deb's binary must equal.
    pub binary_tarball: &'a Path,
    /// The branch the deb must trust: `refs/heads/<branch>`.
    pub expect_ref: &'a str,
    /// The pinned `gh` the deb's must equal, when the job has it.
    pub gh: Option<&'a Path>,
}

/// What is wrong with `deb` (the bytes of the package) as the `kbf-installer` package
/// for `c`: one line per problem, none when it meets every check above.
///
/// # Errors
/// A file `c` names cannot be read.
pub fn problems(deb: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    if !pkg_trust::ref_ok(c.expect_ref) {
        problems.push(format!(
            "--expect-ref {:?} is not refs/heads/<branch>",
            c.expect_ref
        ));
    }
    let parts = match ar_members(deb) {
        Ok(parts) => parts,
        Err(e) => return Ok(vec![e]),
    };
    let part = |name: &str| parts.iter().find(|(n, _)| n == name).map(|(_, b)| b);
    let Some(control) = part("control.tar.gz") else {
        problems.push("has no control.tar.gz (nfpm is configured for gzip)".to_owned());
        return Ok(problems);
    };
    let Some(data) = part("data.tar.gz") else {
        problems.push("has no data.tar.gz (nfpm is configured for gzip)".to_owned());
        return Ok(problems);
    };
    match members(control) {
        Err(e) => problems.push(format!("control.tar.gz: {e}")),
        Ok(files) => {
            let text = files
                .iter()
                .find(|m| m.path == "control")
                .map(|m| String::from_utf8_lossy(&m.bytes).into_owned())
                .unwrap_or_default();
            let want = architecture(c.platform);
            let got = text.lines().find_map(|l| l.strip_prefix("Architecture: "));
            if got != Some(want) {
                problems.push(format!(
                    "control says Architecture {:?}, not {want:?} for {}",
                    got.unwrap_or("(none)"),
                    c.platform.name()
                ));
            }
        }
    }
    let files = match members(data) {
        Ok(files) => files,
        Err(e) => {
            problems.push(format!("data.tar.gz: {e}"));
            return Ok(problems);
        }
    };
    let file = |path: &str| files.iter().find(|m| m.path == path);
    // The binary the attestation covers is the one in the package.
    let tarball = read(c.binary_tarball)?;
    let shipped = members(&tarball)
        .ok()
        .and_then(|m| m.into_iter().find(|m| m.path == pkg_linux::NAME));
    match (file(BINARY), shipped) {
        (None, _) => problems.push(format!("holds no {BINARY}")),
        (Some(_), None) => problems.push(format!(
            "{} holds no {}",
            c.binary_tarball.display(),
            pkg_linux::NAME
        )),
        (Some(in_deb), Some(in_tarball)) => {
            if in_deb.bytes != in_tarball.bytes {
                problems.push(format!(
                    "{BINARY} (sha256:{}) is not the binary of {} (sha256:{})",
                    Sha256::of(&in_deb.bytes),
                    c.binary_tarball.display(),
                    Sha256::of(&in_tarball.bytes)
                ));
            }
            if in_deb.mode != 0o755 {
                problems.push(format!("{BINARY} has mode {:04o}, not 0755", in_deb.mode));
            }
        }
    }
    let dir = TRUST_DIR.trim_start_matches('/');
    for (name, mode) in TRUST_FILES {
        let path = format!("{dir}/{name}");
        let Some(m) = file(&path) else {
            problems.push(format!("holds no {path}"));
            continue;
        };
        if m.mode != mode {
            problems.push(format!("{path} has mode {:04o}, not {mode:04o}", m.mode));
        }
        match name {
            "installer.conf" => {
                let text = String::from_utf8_lossy(&m.bytes);
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
            "trusted_root.jsonl" => {
                problems.extend(
                    pkg_trust::root_problems(&m.bytes)
                        .into_iter()
                        .map(|p| format!("{path} {p}")),
                );
            }
            _ => {
                if m.bytes.is_empty() {
                    problems.push(format!("{path} is empty"));
                }
                if let Some(gh) = c.gh
                    && m.bytes != read(gh)?
                {
                    problems.push(format!(
                        "{path} is not the pinned gh ({}) the job downloaded",
                        gh.display()
                    ));
                }
            }
        }
    }
    Ok(problems)
}

/// Where the verb is told to put the asset, or that it only checks.
pub enum Mode<'a> {
    /// Copy to `<out>/<asset name>` and list it.
    Add(&'a Path),
    /// Check `--package`, which must be the asset already.
    VerifyOnly,
}

/// The `pkg-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the deb is refused, or a file cannot be read or written.
pub fn asset(deb: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    if !matches!(c.platform, Platform::LinuxX86_64 | Platform::LinuxArm64) {
        return Err(Error::Usage(format!(
            "--platform {}: the package is for Linux",
            c.platform.name()
        )));
    }
    let name = file_name(commit, c.platform);
    let bytes = read(deb)?;
    let found = problems(&bytes, c)?;
    let refused = |found: Vec<String>| {
        Error::Refused(
            found
                .into_iter()
                .map(|p| format!("{}: {p}", deb.display()))
                .collect(),
        )
    };
    if !found.is_empty() {
        return Err(refused(found));
    }
    let sha = Sha256::of(&bytes);
    match mode {
        Mode::VerifyOnly => {
            if deb.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
                return Err(Error::Refused(vec![format!(
                    "{} is not named {name}: --verify-only checks the asset itself",
                    deb.display()
                )]));
            }
            Ok(vec![format!(
                "{name}: a {} package of the binary in {}, trusting {}",
                c.platform.name(),
                c.binary_tarball.display(),
                c.expect_ref
            )])
        }
        Mode::Add(out) => {
            let (sums_path, mut lines) = open_sums(out, c.platform)?;
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
