//! A systemd unit as the ops verbs compare it: a fixed list of properties, normalised, hashed.
//!
//! `systemctl show` prints dozens of properties and many change with time (start timestamps,
//! memory in use, CPU used). The comparison asks only for [`STABLE_PROPERTIES`], which are the
//! ones a stop, a restart, a reconfiguration or a unit-file change would alter, and reduces
//! `ExecStart` to its argument vector, since its raw form carries the start time and the pid.
//! A unit that is unchanged gives the same hash before and after a run.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::host::{Cmd, Host, run_ok};
use crate::error::BenchError;

/// The properties compared. `MainPID` and `NRestarts` are here on purpose: they change if the
/// unit was restarted, which is what a run must not do to a live unit.
pub const STABLE_PROPERTIES: [&str; 17] = [
    "Id",
    "LoadState",
    "ActiveState",
    "SubState",
    "UnitFileState",
    "FragmentPath",
    "DropInPaths",
    "Environment",
    "User",
    "Group",
    "WorkingDirectory",
    "MemoryMax",
    "CPUQuotaPerSecUSec",
    "Restart",
    "MainPID",
    "NRestarts",
    "ExecStart",
];

/// What was read of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitView {
    /// SHA-256 of the normalised property listing, lowercase hex.
    pub show_sha256: String,
    /// SHA-256 of the argument vector of `ExecStart`, lowercase hex.
    pub argv_sha256: String,
    /// The names of the flags on the command line (`--name`), values dropped.
    pub flags: BTreeSet<String>,
}

/// SHA-256 as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The arguments of an `ExecStart` value: the words after `argv[]=` up to the next ` ; `.
/// `None` if the value has no `argv[]=`.
#[must_use]
pub fn argv_of(exec_start: &str) -> Option<String> {
    let (_, rest) = exec_start.split_once("argv[]=")?;
    let argv = rest.split_once(" ; ").map_or(rest, |(argv, _)| argv);
    Some(argv.trim_end_matches(" }").trim().to_owned())
}

/// The listing with `ExecStart` reduced to its arguments, one `Name=value` per line, sorted.
#[must_use]
pub fn normalize_show(text: &str) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| line.contains('='))
        .map(|line| match line.strip_prefix("ExecStart=") {
            Some(value) => format!("ExecStart={}", argv_of(value).unwrap_or_default()),
            None => line.to_owned(),
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// The flag names (`--name`, without a `=value`) among the words of an argument vector.
#[must_use]
pub fn flag_names(argv: &str) -> BTreeSet<String> {
    argv.split_whitespace()
        .filter(|word| word.starts_with("--"))
        .map(|word| {
            word.split_once('=')
                .map_or(word, |(name, _)| name)
                .to_owned()
        })
        .collect()
}

/// Reads `unit` with `systemctl show`, read-only.
///
/// # Errors
/// [`BenchError::Ops`] if `systemctl` cannot be run or fails, or the unit has no `ExecStart`
/// with an argument vector.
pub fn read_unit(host: &dyn Host, unit: &str) -> Result<UnitView, BenchError> {
    let mut args = vec!["show".to_owned(), unit.to_owned()];
    args.extend(STABLE_PROPERTIES.iter().map(|p| format!("--property={p}")));
    let cmd = Cmd {
        program: "systemctl".into(),
        args,
        env: Vec::new(),
        stdin: None,
    };
    let text = run_ok(host, "systemctl show", &cmd)?;
    let normal = normalize_show(&text);
    let argv = normal
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .filter(|argv| !argv.is_empty())
        .ok_or_else(|| BenchError::Ops("systemctl show: the unit has no ExecStart".to_owned()))?;
    Ok(UnitView {
        show_sha256: sha256_hex(normal.as_bytes()),
        argv_sha256: sha256_hex(argv.as_bytes()),
        flags: flag_names(argv),
    })
}

/// Whether `unit` is not loaded at all (`LoadState=not-found`), so no unit file of that name
/// exists.
///
/// # Errors
/// [`BenchError::Ops`] if `systemctl` cannot be run or fails.
pub fn unit_absent(host: &dyn Host, unit: &str) -> Result<bool, BenchError> {
    let cmd = Cmd::new(
        "systemctl",
        &["show", unit, "--property=LoadState", "--value"],
    );
    run_ok(host, "systemctl show", &cmd).map(|out| out.trim() == "not-found")
}
