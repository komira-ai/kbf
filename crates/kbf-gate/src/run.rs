//! The run recorder: RUNNING first, FINISHED on every exit it can see, atomic writes.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::doc::{DocError, Header, ResultDoc, Status};

/// The current time in unix seconds. A test passes its own.
pub type Clock = fn() -> u64;

/// Wall-clock unix seconds (0 before 1970).
pub fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Why a result document could not be written.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The file system refused.
    #[error("write {path}: {source}")]
    Io {
        /// The document's path.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// The document does not serialise (a NaN threshold).
    #[error(transparent)]
    Doc(#[from] DocError),
}

/// Write `bytes` to `path` so a reader, or a crash, sees the old file or the new one: a
/// temporary file beside it, synced, renamed over it, and the directory synced.
/// `between` runs after the temporary file is complete and before the rename.
fn write_atomic_between(
    path: &Path,
    bytes: &[u8],
    between: impl FnOnce(),
) -> Result<(), std::io::Error> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    between();
    fs::rename(&tmp, path)?;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(dir)?.sync_all()
}

/// A stage run being recorded in a result document on disk.
///
/// [`Run::start`] writes the document `RUNNING`. [`Run::finish`], [`Run::fail`] and
/// [`Run::extend`] write it finished. A `Run` dropped without one of them (an early return
/// with `?`, a panic) writes it `Red` with a note. See the crate docs for the exits a drop
/// cannot see.
pub struct Run {
    path: PathBuf,
    doc: ResultDoc,
    clock: Clock,
    sealed: bool,
}

impl Run {
    /// Begin a run: write the document `RUNNING` to `path`.
    pub fn start(
        path: impl Into<PathBuf>,
        header: Header,
        clock: Clock,
    ) -> Result<Self, GateError> {
        let doc = ResultDoc::begin(header, clock());
        let run = Self {
            path: path.into(),
            doc,
            clock,
            sealed: false,
        };
        run.write()?;
        Ok(run)
    }

    /// The document as it stands.
    pub fn doc(&self) -> &ResultDoc {
        &self.doc
    }

    /// The current time by this run's clock.
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    /// Change the document (add a gate, set the generation) and write it.
    pub fn update(&mut self, change: impl FnOnce(&mut ResultDoc)) -> Result<(), GateError> {
        change(&mut self.doc);
        self.write()
    }

    /// Finish with the status the gates allow ([`ResultDoc::status_from_gates`]).
    pub fn finish(mut self) -> Result<Status, GateError> {
        let status = self.doc.status_from_gates();
        self.seal(status, None)?;
        Ok(status)
    }

    /// Finish `Red`, with the reason.
    pub fn fail(mut self, note: &str) -> Result<(), GateError> {
        self.seal(Status::Red, Some(note))
    }

    /// Finish `Extended`, with the reason: no decision, which is not a pass.
    pub fn extend(mut self, note: &str) -> Result<(), GateError> {
        self.seal(Status::Extended, Some(note))
    }

    /// Finish `Red` with the reason and end the process with `code`. `std::process::exit`
    /// runs no destructor, so a process that must exit uses this instead of calling it. If the
    /// write fails the process still exits, with the document left as it was.
    pub fn fail_and_exit(mut self, note: &str, code: i32) -> ! {
        let _ = self.seal(Status::Red, Some(note));
        std::process::exit(code)
    }

    fn seal(&mut self, status: Status, note: Option<&str>) -> Result<(), GateError> {
        self.sealed = true;
        self.doc.status = status;
        self.doc.finished = Some((self.clock)());
        if let Some(note) = note {
            self.doc.note = Some(note.to_owned());
        }
        self.write()
    }

    fn write(&self) -> Result<(), GateError> {
        let bytes = self.doc.to_bytes()?;
        write_atomic_between(&self.path, &bytes, || {}).map_err(|source| GateError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.sealed {
            let why = if std::thread::panicking() {
                "the run panicked before it finished"
            } else {
                "the run ended without a result"
            };
            // A failed write leaves the RUNNING file, which no reader counts as a pass.
            let _ = self.seal(Status::Red, Some(why));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/gate-scratch")
            .join(format!("{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Catches a write in place: the process dies between the temporary file and the rename,
    /// and the old document must still be whole. The `between` closure stands for the kill; it
    /// looks at the target at the moment a crash would leave it.
    #[test]
    fn the_target_is_whole_until_the_rename() {
        let path = scratch("atomic").join("doc.json");
        fs::write(&path, b"old document").expect("old");
        let mut seen = Vec::new();
        write_atomic_between(&path, b"new document, longer than the old", || {
            seen = fs::read(&path).expect("target");
        })
        .expect("write");
        assert_eq!(
            seen, b"old document",
            "a crash before the rename leaves the old file"
        );
        assert_eq!(
            fs::read(&path).expect("target"),
            b"new document, longer than the old"
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temporary file is renamed away"
        );
    }

    /// A bare file name has an empty parent; the directory sync falls back to `.`.
    #[test]
    fn a_bare_file_name_is_written_in_the_current_directory() {
        let name = format!("gate-bare-{}.json", std::process::id());
        write_atomic_between(Path::new(&name), b"{}", || {}).expect("write");
        assert_eq!(fs::read(&name).expect("read"), b"{}");
        fs::remove_file(&name).expect("remove");
    }
}
