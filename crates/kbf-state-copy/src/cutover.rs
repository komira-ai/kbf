//! The `cutover` and `rollback` commands: the copy, and the one setting that makes the
//! frontend use the copy.
//!
//! A cutover is, in order: find the setting and check it names the state file being copied
//! and that no backup of an earlier cutover is there; check that the frontend is stopped
//! (the unit, when one is named, and the state file's lock, which is then held to the end);
//! with `--apply`, stage the backup and the new text; copy and verify as the copy command
//! does; and only then rename the new text over the setting file. A step that
//! refuses or fails leaves every file as it was: the staged files are removed, and the copy
//! writes nothing to the target that it does not roll back or report. A dry run does the
//! checks and the copy's dry run and writes no file.
//!
//! A rollback renames `<file>.pre-postgres` over the file. Neither command changes the
//! SQLite file.

use std::io::Write;

use crate::app;
use crate::cli::{Args, CutoverArgs, RollbackArgs};
use crate::error::Error;
use crate::lock;
use crate::seams::Seams;
use crate::setting::{self, Found, Place};
use crate::unit;

const DRY_RUN: &str = "dry run: no file was changed; run again with --apply to copy, verify \
    and switch the setting\n";

const ROLLBACK_DRY_RUN: &str = "dry run: no file was changed; run again with --apply to \
    restore\n";

/// Runs `kbf-state-copy cutover`.
///
/// # Errors
/// [`Error::Refused`] when a precondition does not hold (the setting is not the state file's,
/// a backup is there, the unit is running, the lock is held, the copy's checks refuse);
/// [`Error::Failed`] when a call fails. Until the last step, nothing is changed.
pub fn run(args: &CutoverArgs, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    let place = args.place.resolve()?;
    let wanted = args.frontend_url.as_deref().unwrap_or(&args.postgres_url);
    let url = setting::check_url(wanted)?;
    let db = lock::canonical(&args.state_db)?;
    let found = setting::find(&place)?;
    found.require_no_backup()?;
    setting::names_file(&found.current, &db)?;
    let rewritten = found.rewritten(url)?;
    out.write_all(header(args.apply, &place, &found, url).as_bytes())?;
    if let Some(unit_name) = &args.unit {
        unit::require_stopped(seams, unit_name)?;
        writeln!(out, "unit {unit_name}: stopped")?;
    }
    let held = lock::take(&db)?;
    let staged = args.apply.then(|| found.stage(&rewritten)).transpose()?;
    let copy_args = Args {
        state_db: args.state_db.clone(),
        postgres_url: args.postgres_url.clone(),
        apply: args.apply,
    };
    let _kept = app::copy(&copy_args, seams, out, &db, held)?;
    let Some(staged) = staged else {
        out.write_all(DRY_RUN.as_bytes())?;
        return Ok(());
    };
    staged.put_in_place(&found)?;
    // The setting is switched: a report that cannot be written does not undo that.
    let _ = out.write_all(switched(&place, &found, url).as_bytes());
    Ok(())
}

/// Runs `kbf-state-copy rollback`.
///
/// # Errors
/// [`Error::Refused`] when there is no backup or the unit is running; [`Error::Failed`] when
/// a call fails.
pub fn rollback(args: &RollbackArgs, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    let place = args.place.resolve()?;
    let restore = setting::find_backup(&place)?;
    let mode = if args.apply { "apply" } else { "dry run" };
    let (backup, file) = (restore.backup.display(), restore.file.display());
    let header = format!("kbf-state-copy rollback ({mode})\nrestore: {backup} over {file}\n");
    out.write_all(header.as_bytes())?;
    if let Some(unit_name) = &args.unit {
        unit::require_stopped(seams, unit_name)?;
        writeln!(out, "unit {unit_name}: stopped")?;
    }
    if !args.apply {
        out.write_all(ROLLBACK_DRY_RUN.as_bytes())?;
        return Ok(());
    }
    restore.apply()?;
    let next = place.next_step();
    let text = format!("restored {file} from {backup}; the backup is gone\nnext: {next}\n");
    // The file is restored: a report that cannot be written does not undo that.
    let _ = out.write_all(text.as_bytes());
    Ok(())
}

/// What the command is about to do.
fn header(apply: bool, place: &Place, found: &Found, url: &str) -> String {
    let mode = if apply { "apply" } else { "dry run" };
    let name = place.name();
    let file = found.file.display();
    let current = &found.current;
    format!(
        "kbf-state-copy cutover ({mode})\n\
         setting: {name} in {file}\n  \
         now:  {current}\n  \
         then: {url}\n"
    )
}

/// What a finished switch did.
fn switched(place: &Place, found: &Found, url: &str) -> String {
    let name = place.name();
    let file = found.file.display();
    let backup = found.backup();
    let backup = backup.display();
    let next = place.next_step();
    format!(
        "switched {name} in {file} to {url}\n\
         the original is kept byte for byte as {backup}\n\
         next: {next}\n"
    )
}
