//! The server's durable log: a size-capped, rotating file that gets every event the
//! stderr log gets, in the same format (time, level, target, fields).
//!
//! stderr (journald under systemd) is unchanged; the file is in addition to it. The
//! directory is `--log-dir`, else `log/` under the `--hot-store` directory (beside
//! `meta/`, where the metadata log lives), else there is no file
//! ([`crate::Args::log_dir`]).
//!
//! **Files.** The newest lines are in [`FILE_NAME`] (`kbf-server.log`), older ones in
//! `kbf-server.log.1` (the previous file) up to `kbf-server.log.<files - 1>`. `files`
//! (`--log-files`) counts the current file too, so the directory never holds more than
//! `files` log files, and never more than `max_bytes * files` bytes plus one line (a
//! line longer than the cap is written alone to a fresh file, so that file alone
//! exceeds the cap). The current file is opened for append and never truncated at
//! start, so a restart keeps the entries before it.
//!
//! **Rotation** happens before a write that would cross `max_bytes`: `.<files - 2>`
//! becomes `.<files - 1>` (the oldest file is replaced, so dropped), and so on down to
//! the current file becoming `.1`. With one file, rotation deletes it and starts again.
//! A file past the count (from a run with a larger `--log-files`) is removed at start.
//!
//! **Durability.** Every event is one `write` to the file (the OS has it when the
//! event returns), and the file is never `fsync`ed per line: a crash of the host can
//! lose the last seconds, a crash of the server cannot. Files are created with mode
//! 0640 and the directory with 0750, owned by the server's user.
//!
//! **Errors never stop the server.** A directory or file that cannot be opened, or a
//! write that fails (disk full, permission), is reported once to stderr
//! ([`LogFile::open`]); the file then drops events, and tries again (a rotation) after
//! [`RETRY_BYTES`] more bytes of events would have been written, however large the cap
//! is. The stderr copy of every event is a different writer and is not affected.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::Subscriber;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;

/// The current log file's name; older ones add `.1`, `.2`, ... (`.1` is the newest of
/// them).
pub const FILE_NAME: &str = "kbf-server.log";

/// The subdirectory of the hot store's directory the log goes to by default.
pub const HOT_STORE_SUBDIR: &str = "log";

/// The default `--log-max-bytes`: 64 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 64 << 20;

/// After a failure the file drops events until this many bytes of them have passed, then
/// tries to open (rotating) again: a disk that recovers loses about 64 KiB of events,
/// whatever `--log-max-bytes` is.
pub const RETRY_BYTES: u64 = 64 << 10;

/// The default `--log-files`: 8, the current one included.
pub const DEFAULT_FILES: u32 = 8;

/// A rotating log file, usable as the writer of a `tracing_subscriber` fmt layer. Clones
/// share one file.
#[derive(Clone)]
pub struct LogFile {
    inner: Arc<Mutex<Rotating>>,
}

impl LogFile {
    /// Opens (creating, with its directory) `dir`'s log, keeping at most `files` files
    /// of `max_bytes` each. A failure is reported once on stderr and the file drops
    /// events until it can try again ([`RETRY_BYTES`]); it is not an error, so
    /// the server runs without its file.
    #[must_use]
    pub fn open(dir: &Path, max_bytes: u64, files: u32) -> Self {
        Self::open_reporting(dir, max_bytes, files, |line| eprintln!("{line}"))
    }

    /// [`LogFile::open`], telling `report` what it would tell stderr.
    #[must_use]
    pub fn open_reporting(
        dir: &Path,
        max_bytes: u64,
        files: u32,
        report: impl Fn(&str) + Send + 'static,
    ) -> Self {
        let mut rotating = Rotating {
            dir: dir.to_owned(),
            max_bytes: max_bytes.max(1),
            files: files.max(1),
            file: None,
            size: 0,
            dropped: 0,
            failing: false,
            report: Box::new(report),
        };
        rotating.start();
        Self {
            inner: Arc::new(Mutex::new(rotating)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Rotating> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = Record<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Record(self)
    }
}

/// The writer of one event: the fmt layer hands it the whole formatted line in one
/// write.
pub struct Record<'a>(&'a LogFile);

impl Write for Record<'_> {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        self.0.lock().record(line);
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The file's state, behind the lock.
struct Rotating {
    dir: PathBuf,
    max_bytes: u64,
    files: u32,
    /// The current file; `None` after a failure, until the next retry.
    file: Option<File>,
    /// The current file's length.
    size: u64,
    /// Bytes of events dropped since the failure.
    dropped: u64,
    /// A failure has been reported and no write has succeeded since.
    failing: bool,
    report: Box<dyn Fn(&str) + Send>,
}

impl Rotating {
    /// Opens the directory and the current file, removing files past the count.
    fn start(&mut self) {
        let opened = self.create_dir().and_then(|()| self.open_current());
        if let Err(e) = opened {
            self.fail(&e);
        }
        self.remove_past_count();
    }

    /// Writes `line` as one record, rotating first if it would cross the cap.
    fn record(&mut self, line: &[u8]) {
        if let Err(e) = self.append(line) {
            self.fail(&e);
        }
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.file.is_none() {
            // A failure left no file: drop events until a retry's worth has passed.
            self.dropped = self.dropped.saturating_add(len);
            if self.dropped < RETRY_BYTES {
                return Ok(());
            }
            self.dropped = 0;
            self.rotate()?;
        } else if self.size > 0 && self.size.saturating_add(len) > self.max_bytes {
            self.rotate()?;
        }
        // Every path above leaves a file open (or has returned), so this writes.
        self.file
            .as_mut()
            .map_or(Ok(()), |file| file.write_all(line))?;
        self.size = self.size.saturating_add(len);
        self.failing = false;
        Ok(())
    }

    /// Reports a failure the first time, and leaves no file open.
    fn fail(&mut self, error: &io::Error) {
        self.file = None;
        self.dropped = 0;
        if !self.failing {
            self.failing = true;
            let path = self.dir.join(FILE_NAME);
            (self.report)(&format!(
                "kbf-server: log file {}: {error}; events go to stderr only until the file can be written",
                path.display()
            ));
        }
    }

    fn path(&self, n: u32) -> PathBuf {
        if n == 0 {
            self.dir.join(FILE_NAME)
        } else {
            self.dir.join(format!("{FILE_NAME}.{n}"))
        }
    }

    fn create_dir(&self) -> io::Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(&self.dir)
    }

    /// Opens the current file for append, creating it.
    fn open_current(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o640)
            .open(self.path(0))?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    /// Shifts every file one older (dropping the oldest) and opens a new current file.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        self.create_dir()?;
        if self.path(0).symlink_metadata().is_ok() {
            if self.files == 1 {
                fs::remove_file(self.path(0))?;
            } else {
                for n in (2..self.files).rev() {
                    absent_ok(fs::rename(self.path(n - 1), self.path(n)))?;
                }
                fs::rename(self.path(0), self.path(1))?;
            }
        }
        self.open_current()
    }

    /// Removes `kbf-server.log.<n>` files with `n >= files`: left by a run that kept more.
    fn remove_past_count(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let past = name
                .to_str()
                .and_then(|n| n.strip_prefix(FILE_NAME))
                .and_then(|n| n.strip_prefix('.'))
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n >= self.files);
            if past {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// `result`, except that a file that is not there is fine.
fn absent_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The server's subscriber: events at INFO and above to `stderr`, and to `file` when
/// there is one, both in the default fmt format (the file without colour).
pub fn subscriber<W>(stderr: W, file: Option<LogFile>) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(fmt::layer().with_writer(stderr))
        .with(file.map(|file| fmt::layer().with_ansi(false).with_writer(file)))
}

/// Installs the process's tracing subscriber for `args`: every event to stderr and, when
/// [`Args::log_dir`] names a directory, also to its [`LogFile`]. Then one INFO line says
/// which. Called once by the `kbf-server` binary after the flags are parsed; a
/// subscriber already installed keeps its place. `try_init`, not `set_global_default`:
/// it also routes `log` records (rustls's) into the subscriber.
pub fn init(args: &crate::Args) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    let log_dir = args.log_dir();
    let file = log_dir
        .as_deref()
        .map(|dir| LogFile::open(dir, args.log_max_bytes, args.log_files));
    let _ = subscriber(std::io::stderr, file).try_init();
    match &log_dir {
        Some(dir) => tracing::info!(
            dir = %dir.display(),
            max_bytes = args.log_max_bytes,
            files = args.log_files,
            "durable log file on, besides stderr"
        ),
        None => tracing::info!(
            "no durable log file, stderr only: give --log-dir, or --hot-store file:///dir for <dir>/log"
        ),
    }
}
