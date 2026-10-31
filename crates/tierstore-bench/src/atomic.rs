//! Files that are replaced whole or not at all.
//!
//! The control file of the fault proxy, the ledger of a campaign and the state of `provision` are
//! read by another process or by a later run, possibly after a crash of this one. A reader must
//! see the old bytes or the new ones, never half of either: [`write`] writes a temporary file in
//! the same directory, syncs it, and renames it over the target (a rename inside one directory is
//! atomic), then syncs the directory so the rename itself survives a power cut.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// The temporary file beside `path`: dot-prefixed and tagged with this process, so a reader of
/// the directory that looks for the target's name never sees it, and two processes do not share
/// one.
fn temp_beside(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file name is needed"))?;
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name);
    temp.push(format!(".tmp{}", std::process::id()));
    Ok(path.with_file_name(temp))
}

/// Replaces the file at `path` with `bytes`, atomically.
///
/// # Errors
/// The operating system's error; the target is then as it was, and the temporary file is
/// removed.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = temp_beside(path)?;
    let put = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    };
    if let Err(e) = put() {
        // The error that matters is the first one.
        fs::remove_file(&temp).ok();
        return Err(e);
    }
    // The directory's own entry: best effort, since not every file system lets a directory be
    // opened for this.
    path.parent().into_iter().for_each(|dir| {
        File::open(dir).and_then(|d| d.sync_all()).ok();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;

    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Catches a replace that leaves its temporary file behind, or that does not replace: the
    /// new bytes are the whole file and the directory holds the target alone.
    #[test]
    fn a_write_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-atomic-replace");
        let path = dir.path().join("control");
        write(&path, b"first, and longer\n").unwrap();
        write(&path, b"second\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert_eq!(names(dir.path()), ["control"]);
    }

    /// Catches a replace done in place (truncate and write), where a reader that has the file open
    /// can see half of each: a handle opened before the write still reads the old bytes whole
    /// after it, and the path now leads to a different file holding the new ones.
    #[test]
    fn a_reader_with_the_file_open_keeps_the_old_bytes_whole() {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;

        let dir = TestDir::under(std::env::temp_dir(), "tb-atomic-reader");
        let path = dir.path().join("control");
        write(&path, b"old old old old\n").unwrap();
        let mut reader = File::open(&path).unwrap();
        let before = reader.metadata().unwrap().ino();
        write(&path, b"new\n").unwrap();
        let mut seen = String::new();
        reader.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "old old old old\n");
        assert_ne!(fs::metadata(&path).unwrap().ino(), before);
        assert_eq!(fs::read(&path).unwrap(), b"new\n");
    }

    /// Catches a failed replace damaging the target or leaving a temporary file: the target is a
    /// directory, so the rename fails; nothing is left beside it.
    #[test]
    fn a_failed_write_leaves_the_target_and_no_temporary_file() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-atomic-fail");
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();
        assert!(write(&path, b"x").is_err());
        assert_eq!(names(dir.path()), ["target"]);
        let missing = dir.path().join("absent").join("file");
        assert!(write(&missing, b"x").is_err());
    }

    /// Catches a path with no file name being written to: the error is the caller's input.
    #[test]
    fn a_path_with_no_file_name_is_refused() {
        let err = write(Path::new("/"), b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
