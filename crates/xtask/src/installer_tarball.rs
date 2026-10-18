//! `installer-tarball --binary <path> --platform <platform> --commit <sha> --units <dir>
//! --trust <dir> --sysusers <file> --tmpfiles <file> --out <dir>`: the payload
//! `packaging/install.sh` places: the files of the Debian package, at the same paths
//! with the same modes, because it is made from the same list ([`crate::pkg::files`],
//! which the deb and the rpm are made from too) and cannot drift from it:
//!
//! - `usr/bin/kbf-installer` (0755);
//! - `usr/lib/systemd/system/<file>` (0644): `kbf-*.service` and `kbf.slice` only;
//! - `usr/share/kbf-installer/trust/<file>`: exactly the three trust files, the pinned
//!   `gh` 0755 and the two data files 0644;
//! - `usr/lib/sysusers.d/kbf-installer.conf` and `usr/lib/tmpfiles.d/kbf-installer.conf`
//!   (0644), which `install.sh` applies after placing.
//!
//! It writes `<out>/kbf-installer-payload-<commit>-<platform>.tar.gz` and appends its
//! line to `<out>/SHA256SUMS.<platform>` ([`crate::sums`]). The name differs from the
//! `kbf-installer-<commit>-<platform>.tar.gz` that `package` writes for a binary named
//! `kbf-installer`: that one holds the binary alone.
//!
//! The tarball has regular files only, no directory entries (extracting would reset the
//! mode of `usr/bin` on the host), sorted by path, owner and group 0 with no names, time
//! 0, in a gzip stream with no file name and time 0: the same inputs are the same bytes.
//! After writing it the verb reads the file back and refuses it unless it holds exactly
//! the files it meant to write ([`check_tarball`]).
//!
//! The package ships no kbf runtime release (the installer fetches the first), so
//! neither does this; the payload is Linux only (a Mac gets the `.pkg`).

use std::io::Read;
use std::path::Path;

use flate2::Compression;

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::{Error, build_commit, pkg, sums};

/// The first part of the tarball's name.
pub const NAME: &str = "kbf-installer-payload";

/// One file of the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// Relative to the root the payload is extracted in, such as `usr/bin/kbf-installer`.
    pub path: String,
    pub mode: u32,
    pub bytes: Vec<u8>,
}

/// The file name of the tarball.
pub fn file_name(commit: &str, platform: Platform) -> String {
    format!("{NAME}-{commit}-{}.tar.gz", platform.name())
}

/// The regular files of the package's list `entries` as payload files, sorted by path.
/// The package's directories are not in it: extracting would reset the mode of `usr/bin`
/// on the host, and `tar` makes the missing ones itself.
///
/// # Errors
/// A file that cannot be read.
pub fn payload(entries: &[pkg::Entry]) -> Result<Vec<File>, Error> {
    let mut files = Vec::new();
    for e in entries.iter().filter(|e| e.sha256.is_some()) {
        files.push(File {
            path: e.dst.trim_start_matches('/').to_owned(),
            mode: e.mode,
            bytes: read(&e.src)?,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The gzip-compressed tar of `files`, in the order given.
///
/// # Errors
/// A path that does not fit a ustar header.
pub fn tarball(files: &[File]) -> Result<Vec<u8>, Error> {
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    for f in files {
        let mut header = tar::Header::new_ustar();
        header
            .set_path(&f.path)
            .map_err(|e| Error::Failed(format!("{:?} cannot be a tar entry name: {e}", f.path)))?;
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(f.bytes.len() as u64);
        header.set_mode(f.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        // Writing to memory cannot fail.
        tar.append(&header, &f.bytes[..]).expect("tar into memory");
    }
    let gzip = tar.into_inner().expect("tar into memory");
    Ok(gzip.finish().expect("gzip into memory"))
}

/// What is wrong with `tgz` as the tarball of `files`: one line per problem, none when
/// it is what [`tarball`] writes for them.
pub fn check_tarball(tgz: &[u8], files: &[File]) -> Vec<String> {
    let mut tar = Vec::new();
    if let Err(e) = flate2::read::GzDecoder::new(tgz).read_to_end(&mut tar) {
        return vec![format!("not a gzip stream: {e}")];
    }
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    let mut archive = tar::Archive::new(&tar[..]);
    // Only an archive already read from has no entries to list.
    for entry in archive.entries().expect("a fresh archive") {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!("not a tar: {e}"));
                break;
            }
        };
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let h = entry.header();
        let got = (
            h.entry_type() == tar::EntryType::Regular,
            h.mode().ok(),
            h.uid().ok(),
            h.gid().ok(),
            h.mtime().ok(),
            (h.username_bytes(), h.groupname_bytes()) == (Some(&[][..]), Some(&[][..])),
        );
        let mut held = Vec::new();
        let read_ok = entry.read_to_end(&mut held).is_ok();
        match files.iter().find(|f| f.path == path) {
            None => problems.push(format!("{path}: not a file of the payload")),
            Some(f) => {
                if got != (true, Some(f.mode), Some(0), Some(0), Some(0), true) {
                    problems.push(format!(
                        "{path}: type, mode, owner, time or owner name is not the payload's"
                    ));
                }
                if !read_ok | (held != f.bytes) {
                    problems.push(format!("{path}: the content is not the file's bytes"));
                }
            }
        }
        seen.push(path);
    }
    let want: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    if seen != want {
        problems.push(format!("entries are {seen:?}, not {want:?}"));
    }
    problems
}

/// The `installer-tarball` verb: the lines to print.
pub fn installer_tarball(
    platform: Platform,
    commit: &str,
    sources: &pkg::Sources,
    out: &Path,
) -> Result<Vec<String>, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    if platform == Platform::DarwinArm64 {
        return Err(Error::Usage(
            "--platform darwin-arm64: the payload is for Linux; a Mac gets the .pkg".to_owned(),
        ));
    }
    let files = payload(&pkg::files(sources)?)?;
    let name = file_name(commit, platform);
    let (sums_path, mut lines) = open_sums(out, platform)?;
    if let Some(l) = lines.iter().find(|l| l.name == name) {
        return Err(Error::Failed(format!(
            "{} already lists {}",
            sums_path.display(),
            l.name
        )));
    }
    let tgz = tarball(&files)?;
    let path = out.join(&name);
    write_new(&path, &tgz)?;
    let problems = check_tarball(&read(&path)?, &files);
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", path.display()))
                .collect(),
        ));
    }
    lines.push(sums::Line {
        sha256: Sha256::of(&tgz),
        name: name.clone(),
    });
    let text = write_sums(&sums_path, &lines)?;
    let mut printed = vec![format!("packaged {name}:")];
    printed.extend(text.lines().map(str::to_owned));
    Ok(printed)
}

#[cfg(test)]
mod tests;
