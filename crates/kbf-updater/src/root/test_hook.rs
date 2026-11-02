//! The hook of a fake service, for the tests of the root part (`tests/root_part.rs`,
//! `tests/root_start.rs`, `tests/root_part_root.rs`), run as the program `kbf-updater-test-hook`.
//! Not shipped. It does nothing to the host:
//!
//! ```text
//! kbf-updater-test-hook [update-hook] <step or boot word> [<artifact>] [--params FILE]
//! ```
//!
//! Its record directory is named by the file `record-dir` beside the program in its release tree
//! (a test's paths are longer than a declared argument may be, and nothing is read from the
//! environment). Each run appends its word to `steps.log` there and its arguments to `args.log`,
//! then exits as `exit-<word>` there says (`<code> [<last line>]`), or `0` with `{}`. A line of
//! noise comes first on standard output, so the last line is the report.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The file beside the program that names its record directory.
pub const RECORD_DIR_FILE: &str = "record-dir";

/// The exit code of what is not a run: no record directory, or one that cannot be written.
pub const EXIT_BROKEN: u8 = 2;

/// Runs the hook with `args` (its arguments, the program name left out) as the program at
/// `program`, writing its standard output to `out`. The exit code: the one `exit-<word>` asks for,
/// or [`EXIT_BROKEN`] with the reason on standard error.
pub fn exit_code(args: &[OsString], program: Option<&Path>, out: &mut dyn Write) -> u8 {
    run(args, program, out).unwrap_or_else(|why| {
        eprintln!("kbf-updater-test-hook: {why}");
        EXIT_BROKEN
    })
}

/// The record directory `record-dir` beside `program` names.
fn record_dir(program: Option<&Path>) -> Result<PathBuf, String> {
    let file = program
        .ok_or("the program's own path is unknown")?
        .with_file_name(RECORD_DIR_FILE);
    let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(PathBuf::from(text.trim()))
}

fn run(args: &[OsString], program: Option<&Path>, out: &mut dyn Write) -> Result<u8, String> {
    let mut args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if args.first().is_some_and(|arg| arg == "update-hook") {
        args.remove(0);
    }
    let dir = record_dir(program)?;
    let word = args.first().cloned().unwrap_or_default();
    append(&dir.join("steps.log"), &word)?;
    append(&dir.join("args.log"), &args.join(" "))?;
    let told = fs::read_to_string(dir.join(format!("exit-{word}"))).unwrap_or_default();
    let told = told.trim();
    let (code, line) = told.split_once(' ').unwrap_or((told, "{}"));
    let code = code.parse().unwrap_or(0);
    let said = |e: io::Error| format!("standard output: {e}");
    writeln!(out, "{word}: a line of noise before the report").map_err(said)?;
    writeln!(out, "{line}").map_err(said)?;
    Ok(code)
}

fn append(path: &Path, line: &str) -> Result<(), String> {
    let text = |e: io::Error| format!("{}: {e}", path.display());
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(text)?;
    writeln!(file, "{line}").map_err(text)
}
