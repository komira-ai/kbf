//! `kbf-updater-test-hook`: the hook of a fake service, for the tests of the root part
//! (tests/root_part.rs, tests/root_part_root.rs). It does nothing to the host:
//!
//! ```text
//! kbf-updater-test-hook [update-hook] <step or boot word> [<artifact>] [--params FILE]
//! ```
//!
//! Its record directory is the one variable its declaration gives it, `HOOK_RECORD_DIR` (a
//! declared `hook.environment`: the paths of a test are longer than a declared argument may be).
//! Each run appends its word to `steps.log` there and its arguments to `args.log`, then exits as
//! `exit-<word>` there says (`<code> [<last line>]`), or `0` with `{}`. A line of noise comes
//! first on standard output, so the last line is the report.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

fn append(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    writeln!(file, "{line}").unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "update-hook") {
        args.remove(0);
    }
    let dir = std::env::var("HOOK_RECORD_DIR").expect("HOOK_RECORD_DIR is declared");
    let dir = Path::new(&dir);
    let word = args.first().cloned().unwrap_or_default();
    append(&dir.join("steps.log"), &word);
    append(&dir.join("args.log"), &args.join(" "));
    let told = fs::read_to_string(dir.join(format!("exit-{word}"))).unwrap_or_default();
    let told = told.trim();
    let (code, line) = told.split_once(' ').unwrap_or((told, "{}"));
    let code: u8 = code.parse().unwrap_or(0);
    println!("{word}: a line of noise before the report");
    println!("{line}");
    ExitCode::from(code)
}
