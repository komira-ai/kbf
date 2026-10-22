//! The residue rows of the slot-clean freshness test (`tests/slot_clean.rs`), and which
//! step of the clean removes each: the table the mutant tests read. `tests/slot_mutants.rs`
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

/// The rows each step of the clean is the only thing removing: the rows that must turn
/// red when that step alone is skipped. A row two steps remove is under both (`agent`:
/// the loaded job is [`Step::Bootout`]'s, the plist in the home [`Step::Contents`]').
/// On a Mac the lease's end has run a bootout and a kill before the clean, so the real
/// mutant tests assert that no row is red for [`Step::Kill`], and assert nothing for
/// [`Step::Bootout`] or [`Step::Images`] (the Mac run 39645223554 saw no red row for
/// either; a user's disk image is detached by the lease's end too)
/// (`tests/slot_clean/mutants.rs`); the entries are the clean's claim, not what those
/// tests see.
///
/// Every step has an entry (the array's length is [`STEPS`](kbf_driver_native::slots::clean::STEPS)'s);
/// the steps whose entry is empty are [`UNROWED`]: no row of the freshness test is theirs,
/// and a test of its own guards each.
///
/// This is a claim about the clean as written (`src/slots/clean.rs`,
/// `src/slots/macos/host.rs`): the Linux model in `tests/slot_mutants.rs` is built from
/// it, so it proves the harness, not the claim. The claim is proved only by a run on a
/// Mac (`tests/slot_clean.rs`, mutant tests).
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
/// and its mutant), [`Step::Crashes`] by the crash report under another user's
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
