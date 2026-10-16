//! The offline restore: replaces a state file with a backup of one, under the lock, and
//! moves the epoch past both.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::SqliteError;
use super::guard::{filesystem_type, refuse_network_filesystem, take_lock};
use super::schema::{bump_epoch, configure, epoch, require_state_store};

/// `dest`'s name with `suffix` after it.
fn beside(dest: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = dest.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Replaces the state file `dest` with a copy of `backup`, which must be a kbf state
/// store (a file `sqlite3 .backup` wrote from one, or a copy of the file taken while no
/// process held it). The file in `dest` is dropped, with any journal beside it. The
/// epoch of the restored file becomes one past the larger of the backup's and the one
/// `dest` held, so a restored file cannot resume leases the replaced one issued; that
/// epoch is returned, and the store's next start bumps it again.
///
/// # Errors
/// [`SqliteError::Locked`] while a process has `dest` open (the restore is offline);
/// [`SqliteError::NetworkFs`] on a network filesystem; [`SqliteError::NotAStateStore`]
/// when `backup` is not a state store; otherwise the failing call. `dest` is untouched
/// when the restore fails before it replaces the file.
pub fn restore(backup: &Path, dest: &Path) -> Result<u64, SqliteError> {
    restore_into(backup, dest).map_err(|error| error.at(dest))
}

fn restore_into(backup: &Path, dest: &Path) -> Result<u64, SqliteError> {
    refuse_network_filesystem(dest, &filesystem_type)?;
    let _lock = take_lock(dest)?;
    let staging = beside(dest, ".restoring");
    let staged = stage(backup, dest, &staging);
    if staged.is_err() {
        // Best effort: a failed restore leaves no staging copy behind.
        let _ = fs::remove_file(&staging);
    }
    let epoch = staged?;
    for suffix in ["-wal", "-shm"] {
        match fs::remove_file(beside(dest, suffix)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    fs::rename(&staging, dest)?;
    let dir = dest.parent().filter(|dir| !dir.as_os_str().is_empty());
    fs::File::open(dir.unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(epoch)
}

/// Copies `backup` to `staging`, checks it, stamps its epoch and folds its journal in,
/// so that one file is all there is to move.
fn stage(backup: &Path, dest: &Path, staging: &Path) -> Result<u64, SqliteError> {
    fs::copy(backup, staging).map_err(|error| SqliteError::from(error).at(backup))?;
    let replaced = if dest.exists() {
        epoch(&Connection::open(dest)?)?
    } else {
        0
    };
    let conn = Connection::open(staging)?;
    configure(&conn)?;
    require_state_store(&conn).map_err(|error| error.at(backup))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(SqliteError::NotAStateStore(format!(
            "{}: the backup is damaged ({integrity})",
            backup.display()
        )));
    }
    let restored = bump_epoch(&conn, replaced)?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(restored)
}
