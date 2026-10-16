//! `kbf-deploy log`: the last lines of a kbf-server's durable log, read across its
//! rotated files, oldest first. It only reads: nothing is created, renamed or removed.
//!
//! The server (`kbf_server::log_file`) writes `kbf-server.log` and, older,
//! `kbf-server.log.1`, `.2`, ... in its `--log-dir`, by default the `log` directory
//! under its `--hot-store` directory. This crate does not depend on the server, so the
//! names and the default directory are repeated here ([`FILE_NAME`], [`HOT_STORE_SUBDIR`]).
//! The verb cannot find a server's flags, so it is told the directory (`--log-dir`) or
//! the hot store's directory (`--hot-store`, which names `<dir>/log`).

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;

/// The server's current log file; older ones add `.1`, `.2`, ... (`.1` the newest).
pub const FILE_NAME: &str = "kbf-server.log";

/// The directory of the hot store's directory the server logs to by default.
pub const HOT_STORE_SUBDIR: &str = "log";

/// The lines `kbf-deploy log` prints without `--lines`.
pub const DEFAULT_LINES: u32 = 100;

/// Why the log could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The directory could not be listed.
    #[error("{}: {source}", .dir.display())]
    Dir {
        dir: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The directory holds no `kbf-server.log`, current or rotated: not a server's log
    /// directory, or a server that never logged to it.
    #[error("{}: no {FILE_NAME} here: not a kbf-server --log-dir (or the server has not logged to it)", .0.display())]
    NoLog(PathBuf),
    /// A log file could not be read.
    #[error("{}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The output could not be written.
    #[error("write: {0}")]
    Write(#[from] io::Error),
}

/// The log files of `dir`, newest first: the current one, then `.1`, `.2`, ....
fn files(dir: &Path) -> Result<Vec<PathBuf>, LogError> {
    let entries = fs::read_dir(dir).map_err(|source| LogError::Dir {
        dir: dir.to_owned(),
        source,
    })?;
    let mut found: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let n = match name.to_str()?.strip_prefix(FILE_NAME)? {
                "" => 0,
                rest => rest.strip_prefix('.')?.parse().ok()?,
            };
            Some((n, entry.path()))
        })
        .collect();
    found.sort();
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// A log file opened for reading: its path (for errors) and the open handle.
struct Opened {
    path: PathBuf,
    file: File,
}

/// Opens every log file of `dir`, newest first. A file a rotation removed since the
/// listing is skipped.
///
/// Open handles keep their files across the server's renames, so reading them after
/// this returns sees the files as they were here, whatever the server rotates meanwhile.
///
/// # Errors
/// [`LogError`]: the directory is not readable or holds no log, or a file cannot be
/// opened.
fn open_all(dir: &Path) -> Result<Vec<Opened>, LogError> {
    let paths = files(dir)?;
    if paths.is_empty() {
        return Err(LogError::NoLog(dir.to_owned()));
    }
    open_paths(paths)
}

/// Opens each of `paths`, skipping one that is not there.
fn open_paths(paths: Vec<PathBuf>) -> Result<Vec<Opened>, LogError> {
    let mut opened = Vec::new();
    for path in paths {
        match File::open(&path) {
            Ok(file) => opened.push(Opened { path, file }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(LogError::Read { path, source }),
        }
    }
    Ok(opened)
}

/// The last `count` lines of the log in `dir`, oldest first, without their newlines.
///
/// # Errors
/// [`LogError`]: see [`open_all`]; or a file cannot be read.
pub fn tail(dir: &Path, count: usize) -> Result<Vec<Vec<u8>>, LogError> {
    tail_of(open_all(dir)?, count, BLOCK)
}

/// The bytes read at a time from the end of a file.
const BLOCK: u64 = 64 << 10;

/// The last `count` lines across `opened` (newest file first), oldest first. Each file
/// is read from its end in blocks of `block` bytes, only as far back as it needs.
fn tail_of(opened: Vec<Opened>, count: usize, block: u64) -> Result<Vec<Vec<u8>>, LogError> {
    // Newest file first, each file's last lines, until `count` are found.
    let mut chunks: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut need = count;
    for Opened { path, mut file } in opened {
        if need == 0 {
            break;
        }
        let lines =
            last_lines(&mut file, need, block).map_err(|source| LogError::Read { path, source })?;
        need -= lines.len();
        chunks.push(lines);
    }
    Ok(chunks.into_iter().rev().flatten().collect())
}

/// The last `need` lines of `file` (fewer if it has fewer), oldest first, without their
/// newlines; a last line without its newline counts. Reads backwards from the end
/// until it has seen enough newlines to know where the first wanted line starts.
fn last_lines(file: &mut File, need: usize, block: u64) -> io::Result<Vec<Vec<u8>>> {
    let mut pos = file.metadata()?.len();
    // The blocks read, last block first.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    // Newlines read so far, not counting one that ends the file.
    let mut newlines = 0;
    while pos > 0 && newlines < need {
        let read = pos.min(block);
        pos -= read;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; usize::try_from(read).unwrap_or(usize::MAX)];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        if blocks.is_empty() && chunk.ends_with(b"\n") {
            newlines -= 1;
        }
        blocks.push(chunk);
    }
    blocks.reverse();
    let tail = blocks.concat();
    let body = tail.strip_suffix(b"\n").unwrap_or(&tail);
    let mut lines: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
    if tail.is_empty() {
        lines.clear();
    } else if pos > 0 {
        // The first piece started before the first block read: possibly cut.
        lines.remove(0);
    }
    let from = lines.len().saturating_sub(need);
    Ok(lines[from..].iter().map(|l| l.to_vec()).collect())
}

/// Prints the last `count` lines of the log in `dir` to `out`, one to a line.
///
/// # Errors
/// [`LogError`].
pub fn print(dir: &Path, count: usize, out: &mut dyn Write) -> Result<(), LogError> {
    for line in tail(dir, count)? {
        out.write_all(&line)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// The arguments of `kbf-deploy log`.
#[derive(Args)]
pub struct LogArgs {
    /// The server's `--log-dir`.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with = "hot_store",
        required_unless_present = "hot_store"
    )]
    log_dir: Option<PathBuf>,
    /// The directory of the server's `--hot-store`, when it was started without
    /// `--log-dir`: the log is then in its `log` directory.
    #[arg(long, value_name = "DIR")]
    hot_store: Option<PathBuf>,
    /// How many lines, counted from the newest.
    #[arg(
        long,
        value_name = "N",
        default_value_t = DEFAULT_LINES,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    lines: u32,
}

/// `kbf-deploy log`: prints the lines, or `REFUSED: ...` and exit 2.
pub fn run(a: &LogArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let dir = (a.log_dir.clone())
        .or_else(|| a.hot_store.as_ref().map(|h| h.join(HOT_STORE_SUBDIR)))
        .unwrap_or_default();
    match print(&dir, a.lines as usize, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
