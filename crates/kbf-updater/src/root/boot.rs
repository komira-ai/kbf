//! What the root part does at its start about the host's boot (docs/design/host-updates.md, 3.5).
//!
//! **A start after a boot.** The root part keeps the boot id it last started under. When it
//! starts under another, the host booted, and an update whose record says a step was running or
//! a `cutover` was about to start was cut off by the boot: that step is read as a step that was
//! killed, and the table of 3.2 decides what follows (a killed `prepare` or `cutover` is rolled
//! back, a killed `verify` too, a killed `rollback` is run again, a killed `cleanup` is run again
//! forward). So a `cutover` pending at boot is rolled back, before anything starts the service's
//! units. The post-success watcher is outside that table: a `verify` it was running is run again.
//!
//! **A start in the same boot** (the root part was restarted) changes nothing in the record:
//! the step runner resumes it as after any crash, stopping the unit of a step that was running
//! and running the step again.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use kbf_step_runner::Clock;
use kbf_step_runner::record::{Loaded, RecordError, RecordStore, State};
use update_hook::Step;

/// The file of the boot id the root part last started under, in its state directory.
pub const BOOT_ID_FILE: &str = "boot-id";

/// Whether this start is the first since the host booted: the boot id written by the last start
/// pass that ended ([`commit_boot`]) is not the clock's, or none was written.
pub fn boot_changed(state: &Path, clock: &dyn Clock) -> bool {
    let before = fs::read_to_string(state.join(BOOT_ID_FILE)).unwrap_or_default();
    before.trim() != clock.boot_id()
}

/// Writes the clock's boot id, once the start pass has read every record under it: a root part
/// killed in the middle of the pass after a boot reads the rest after a boot too.
///
/// # Errors
/// The state directory cannot be written.
pub fn commit_boot(state: &Path, clock: &dyn Clock) -> io::Result<()> {
    let tmp = state.join(format!("{BOOT_ID_FILE}.tmp"));
    // A stale one goes; one that cannot go makes the create below fail. Made new, never through a
    // link someone left at its name.
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.write_all(clock.boot_id().as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, state.join(BOOT_ID_FILE))?;
    File::open(state)?.sync_all()
}

/// After a boot: reads a step the record holds as running, or a `cutover` that had not started,
/// as a step that was killed (no exit code, no last line), so that the runner's next move is the
/// table's. A step that started under `boot_id`, the boot now, was not cut off by the boot (a
/// start after the boot ran it and was itself stopped before the boot id was written): it is left
/// as it is, and the runner stops its unit by name and runs it again, as after any crash. A record
/// that is finished, absent, of a newer schema, or in the post-success watcher is left as it is.
/// `true` when the record was changed.
///
/// # Errors
/// The record cannot be read or written.
pub fn read_as_killed(store: &RecordStore, boot_id: &str) -> Result<bool, RecordError> {
    let Loaded::Current(mut rec) = store.load()? else {
        return Ok(false);
    };
    let cut_off = match (&rec.state, rec.step) {
        (State::Running { started, .. }, _) => started.boot_id != boot_id,
        (State::Pending, Step::Cutover) => true,
        _ => false,
    };
    let killed = cut_off & rec.progress.watching.is_none();
    if killed {
        rec.state = State::Done {
            code: None,
            last_line: None,
        };
        store.save(&rec)?;
    }
    Ok(killed)
}
