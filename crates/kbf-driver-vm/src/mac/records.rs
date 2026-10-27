//! Run records: which process group each VM's `vm-run` leads, written where the next
//! daemon on the Mac finds it, so a daemon that dies without stopping its VMs does not
//! leave them to a Mac that is out of VM slots.
//!
//! Two files per run, in the `runs` directory, named by the VM:
//! - `<name>.pending`, written before the helper is asked to start anything: the boot it
//!   was written in. It names no process, because the process does not exist yet (the
//!   helper starts it and only then reports its pid). A pending record left behind
//!   means the daemon died around the start; the VM stops by itself when the control
//!   pipe closed ([`crate::mac::pipes`]), and the sweep only removes the trace.
//! - `<name>`, written as soon as the helper reports the pid: the group id, the leader's
//!   start time and the boot ([`Record`], the line the native driver's records use). It
//!   replaces the pending one and is removed when the run has ended.
//!
//! [`sweep`] is the next daemon's start: a record is acted on only when the boot is this
//! boot and the process under the recorded pid started when the record says; the helper
//! checks the same again (and that the process is the VM user's) before it kills. A record
//! of another boot, of a process that is gone and of a pid some other process has now is
//! removed and nothing is signalled. A record that is not one whole line, or is not a
//! regular file, is removed unread or after a bounded read.
//!
//! The window the pending record leaves open: between the helper starting the process and
//! the daemon writing the complete record. A daemon killed there leaves a VM that the
//! control pipe stops, not one the sweep finds.

use std::fs::OpenOptions;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use kbf_driver_native::Record;

use super::session::VmSession;

/// The suffix of a record written before the process exists.
pub const PENDING: &str = ".pending";

/// The most a record file holds that the sweep reads.
const MAX_BYTES: u64 = 256;

/// What a [`sweep`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// The groups it had the helper end.
    pub reaped: Vec<Record>,
    /// The groups the helper would not end, with why: their records stay for the next start.
    pub failed: Vec<(Record, String)>,
    /// How many records it removed without ending anything.
    pub stale: usize,
}

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

fn pending(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{PENDING}"))
}

/// Writes the pending record of `name` (new: a record never overwrites one).
///
/// # Errors
/// The directory cannot be made or the file written.
pub fn begin(dir: &Path, name: &str, boot: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_new(&pending(dir, name), &format!("{boot}\n"))
}

/// Writes the complete record of `name` and removes its pending one.
///
/// # Errors
/// The file cannot be written.
pub fn complete(dir: &Path, name: &str, record: &Record) -> io::Result<()> {
    let target = path(dir, name);
    // Written beside its name and renamed over it, so a reader sees all of it or none.
    let part = dir.join(format!("{name}.part"));
    let _ = std::fs::remove_file(&part);
    write_new(&part, &record.line())?;
    if let Err(why) = std::fs::rename(&part, &target) {
        let _ = std::fs::remove_file(&part);
        return Err(why);
    }
    let _ = std::fs::remove_file(pending(dir, name));
    Ok(())
}

/// Removes every record of `name`.
pub fn finish(dir: &Path, name: &str) {
    for file in [path(dir, name), pending(dir, name)] {
        if let Err(why) = std::fs::remove_file(&file)
            && why.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(file = %file.display(), "removing a run record: {why}");
        }
    }
}

fn write_new(file: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// The records of earlier daemons (the caller owns the directory, see
/// [`crate::mac::owner`]): ends the runs they name that are still the same
/// processes in this boot, and removes every record it has dealt with.
///
/// # Errors
/// The directory cannot be read, or this boot or the process table cannot be read. A run
/// the helper fails to end is not an error: see [`Swept::failed`].
pub fn sweep<S: VmSession>(dir: &Path, session: &S) -> io::Result<Swept> {
    let mut swept = Swept::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(swept),
        Err(why) => return Err(why),
    };
    let boot = session.boot_id()?;
    let mut names = Vec::new();
    for entry in entries {
        names.push(entry?.file_name());
    }
    names.sort();
    for name in names {
        let file = dir.join(&name);
        let label = name.to_string_lossy();
        if label.ends_with(PENDING) || label.ends_with(".part") {
            tracing::warn!(file = %file.display(), "a VM start was interrupted; its VM stops with the control pipe");
            remove(&file);
            swept.stale += 1;
            continue;
        }
        let Some(record) = read(&file) else {
            tracing::warn!(file = %file.display(), "removing a run record that is not one");
            remove(&file);
            swept.stale += 1;
            continue;
        };
        if record.boot == boot && session.process_start(record.pgid)? == Some(record.start) {
            tracing::warn!(pgid = record.pgid, "ending a VM run an earlier daemon left");
            // A refusal (the helper finds the process not the VM user's, say) is the same
            // at every start: failing here would lock the daemon out until someone
            // removed the record by hand. The record stays, and the run is tried again.
            if let Err(why) = session.reap(&record) {
                tracing::error!(
                    pgid = record.pgid,
                    "the helper would not end a left VM run: {why}"
                );
                swept.failed.push((record, why.to_string()));
                continue;
            }
            swept.reaped.push(record);
        } else {
            tracing::info!(file = %file.display(), "the run a record names is gone");
            swept.stale += 1;
        }
        remove(&file);
    }
    Ok(swept)
}

fn remove(file: &Path) {
    if let Err(why) = std::fs::remove_file(file) {
        tracing::warn!(file = %file.display(), "removing a run record: {why}");
    }
}

/// The record in `file`: a regular file read without following a link, at most
/// [`MAX_BYTES`] of it.
fn read(file: &Path) -> Option<Record> {
    let open = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)
        .ok()?;
    if !open.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    open.take(MAX_BYTES).read_to_string(&mut text).ok()?;
    Record::parse(&text)
}
