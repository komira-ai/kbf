//! The archive `meta-backup` writes and `meta-verify` reads: an uncompressed tar whose
//! entries are the metadata directory's files as `meta/<name>`, in name order, then
//! [`MANIFEST`], one `<sha256>  meta/<name>` line per file, the form `sha256sum -c`
//! checks after a plain `tar -xf`.
//!
//! Uncompressed, so the copy costs only what the disk takes to read the log while the
//! server is stopped.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use super::{MetaError, RECORD};

/// The manifest's name in the archive.
pub const MANIFEST: &str = "MANIFEST.sha256";
/// The directory the files are under in the archive.
const PREFIX: &str = "meta/";
/// The most bytes of a manifest `extract` reads.
const MANIFEST_MAX: u64 = 1 << 20;
/// The buffer between the disk and the archive, each way.
const BUF: usize = 1 << 20;

/// What [`write`] wrote.
#[derive(Debug)]
pub struct Written {
    pub files: usize,
    pub bytes: u64,
    /// The archive's own sha256, for the copy taken off the node.
    pub sha256: String,
}

/// Write every file of `meta` except `*.tmp` (a write cut short, which the server
/// removes at start) into a new archive at `out`, through a hidden file beside it that
/// is synced, linked as `out` (never replacing a file there) and its directory synced;
/// the hidden file is removed on any failure. Refuses anything in `meta` that is not a regular file, and a name
/// that is not UTF-8 or holds a newline (the manifest could not carry it).
pub fn write(meta: &Path, out: &Path) -> Result<Written, MetaError> {
    let names = files_of(meta)?;
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    let dir = out.parent().unwrap_or(Path::new("/"));
    let partial = dir.join(format!(".{name}.partial"));
    let written = write_into(meta, &names, &partial);
    let done = written.and_then(|w| {
        // A link, not a rename: it fails if `out` appeared since the check, which a
        // rename would replace.
        fs::hard_link(&partial, out).map_err(MetaError::io(format!(
            "cannot link {} to {}",
            partial.display(),
            out.display()
        )))?;
        fs::remove_file(&partial).map_err(MetaError::io(format!(
            "cannot remove {}",
            partial.display()
        )))?;
        sync_dir(dir)?;
        Ok(w)
    });
    if done.is_err() {
        let _ = fs::remove_file(&partial);
    }
    done
}

/// The names to archive, sorted.
fn files_of(meta: &Path) -> Result<Vec<String>, MetaError> {
    let what = || format!("cannot list {}", meta.display());
    let mut names = Vec::new();
    for entry in fs::read_dir(meta).map_err(MetaError::io(what()))? {
        let entry = entry.map_err(MetaError::io(what()))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(MetaError::NotFile { path });
        };
        if name.ends_with(".tmp") {
            continue;
        }
        let kind = entry.file_type().map_err(MetaError::io(what()))?;
        if !kind.is_file() || name.contains('\n') {
            return Err(MetaError::NotFile { path });
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn write_into(meta: &Path, names: &[String], partial: &Path) -> Result<Written, MetaError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(partial)
        .map_err(MetaError::io(format!(
            "cannot create {}",
            partial.display()
        )))?;
    let mut tar = tar::Builder::new(Hashing::new(BufWriter::with_capacity(BUF, file)));
    let fail = || MetaError::io(format!("cannot write {}", partial.display()));
    let mut manifest = String::new();
    let mut bytes = 0;
    for name in names {
        let path = meta.join(name);
        let src =
            File::open(&path).map_err(MetaError::io(format!("cannot open {}", path.display())))?;
        let m = src
            .metadata()
            .map_err(MetaError::io(format!("cannot stat {}", path.display())))?;
        let mut header = header(m.len(), m.mtime());
        let mut reader = Hashing::new(BufReader::with_capacity(BUF, src).take(m.len()));
        tar.append_data(&mut header, format!("{PREFIX}{name}"), &mut reader)
            .map_err(fail())?;
        exact(&path, m.len(), reader.len)?;
        manifest.push_str(&format!("{}  {PREFIX}{name}\n", reader.hex()));
        bytes += m.len();
    }
    let mut header = header(manifest.len() as u64, 0);
    tar.append_data(&mut header, MANIFEST, manifest.as_bytes())
        .map_err(fail())?;
    let mut hashing = tar.into_inner().map_err(fail())?;
    hashing.flush().map_err(fail())?;
    let sha256 = hashing.hex();
    let file = hashing
        .inner
        .into_inner()
        .map_err(io::IntoInnerError::into_error);
    file.and_then(|f| f.sync_all()).map_err(fail())?;
    Ok(Written {
        files: names.len(),
        bytes,
        sha256,
    })
}

/// Refuses a file that gave other than its `want` bytes: it changed while it was read,
/// and its entry in the archive does not match its header.
fn exact(path: &Path, want: u64, got: u64) -> Result<(), MetaError> {
    if want == got {
        return Ok(());
    }
    Err(MetaError::Changed {
        path: path.to_path_buf(),
        want,
        got,
    })
}

fn header(len: u64, mtime: i64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(len);
    h.set_mode(0o600);
    h.set_mtime(u64::try_from(mtime).unwrap_or(0));
    h
}

fn sync_dir(dir: &Path) -> Result<(), MetaError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(MetaError::io(format!("cannot sync {}", dir.display())))
}

/// What [`extract`] restored.
#[derive(Debug)]
pub struct Restored {
    pub files: usize,
    pub bytes: u64,
    /// The `*.log` files: the log's segments.
    pub segments: usize,
    /// The log id the record names.
    pub log_id: String,
}

/// Extract the archive at `archive` into `into` (made, mode 0700, when absent; refused
/// when it holds anything) and check every file against the manifest. Each file is
/// written mode 0600 and synced, and the directory after them. On any failure the
/// files written are removed again (and `into` when this made it), so no unchecked copy
/// is left to start a server over.
pub fn extract(archive: &Path, into: &Path) -> Result<Restored, MetaError> {
    let open = File::open(archive);
    let src = open.map_err(MetaError::io(format!("cannot open {}", archive.display())))?;
    let made = prepare(into)?;
    let mut written = Vec::new();
    let done = unpack(src, into, &mut written);
    if done.is_err() {
        for name in &written {
            let _ = fs::remove_file(into.join(name));
        }
        if made {
            let _ = fs::remove_dir(into);
        }
    }
    done
}

/// Refuses a non-empty `into`; makes it when absent. Whether it made it.
fn prepare(into: &Path) -> Result<bool, MetaError> {
    match fs::read_dir(into) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(MetaError::NotEmpty(into.to_path_buf()));
            }
            Ok(false)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(0o700)
                .create(into)
                .map_err(MetaError::io(format!("cannot make {}", into.display())))?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
            Err(MetaError::NotDir(into.to_path_buf()))
        }
        Err(e) => Err(MetaError::io(format!("cannot read {}", into.display()))(e)),
    }
}

fn unpack(src: File, into: &Path, written: &mut Vec<String>) -> Result<Restored, MetaError> {
    let bad = |why: String| MetaError::Archive(why);
    let mut tar = tar::Archive::new(BufReader::new(src));
    let mut got = BTreeMap::new();
    let mut manifest = None;
    let mut bytes = 0;
    for entry in tar.entries().map_err(broken)? {
        let entry = entry.map_err(broken)?;
        let path = entry.path().map_err(broken)?;
        let path = path
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| bad("a name that is not UTF-8".into()))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(bad(format!("{path} is not a regular file")));
        }
        if path == MANIFEST {
            if manifest.is_some() {
                return Err(bad(format!("{MANIFEST} twice")));
            }
            let mut text = String::new();
            entry
                .take(MANIFEST_MAX)
                .read_to_string(&mut text)
                .map_err(|e| bad(format!("{MANIFEST}: {e}")))?;
            manifest = Some(text);
            continue;
        }
        let name = path
            .strip_prefix(PREFIX)
            .filter(|n| !matches!(*n, "" | "." | "..") && !n.contains('/'))
            .ok_or_else(|| bad(format!("{path} is not meta/<file>")))?
            .to_owned();
        if got.contains_key(&name) {
            return Err(bad(format!("{path} twice")));
        }
        let to = into.join(&name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&to)
            .map_err(MetaError::io(format!("cannot create {}", to.display())))?;
        written.push(name.clone());
        let mut sink = Hashing::new(file);
        let mut entry = entry;
        io::copy(&mut entry, &mut sink).map_err(broken)?;
        sink.inner
            .sync_all()
            .map_err(MetaError::io(format!("cannot sync {}", to.display())))?;
        bytes += sink.len;
        got.insert(name, sink.hex());
    }
    let manifest = manifest.ok_or(MetaError::NoManifest(MANIFEST))?;
    check(&manifest, &got)?;
    sync_dir(into)?;
    let record = fs::read_to_string(into.join(RECORD)).map_err(MetaError::io(format!(
        "cannot read {}",
        into.join(RECORD).display()
    )))?;
    let log_id = record
        .lines()
        .find_map(|l| l.strip_prefix("log_id "))
        .unwrap_or("(none in the record)")
        .to_owned();
    Ok(Restored {
        files: got.len(),
        bytes,
        segments: got.keys().filter(|n| n.ends_with(".log")).count(),
        log_id,
    })
}

/// A tar the reader cannot read on.
fn broken(e: io::Error) -> MetaError {
    MetaError::Archive(e.to_string())
}

/// Every manifest line names an extracted file with its hash, every extracted file has
/// a line, and the log record is among them.
fn check(manifest: &str, got: &BTreeMap<String, String>) -> Result<(), MetaError> {
    let mut listed = BTreeMap::new();
    for l in manifest.lines() {
        let (hash, path) = l
            .split_once("  ")
            .ok_or_else(|| MetaError::Archive(format!("manifest line {l:?}")))?;
        let name = path
            .strip_prefix(PREFIX)
            .ok_or_else(|| MetaError::Archive(format!("manifest line {l:?}")))?;
        if listed.insert(name.to_owned(), hash.to_owned()).is_some() {
            return Err(MetaError::Archive(format!("manifest lists {path} twice")));
        }
    }
    for (name, hash) in &listed {
        match got.get(name) {
            None => return Err(MetaError::Missing { name: name.clone() }),
            Some(h) if h != hash => return Err(MetaError::Checksum { name: name.clone() }),
            Some(_) => {}
        }
    }
    if let Some(name) = got.keys().find(|n| !listed.contains_key(*n)) {
        return Err(MetaError::Unlisted { name: name.clone() });
    }
    if !got.contains_key(RECORD) {
        return Err(MetaError::NoRecordInArchive);
    }
    Ok(())
}

/// A reader or writer that hashes and counts what passes through it.
struct Hashing<T> {
    inner: T,
    hash: Sha256,
    len: u64,
}

impl<T> Hashing<T> {
    fn new(inner: T) -> Self {
        Hashing {
            inner,
            hash: Sha256::new(),
            len: 0,
        }
    }

    fn hex(&self) -> String {
        hex::encode(self.hash.clone().finalize())
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The entry names of the archive at `archive`, in order, for tests.
#[cfg(test)]
pub(crate) fn entry_names(archive: &Path) -> Vec<PathBuf> {
    let mut tar = tar::Archive::new(File::open(archive).expect("open the archive"));
    tar.entries()
        .expect("entries")
        .map(|e| e.expect("an entry").path().expect("a path").into_owned())
        .collect()
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
