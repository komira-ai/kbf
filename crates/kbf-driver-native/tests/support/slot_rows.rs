//! The residue rows of the slot-clean freshness test (`tests/slot_clean.rs`), and which
//! step of the clean removes each: the table the tests of necessity read. `tests/slot_steps.rs`
//! runs a model of it on every platform; `tests/slot_clean.rs` checks it against a real
//! Mac.

use kbf_driver_native::slots::clean::Step;

/// The rows of the freshness test: each place lease A leaves a marker, and what looks
/// for it after the clean. Lease B looks for most from inside its sandbox; the sandbox
/// refuses an action the setuid tools (`crontab`, `at`, `ps`), so the test runs those
/// as the slot user outside it, and reads the process table and the disk images as
/// root.
pub const ROWS: [(&str, &str); 18] = [
    (
        "lease-dir",
        "a file in lease A's directory (lease B: test -e)",
    ),
    (
        "home",
        "a file in the slot user's real home (lease B: test -e)",
    ),
    ("defaults", "a preference (lease B: defaults read)"),
    (
        "keychain",
        "a keychain with a generic password (lease B: security)",
    ),
    ("T", "a file in DARWIN_USER_TEMP_DIR (lease B: test -e)"),
    ("C", "a file in DARWIN_USER_CACHE_DIR (lease B: test -e)"),
    ("tmp", "a file in /private/tmp (lease B: test -e)"),
    ("var-tmp", "a file in /private/var/tmp (lease B: test -e)"),
    (
        "shared",
        "a file in the Shared folder under /Users (lease B: test -e)",
    ),
    (
        "agent",
        "a KeepAlive LaunchAgent in user/<uid> (lease B: launchctl print)",
    ),
    (
        "apple-agent",
        "a KeepAlive LaunchAgent labelled com.apple.* in user/<uid>, its plist the lease's (lease B: launchctl print)",
    ),
    ("crontab", "a crontab line (the user: crontab -l)"),
    ("at", "an at job (the user: atq)"),
    (
        "sleeper",
        "a sleep in a session of its own whose parent exited (root: libproc)",
    ),
    (
        "setuid",
        "a setuid-root sleep the slot user started (root: libproc)",
    ),
    ("image", "an attached disk image (root: hdiutil info)"),
    ("sem", "a System V semaphore (lease B: ipcs)"),
    (
        "processes",
        "a process of the uid, by effective or real uid, but launchd's own agents (root: libproc)",
    ),
];

/// The rows each step of the clean is the only thing removing: the rows lease B must find
/// when that step alone is left out. A row two steps remove is under both (`agent`:
/// the loaded job is [`Step::Bootout`]'s, the plist in the home [`Step::Contents`]').
/// On a Mac the lease's end has run a bootout and a kill before the clean, so the real
/// tests of necessity assert that no row is found for [`Step::Kill`], and assert nothing
/// for [`Step::Bootout`] or [`Step::Images`] (a user's disk image is detached by the
/// lease's end too) (`tests/slot_clean/steps_needed.rs`); the entries are the clean's
/// claim, not what those tests see.
///
/// Every step has an entry (the array's length is [`STEPS`](kbf_driver_native::slots::clean::STEPS)'s);
/// the steps whose entry is empty are [`UNROWED`]: no row of the freshness test is theirs,
/// and a test of its own guards each.
///
/// This is a claim about the clean as written (`src/slots/clean.rs`,
/// `src/slots/macos/host.rs`): the Linux model in `tests/slot_steps.rs` is built from
/// it, so it proves the harness, not the claim. The claim is proved only by a run on a
/// Mac (`tests/slot_clean/steps_needed.rs`).
pub const COVERS: [(Step, &[&str]); 11] = [
    (Step::Bootout, &["agent", "apple-agent"]),
    (Step::Schedules, &["crontab", "at"]),
    (Step::Kill, &["sleeper", "setuid", "processes"]),
    (Step::Images, &["image"]),
    (Step::Ipc, &["sem"]),
    (Step::Flags, &[]),
    (
        Step::Contents,
        &["home", "defaults", "keychain", "agent", "T", "C"],
    ),
    (Step::Shared, &["tmp", "var-tmp", "shared"]),
    (Step::Crashes, &[]),
    (Step::Restore, &[]),
    (Step::Look, &[]),
];

/// The steps with no row of the freshness test, and the test that guards each instead:
/// [`Step::Flags`] by the locked home (`a_home_locked_by_its_user_is_cleaned_on_the_single_pass`
/// and the same run without the flags step), [`Step::Crashes`] by the crash report under another user's
/// `DiagnosticReports`, [`Step::Restore`] by the check of the home's owner, mode and flags
/// that every clean makes, [`Step::Look`] by the second look of the check.
pub const UNROWED: [Step; 4] = [Step::Flags, Step::Crashes, Step::Restore, Step::Look];

/// The rows no step of the clean removes: the lease's directory goes with the lease,
/// before the slot is given back.
pub const UNCOVERED: [&str; 1] = ["lease-dir"];

/// The rows `step` covers.
pub fn covered_by(step: Step) -> &'static [&'static str] {
    COVERS
        .iter()
        .find(|(s, _)| *s == step)
        .map_or(&[], |(_, rows)| *rows)
}

/// The rows lease A must plant on any Mac: a run that planted fewer proves nothing.
pub const CORE: [&str; 7] = ["home", "T", "C", "keychain", "agent", "defaults", "tmp"];

/// The only rows whose planting the system may refuse and the test excuse: the setuid
/// schedulers, which a launch daemon without Full Disk Access is refused.
const EXCUSABLE: [&str; 2] = ["crontab", "at"];

/// How [`classify_unplanted`] sorted the rows lease A did not plant.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unplanted {
    /// Rows the system refused to let lease A plant here: the row and the line that says so
    /// ("Operation not permitted" or EPERM). They are left out of the precondition.
    pub not_plantable: Vec<(String, String)>,
    /// Rows not planted for any other reason, or a [`CORE`] row at all: the test fails.
    pub failed: Vec<String>,
}

/// Sorts `unplanted` (rows not planted) by what the plant's own output `text` says: a row
/// of [`EXCUSABLE`] for which the plant printed exactly `refused <row>: EPERM` is not
/// plantable here; every other row, and every core row, is a failure.
#[must_use]
pub fn classify_unplanted(unplanted: &[&str], text: &str) -> Unplanted {
    let mut sorted = Unplanted::default();
    for row in unplanted {
        let line = format!("refused {row}: EPERM");
        let refused = EXCUSABLE.contains(row)
            && !CORE.contains(row)
            && text.lines().any(|l| l.trim() == line);
        if refused {
            sorted.not_plantable.push(((*row).to_owned(), line));
        } else {
            sorted.failed.push((*row).to_owned());
        }
    }
    sorted
}
