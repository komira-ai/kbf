//! One run of the tool: the order of the checks, the copy, and the two read-backs.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use crate::cli::{self, Args, Request};
use crate::cutover;
use crate::digest::Summary;
use crate::error::Error;
use crate::lock::{self, Guard};
use crate::seams::{Seams, Stage};
use crate::source::{self, Snapshot};
use crate::target::{self, Target};

const CHECKED: &str = "the target is empty, its schema is current and the clocks agree\n";
const DRY_RUN: &str = "dry run: nothing was written; run again with --apply to copy\n";

/// Runs the tool on a command line: prints the report to `out` and any error to `err`, and
/// returns the exit status (0 done, 2 refused, 1 failed or a usage error).
pub fn main_with(
    argv: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    main_with_seams(argv, &Seams::default(), out, err)
}

/// [`main_with`] with the places a test reaches into passed in.
pub fn main_with_seams(
    argv: impl IntoIterator<Item = OsString>,
    seams: &Seams,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let request = match cli::parse(argv.into_iter().collect()) {
        Ok(request) => request,
        Err(usage) => return usage_exit(&usage, out, err),
    };
    match dispatch(&request, seams, out) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "kbf-state-copy: {error}");
            error.code()
        }
    }
}

fn dispatch(request: &Request, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    match request {
        Request::Copy(args) => run(args, seams, out),
        Request::Cutover(args) => cutover::run(args, seams, out),
        Request::Rollback(args) => cutover::rollback(args, seams, out),
    }
}

/// Help goes to `out` and ends the run well; a usage error goes to `err` and fails it.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let text = usage.render().to_string();
    let failed = usage.use_stderr();
    let _ = if failed {
        err.write_all(text.as_bytes())
    } else {
        out.write_all(text.as_bytes())
    };
    u8::from(failed)
}

/// The checks and, with `args.apply`, the copy.
///
/// In order: the frontend's lock (a held lock is refused before anything else is read);
/// the state file, read once; the target's URL, schema, emptiness and clock. A dry run
/// stops there. A copy then writes in one transaction, which reads the rows back and
/// compares them with the file before it commits, and after the commit reads them back
/// again on a new connection (a failure after the commit says the copy is committed). The
/// lock is kept until the end of a copy and let go of once the file is read in a dry run.
///
/// # Errors
/// [`Error::Refused`] when a precondition does not hold (nothing was changed);
/// [`Error::Failed`] when a call fails or a read-back does not match.
pub fn run(args: &Args, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    let db = lock::canonical(&args.state_db)?;
    let held = lock::take(&db)?;
    copy(args, seams, out, &db, held)?;
    if !args.apply {
        out.write_all(DRY_RUN.as_bytes())?;
    }
    Ok(())
}

/// The checks and the copy of [`run`], for a caller that holds the frontend's lock `held` on
/// the state file `db` (its canonical name). After a copy the lock is handed back still
/// held, for the caller to keep while it does what must follow; after a dry run there is
/// none (the lock is let go of once the file is read).
///
/// # Errors
/// As [`run`].
pub(crate) fn copy(
    args: &Args,
    seams: &Seams,
    out: &mut dyn Write,
    db: &Path,
    held: Guard,
) -> Result<Option<Guard>, Error> {
    let snapshot = source::read(db)?;
    let kept = args.apply.then_some(held);
    let want = snapshot.summary();
    let text = report(db, args.apply, &snapshot, &want);
    out.write_all(text.as_bytes())?;
    let target = Target::new(&args.postgres_url)?;
    target.check_schema()?;
    let probe = target.probe()?;
    target::require_empty(probe.held)?;
    target::require_clocks(probe.server_clock_us, (seams.local_now_us)())?;
    out.write_all(CHECKED.as_bytes())?;
    if !args.apply {
        return Ok(kept);
    }
    target.apply(&snapshot, &want, seams)?;
    read_back(&target, &snapshot, &want, seams)?;
    // The copy is committed and has passed both checks: a report that cannot be written does
    // not undo that.
    let _ = out.write_all(done(&snapshot, &want).as_bytes());
    Ok(kept)
}

/// Reads the committed copy back on a new connection and compares it with the file: its
/// families and its head revision.
fn read_back(
    target: &Target,
    snapshot: &Snapshot,
    want: &Summary,
    seams: &Seams,
) -> Result<(), Error> {
    let (copy, head) = fetch(target, seams).map_err(unread)?;
    target::verify(want, &copy, target::AFTER_COMMIT)?;
    target::verify_head(snapshot.target_head(), head, target::AFTER_COMMIT)
}

/// The families and the head revision of the committed copy, read on a new connection.
fn fetch(target: &Target, seams: &Seams) -> Result<(Summary, i64), Error> {
    let mut fresh = target.connect()?;
    if let Some(statement) = (seams.tamper)(Stage::AfterCommit) {
        fresh.batch_execute(statement)?;
    }
    let copy = target::summary(&mut fresh)?;
    Ok((copy, target::read_head(&mut fresh)?))
}

/// A failure to read the committed copy back, which says the copy is in the target all the
/// same: a retry would be refused as a target that holds data.
fn unread(error: Error) -> Error {
    Error::Failed(format!(
        "the copy was committed but could not be read back ({error}); check the target, and \
         drop its schema and set it up again before another try"
    ))
}

/// What was read from the state file.
fn report(db: &Path, apply: bool, snapshot: &Snapshot, want: &Summary) -> String {
    let mode = if apply { "copy" } else { "dry run" };
    let path = db.display();
    let rows = want.key_rows();
    let counters = want.counter_rows();
    let highest = snapshot.highest_version();
    let head = snapshot.head;
    let table = want.table();
    format!(
        "kbf-state-copy ({mode})\n\
         state file: {path}\n\
         key/value rows: {rows}; counters: {counters}; highest version: {highest}; \
         head revision: {head}\n\
         {table}"
    )
}

/// What a finished copy did.
fn done(snapshot: &Snapshot, want: &Summary) -> String {
    let rows = want.key_rows();
    let counters = want.counter_rows();
    let head = snapshot.target_head();
    let families = want.keys.len() + want.counters.len();
    format!(
        "copied key/value rows: {rows}, counters: {counters}; head revision now {head}\n\
         verified before the commit and again on a new connection after it: \
         {families} families match\n"
    )
}
