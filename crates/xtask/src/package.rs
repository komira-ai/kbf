//! `package --binary <path> --sbom <path> --platform <platform> --commit <sha> --out <dir>`:
//! one kbf binary as a release asset (docs/artifacts.md).
//!
//! It writes, in `<out>`:
//!
//! - `<name>-<commit>-<platform>.tar.gz`: the binary alone, named `<name>` (the file
//!   name of `--binary`), the exact bytes given (on macOS, the signed binary);
//! - `<name>-<commit>-<platform>.cdx.json`: a copy of `--sbom`;
//!
//! and appends both files' lines to `<out>/SHA256SUMS.<platform>` ([`crate::sums`]).
//! Neither asset may exist already, and the sums file may not list either yet.
//!
//! The tarball is the same bytes for the same binary on every runner: one ustar entry
//! with mode 0755, owner and group 0 and no owner names (so an extraction as root leaves
//! the binary owned by root, not by the CI runner's uid), modification time 0 (the
//! epoch), no extended attributes, in a gzip stream with no file name and time 0
//! ([`tarball`]). After writing it, `package` reads the file back and refuses it unless
//! it holds exactly that one entry ([`check_tarball`]).

use std::io::{Read, Write};
use std::path::Path;

use flate2::Compression;

use crate::fetch::Sha256;
use crate::{Error, build_commit, sums};

/// The mode of the binary in the tarball.
pub const MODE: u32 = 0o755;

/// The modification time of the binary in the tarball, and of the gzip stream: the
/// epoch, so the bytes do not depend on when or where the binary was built.
pub const MTIME: u32 = 0;

/// A platform an asset is built for: the last part of its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Platform {
    #[value(name = "darwin-arm64")]
    DarwinArm64,
    #[value(name = "linux-x86_64")]
    LinuxX86_64,
    #[value(name = "linux-arm64")]
    LinuxArm64,
}

impl Platform {
    /// Every platform, in the order docs/artifacts.md lists them.
    pub const ALL: [Platform; 3] = [
        Platform::DarwinArm64,
        Platform::LinuxX86_64,
        Platform::LinuxArm64,
    ];

    /// The name assets and sums files use.
    pub fn name(self) -> &'static str {
        match self {
            Platform::DarwinArm64 => "darwin-arm64",
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::LinuxArm64 => "linux-arm64",
        }
    }

    /// The platform named `name`, if any.
    pub fn named(name: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// The gzip-compressed tar holding `bytes` as the one file `name`, mode [`MODE`], owner
/// and group 0, time [`MTIME`].
///
/// # Errors
/// `name` does not fit a ustar header.
pub fn tarball(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut header = tar::Header::new_ustar();
    header
        .set_path(name)
        .map_err(|e| format!("{name:?} cannot be a tar entry name: {e}"))?;
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_mode(MODE);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(u64::from(MTIME));
    header.set_cksum();
    let gzip = flate2::GzBuilder::new()
        .mtime(MTIME)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    // Writing to memory cannot fail.
    tar.append(&header, bytes).expect("tar into memory");
    let gzip = tar.into_inner().expect("tar into memory");
    Ok(gzip.finish().expect("gzip into memory"))
}

/// What is wrong with `tgz` as the tarball of the binary `name` holding `bytes`: one
/// line per problem, none when it is exactly what [`tarball`] writes for them.
pub fn check_tarball(tgz: &[u8], name: &str, bytes: &[u8]) -> Vec<String> {
    let mut tar = Vec::new();
    if let Err(e) = flate2::read::GzDecoder::new(tgz).read_to_end(&mut tar) {
        return vec![format!("not a gzip stream: {e}")];
    }
    let mut problems = Vec::new();
    let mut entries = 0;
    let mut archive = tar::Archive::new(&tar[..]);
    // Only an archive already read from has no entries to list.
    let all = archive.entries().expect("a fresh archive");
    for entry in all {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!("not a tar: {e}"));
                break;
            }
        };
        entries += 1;
        let h = entry.header();
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let mut wrong = |what: &str, got: String| {
            problems.push(format!("{path}: {what} is {got}"));
        };
        if path != name {
            wrong("the name", format!("not {name:?}"));
        }
        if h.entry_type() != tar::EntryType::Regular {
            wrong(
                "the type",
                format!("{:?}, not a regular file", h.entry_type()),
            );
        }
        let unreadable = || "unreadable".to_owned();
        let fields = [
            (
                "the mode",
                h.mode().map_or_else(|_| unreadable(), |m| format!("{m:o}")),
                "755",
            ),
            (
                "the owner",
                h.uid().map_or_else(|_| unreadable(), |v| v.to_string()),
                "0",
            ),
            (
                "the group",
                h.gid().map_or_else(|_| unreadable(), |v| v.to_string()),
                "0",
            ),
            (
                "the time",
                h.mtime().map_or_else(|_| unreadable(), |v| v.to_string()),
                "0",
            ),
        ];
        for (what, got, want) in fields {
            if got != want {
                wrong(what, format!("{got}, not {want}"));
            }
        }
        let names = (h.username_bytes(), h.groupname_bytes());
        if names != (Some(&[][..]), Some(&[][..])) {
            wrong("the owner name", "not empty".to_owned());
        }
        let mut held = Vec::new();
        let read = entry.read_to_end(&mut held);
        if read.is_err() | (held != bytes) {
            wrong("the content", "not the binary's bytes".to_owned());
        }
    }
    if entries != 1 {
        problems.push(format!("{entries} entries, not 1"));
    }
    problems
}

/// Reads `path`, with the path in the error.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(Error::spawn(format!("read {}", path.display())))
}

/// Writes `bytes` to the new file `path`; a file already there is an error.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::File::create_new(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(Error::spawn(format!("write {}", path.display())))
}

/// Writes the tarball `tgz` to the new file `path`, then reads it back as a node would
/// and refuses it unless [`check_tarball`] finds it holds exactly the binary `name`
/// with `bytes`.
pub fn write_tarball(path: &Path, tgz: &[u8], name: &str, bytes: &[u8]) -> Result<(), Error> {
    write_new(path, tgz)?;
    let problems = check_tarball(&read(path)?, name, bytes);
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(
        problems
            .into_iter()
            .map(|p| format!("{}: {p}", path.display()))
            .collect(),
    ))
}

/// Creates `out` when missing and reads its `SHA256SUMS.<platform>`: its path and the
/// lines already there (none when the file is missing).
pub(crate) fn open_sums(
    out: &Path,
    platform: Platform,
) -> Result<(std::path::PathBuf, Vec<sums::Line>), Error> {
    std::fs::create_dir_all(out).map_err(Error::spawn(format!("create {}", out.display())))?;
    let path = out.join(format!("{}.{}", sums::SUMS, platform.name()));
    let lines = match std::fs::read_to_string(&path) {
        Ok(text) => sums::parse(&text).map_err(Error::Refused)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::spawn(format!("read {}", path.display()))(e)),
    };
    Ok((path, lines))
}

/// Writes `lines` to the sums file `path` and returns the text written.
pub(crate) fn write_sums(path: &Path, lines: &[sums::Line]) -> Result<String, Error> {
    let text = sums::render(lines);
    std::fs::write(path, &text).map_err(Error::spawn(format!("write {}", path.display())))?;
    Ok(text)
}

/// The `package` verb: the lines to print.
pub fn package(
    binary: &Path,
    sbom: &Path,
    platform: Platform,
    commit: &str,
    out: &Path,
) -> Result<Vec<String>, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let Some(name) = binary.file_name().and_then(|n| n.to_str()) else {
        return Err(Error::Failed(format!(
            "{} has no file name to package it as",
            binary.display()
        )));
    };
    let bytes = read(binary)?;
    let sbom_bytes = read(sbom)?;
    if sbom_bytes.is_empty() {
        return Err(Error::Failed(format!(
            "the SBOM {} is empty",
            sbom.display()
        )));
    }
    let stem = format!("{name}-{commit}-{}", platform.name());
    let tgz_name = format!("{stem}.tar.gz");
    let sbom_name = format!("{stem}.cdx.json");
    let (sums_path, mut lines) = open_sums(out, platform)?;
    if let Some(l) = lines
        .iter()
        .find(|l| [&tgz_name, &sbom_name].contains(&&l.name))
    {
        return Err(Error::Failed(format!(
            "{} already lists {}",
            sums_path.display(),
            l.name
        )));
    }
    let tgz = tarball(name, &bytes).map_err(Error::Failed)?;
    write_tarball(&out.join(&tgz_name), &tgz, name, &bytes)?;
    write_new(&out.join(&sbom_name), &sbom_bytes)?;
    lines.push(sums::Line {
        sha256: Sha256::of(&tgz),
        name: tgz_name,
    });
    lines.push(sums::Line {
        sha256: Sha256::of(&sbom_bytes),
        name: sbom_name,
    });
    let text = write_sums(&sums_path, &lines)?;
    let mut printed = vec![format!("packaged {stem}:")];
    printed.extend(text.lines().map(str::to_owned));
    Ok(printed)
}

#[cfg(test)]
mod tests;
