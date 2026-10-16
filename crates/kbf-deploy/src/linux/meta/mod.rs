//! `kbf-deploy linux meta-backup` and `linux meta-verify`: a byte copy of kbf-server's
//! metadata log (`--meta-dir`) taken in a brief stop, and the check that a copy restores
//! (docs/deploy/storage-node.md#metadata-backup-and-restore).
//!
//! kbf-server has no snapshot or backup command. The metadata directory holds the log's
//! record (`log-record`), its lock, the Raft hard state and the log's segment files; a
//! copy of every file taken while the server is stopped is a valid backup, and a server
//! started over that copy opens the same log (the same log id) at the same entry.
//!
//! `meta-backup` ([`backup`]) refuses unless the server's `/metrics` shows no write-tier
//! bytes or blocks left to flush and no flush conflict, stops the unit, refuses (and
//! restarts) if the hot store still holds a `*.blk` block (acknowledged bytes not in the
//! object store, which the copy would point at), writes the directory into one tar file
//! with a sha256 manifest inside ([`archive`]), synced with its directory, starts the
//! unit and waits for `/readyz`. It prints the seconds from the stop to the ready answer:
//! the downtime the backup cost. After the stop, any failure restarts the unit before
//! the verb returns. Without `--apply` it reads `/metrics` and the directories and
//! prints each step as `DRY-RUN:`, changing nothing.
//!
//! `meta-verify` ([`verify`]) extracts an archive into an empty directory and checks
//! every file against the manifest and that the log record is there. It never starts a
//! server: a restored copy shares the live log's id and epochs, so a server over it must
//! never hold write access to the live server's object store.
//!
//! Exit codes: 0 done, 1 a failure after the stop (the unit restarted) or an archive
//! that does not verify, 2 refused before anything changed (`REFUSED: ...`).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::plan::Mode;
use kbf_host::step::{Cmd, HttpUrl, Step, UrlError};

pub mod archive;
pub mod cli;
pub mod metrics;

use self::metrics::Tier;
use super::host::{SYSTEMCTL, tool};

/// The log's record in the metadata directory; a directory without one is not a log.
pub const RECORD: &str = "log-record";

/// How long `meta-backup` waits for `/readyz` after the start, unless told otherwise.
pub const READY: Duration = Duration::from_secs(300);

/// Why a `meta-backup` or `meta-verify` refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error(transparent)]
    Host(#[from] kbf_host::HostError),
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error("cannot read {url}: {why}")]
    Metrics { url: String, why: String },
    #[error(
        "the write tier holds {bytes} bytes in {blocks} blocks not yet in the object store; wait for kbf_write_tier_unflushed_bytes to reach 0"
    )]
    Unflushed { bytes: u64, blocks: u64 },
    #[error(
        "kbf_write_tier_flush_conflicts_total is {0}: a staged object met another in the store; find out why before a backup"
    )]
    Conflicts(u64),
    #[error("{url} shows a write tier: give --hot-dir, the hot store's directory")]
    NoHotDir { url: String },
    #[error("{url} shows no write tier, but --hot-dir was given")]
    NoTier { url: String },
    #[error("{url}: {name} {why}")]
    Family {
        url: String,
        name: &'static str,
        why: &'static str,
    },
    #[error("{0} is not a directory")]
    NotDir(PathBuf),
    #[error("{dir} holds no {RECORD}: not a kbf-server metadata directory")]
    NoRecord { dir: PathBuf },
    #[error("{0} exists; a backup never replaces a file")]
    Exists(PathBuf),
    #[error("{out} is inside {dir}, the directory being backed up")]
    Inside { out: PathBuf, dir: PathBuf },
    #[error("{0} is not empty; restore into an empty or new directory")]
    NotEmpty(PathBuf),
    #[error("unit name {0:?} holds a character a unit name cannot")]
    Unit(String),
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
    #[error("{path} is not a regular file; the metadata directory holds only files")]
    NotFile { path: PathBuf },
    #[error("{dir} still holds unflushed write-tier blocks after the stop: {names}")]
    Blocks { dir: PathBuf, names: String },
    #[error("{path} changed while it was archived ({want} bytes expected, {got} read)")]
    Changed { path: PathBuf, want: u64, got: u64 },
    #[error("{0}; kbf-server was started again")]
    Restarted(Box<MetaError>),
    #[error("{source}; and the restart failed: {restart}")]
    RestartFailed {
        source: Box<MetaError>,
        restart: Box<MetaError>,
    },
    // meta-verify: the archive does not hold a good copy.
    #[error("the archive is not one meta-backup writes: {0}")]
    Archive(String),
    #[error("the archive has no {0}")]
    NoManifest(&'static str),
    #[error("{name} does not match the manifest's sha256")]
    Checksum { name: String },
    #[error("the manifest lists {name}, which the archive does not hold")]
    Missing { name: String },
    #[error("the archive holds {name}, which the manifest does not list")]
    Unlisted { name: String },
    #[error("the archive holds no {RECORD}: a server over it would refuse the directory")]
    NoRecordInArchive,
}

impl MetaError {
    /// 1 for a failure after the stop or an archive that does not verify; 2 for a
    /// refusal before anything changed.
    pub fn exit_code(&self) -> u8 {
        match self {
            MetaError::Host(kbf_host::HostError::Step { .. })
            | MetaError::Restarted(_)
            | MetaError::RestartFailed { .. }
            | MetaError::Archive(_)
            | MetaError::NoManifest(_)
            | MetaError::Checksum { .. }
            | MetaError::Missing { .. }
            | MetaError::Unlisted { .. }
            | MetaError::NoRecordInArchive => 1,
            _ => 2,
        }
    }

    fn io(what: impl Into<String>) -> impl FnOnce(io::Error) -> MetaError {
        let what = what.into();
        move |source| MetaError::Io { what, source }
    }
}

/// What `meta-backup` was asked for.
pub struct Backup {
    pub meta_dir: HostPath,
    pub hot_dir: Option<HostPath>,
    pub out: HostPath,
    /// kbf-server's `/metrics` on its operator API port; `/readyz` is read on the same
    /// host and port.
    pub metrics: HttpUrl,
    pub unit: String,
    pub ready: Duration,
}

/// One plain-HTTP `GET`: the status and the body.
pub type Get<'a> = &'a dyn Fn(&HttpUrl) -> io::Result<(u16, Vec<u8>)>;

/// `meta-backup`: see the module's documentation. Reads go through `get` (the metrics)
/// and the file system; the unit's stop, start and the wait for `/readyz` are steps of
/// `exec`, printed to `out` as `DRY-RUN:` or `+` lines.
pub fn backup<E: Exec>(
    exec: &mut E,
    b: &Backup,
    get: Get<'_>,
    mode: Mode,
    out: &mut dyn io::Write,
) -> Result<(), MetaError> {
    let meta = Path::new(b.meta_dir.as_str());
    let target = Path::new(b.out.as_str());
    let hot = b.hot_dir.as_ref().map(|h| Path::new(h.as_str()));
    preflight(b, meta, target)?;
    let tier = metrics::read(get, &b.metrics)?;
    match (tier, hot) {
        (Some(_), None) => {
            return Err(MetaError::NoHotDir {
                url: b.metrics.to_string(),
            });
        }
        (None, Some(_)) => {
            return Err(MetaError::NoTier {
                url: b.metrics.to_string(),
            });
        }
        (Some(Tier { conflicts, .. }), _) if conflicts > 0 => {
            return Err(MetaError::Conflicts(conflicts));
        }
        (Some(Tier { bytes, blocks, .. }), _) if bytes > 0 || blocks > 0 => {
            return Err(MetaError::Unflushed { bytes, blocks });
        }
        _ => {}
    }
    if let Some(h) = hot {
        line(
            out,
            format_args!("write tier: 0 unflushed bytes, 0 blocks, 0 conflicts"),
        )?;
        if !h.is_dir() {
            return Err(MetaError::NotDir(h.to_path_buf()));
        }
    }
    if mode == Mode::Apply {
        let euid = exec.euid();
        if euid != 0 {
            return Err(kbf_host::HostError::NotRoot { euid }.into());
        }
        exec.check_binary().map_err(kbf_host::HostError::from)?;
    }

    let readyz = HttpUrl::new(&format!("http://{}/readyz", b.metrics.authority()))?;
    let started = Instant::now();
    // A stop that failed may still have stopped the server: start it either way.
    let copied = step(exec, mode, out, 0, &Step::Run(systemctl("stop", &b.unit)))
        .and_then(|()| after_stop(meta, hot, target, mode, out));
    let restart =
        step(exec, mode, out, 1, &Step::Run(systemctl("start", &b.unit))).and_then(|()| {
            let wait = Step::WaitHttp {
                url: readyz.clone(),
                deadline: b.ready,
            };
            step(exec, mode, out, 2, &wait)
        });
    let downtime = started.elapsed();
    match (copied, restart) {
        (Ok(written), Ok(())) => {
            if let Some(w) = written {
                line(
                    out,
                    format_args!(
                        "archive: {} ({} files, {} bytes, sha256 {})",
                        b.out, w.files, w.bytes, w.sha256
                    ),
                )?;
                line(
                    out,
                    format_args!(
                        "downtime: {:.1} s from the stop to {readyz} answering 200",
                        downtime.as_secs_f64()
                    ),
                )?;
            } else {
                line(out, format_args!("{}", kbf_host::plan::DRY_RUN_NOTE))?;
            }
            Ok(())
        }
        (Err(e), Ok(())) => Err(MetaError::Restarted(Box::new(e))),
        (Ok(_), Err(restart)) => Err(restart),
        (Err(e), Err(restart)) => Err(MetaError::RestartFailed {
            source: Box::new(e),
            restart: Box::new(restart),
        }),
    }
}

/// The checks before the stop, in a dry run as in an apply.
fn preflight(b: &Backup, meta: &Path, target: &Path) -> Result<(), MetaError> {
    let unit_ok = !b.unit.is_empty()
        && b.unit
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.@:".contains(&c));
    if !unit_ok {
        return Err(MetaError::Unit(b.unit.clone()));
    }
    if !meta.is_dir() {
        return Err(MetaError::NotDir(meta.to_path_buf()));
    }
    if !meta.join(RECORD).is_file() {
        return Err(MetaError::NoRecord {
            dir: meta.to_path_buf(),
        });
    }
    if target.symlink_metadata().is_ok() {
        return Err(MetaError::Exists(target.to_path_buf()));
    }
    let parent = target.parent().unwrap_or(Path::new("/"));
    if !parent.is_dir() {
        return Err(MetaError::NotDir(parent.to_path_buf()));
    }
    let parent = parent.canonicalize().map_err(MetaError::io(format!(
        "cannot resolve {}",
        parent.display()
    )))?;
    let meta = meta
        .canonicalize()
        .map_err(MetaError::io(format!("cannot resolve {}", meta.display())))?;
    if parent.starts_with(&meta) {
        return Err(MetaError::Inside {
            out: target.to_path_buf(),
            dir: meta,
        });
    }
    Ok(())
}

/// The block check and the archive, between the stop and the start; `None` in a dry
/// run, which only prints them.
fn after_stop(
    meta: &Path,
    hot: Option<&Path>,
    target: &Path,
    mode: Mode,
    out: &mut dyn io::Write,
) -> Result<Option<archive::Written>, MetaError> {
    let prefix = if mode == Mode::Apply { "+" } else { "DRY-RUN:" };
    if let Some(h) = hot {
        line(
            out,
            format_args!("{prefix} check {} holds no *.blk", h.display()),
        )?;
        if mode == Mode::Apply {
            blocks_left(h)?;
        }
    }
    line(
        out,
        format_args!(
            "{prefix} archive {} into {} with a sha256 manifest, synced",
            meta.display(),
            target.display()
        ),
    )?;
    if mode == Mode::DryRun {
        return Ok(None);
    }
    archive::write(meta, target).map(Some)
}

/// Refuses if `hot` holds a write-tier block (`*.blk`): bytes acknowledged to a client
/// and not yet in the object store. A flushed copy is `*.seg`.
fn blocks_left(hot: &Path) -> Result<(), MetaError> {
    let what = || format!("cannot list {}", hot.display());
    let mut names = Vec::new();
    for entry in std::fs::read_dir(hot).map_err(MetaError::io(what()))? {
        let name = entry.map_err(MetaError::io(what()))?.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".blk") {
            names.push(name.into_owned());
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    names.sort();
    Err(MetaError::Blocks {
        dir: hot.to_path_buf(),
        names: names.join(" "),
    })
}

fn systemctl(verb: &str, unit: &str) -> Cmd {
    Cmd::new(tool(SYSTEMCTL)).arg(verb).arg(unit)
}

/// Print `s`, and in an apply carry it out; `index` names it in a failure.
fn step<E: Exec>(
    exec: &mut E,
    mode: Mode,
    out: &mut dyn io::Write,
    index: usize,
    s: &Step,
) -> Result<(), MetaError> {
    match mode {
        Mode::DryRun => line(out, format_args!("DRY-RUN: {s}")),
        Mode::Apply => {
            line(out, format_args!("+ {s}"))?;
            exec.apply(s).map_err(|source| {
                kbf_host::HostError::Step {
                    index,
                    step: s.to_string(),
                    source,
                }
                .into()
            })
        }
    }
}

fn line(out: &mut dyn io::Write, l: std::fmt::Arguments<'_>) -> Result<(), MetaError> {
    writeln!(out, "{l}").map_err(MetaError::io("cannot write the plan out"))
}

/// `meta-verify`: extract `archive` into `into` (empty or absent) and check it; the
/// lines to print.
pub fn verify(archive: &Path, into: &Path) -> Result<Vec<String>, MetaError> {
    let r = archive::extract(archive, into)?;
    Ok(vec![
        format!(
            "verified: {} files, {} bytes, {} log segments, every sha256 matches the manifest",
            r.files, r.bytes, r.segments
        ),
        format!("log id: {}", r.log_id),
        format!(
            "restored into {}; it shares the live log's id: never start a server over it with write access to the live object store",
            into.display()
        ),
    ])
}

#[cfg(test)]
mod tests;
