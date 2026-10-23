//! Each step of the slot clean is needed (`tests/slot_clean.rs`): the freshness scenario
//! run with one step of the clean left out, to show which rows lease B then finds. One test
//! per step ([`kbf_driver_native::slots::clean::STEPS`]); the table of rows per step is
//! `tests/support/slot_rows.rs`, whose Linux model is `tests/slot_steps.rs`. These run
//! that table against the real steps, on a Mac as root.

use std::collections::BTreeSet;

use kbf_driver_native::slots::clean::{Check, Skipped, Step};

use super::freshness;
use crate::support::slot_rows::covered_by;

/// The rows [`freshness`] must find with `skip`'s step of the clean left out and the
/// check blind: the rows the table gives the step ([`covered_by`]), but for three steps the
/// lease's end has done before the clean runs. `None` is "not asserted"; `Some` is
/// asserted.
///
/// The lease's end ends the uid's jobs and processes ([`kbf_daemon::uid::end`], from
/// `UidWatch::end`: a bootout of `user/<uid>`, then the kill) before the clean runs.
/// - [`Step::Kill`]: the process rows are gone before the clean's own kill, so no row
///   shows that step left out; the test asserts that no row is found, and changes to the
///   table's rows when a row plants what only the clean's kill can end.
/// - [`Step::Bootout`]: the same reasoning says the `agent` rows are gone before the
///   clean's bootout (the plist in the home is the contents removal's row, not this
///   step's). The first Mac run showed no row found, but a bootout that leaves nothing
///   behind is also what a flaky observation looks like, so what is found is printed and
///   not asserted.
/// - [`Step::Images`]: the same run showed the `image` row absent with the detach left
///   out, and `hdiutil info` listed no image: the lease's end (the bootout or the kill of
///   the process that attached it) had detached a user's disk image already. Printed, not
///   asserted, for the same reason as the kill and the bootout.
fn expected_found(skip: Step) -> Option<BTreeSet<String>> {
    match skip {
        Step::Bootout | Step::Images => None,
        Step::Kill => Some(BTreeSet::new()),
        _ => Some(
            covered_by(skip)
                .iter()
                .map(|row| (*row).to_owned())
                .collect(),
        ),
    }
}

/// Runs [`freshness`] with `skip` left out and the check blind, so that what the step
/// should have removed reaches lease B. Asserts the scenario ran (every row planted, both
/// leases exited 0) and that the [`expected_found`] rows are found, if it names any. Rows
/// outside it that are found too are printed, not asserted: they are how a missing step
/// can ripple (a job launchd starts again after a kill nobody booted it out of leaves a
/// process).
async fn finds_what_the_step_removes(skip: Step) {
    let name = format!("without-{skip}");
    let skipped = Skipped {
        skip,
        check: Check::Blind,
    };
    let fresh = freshness(&name, Some(skipped)).await;
    fresh.assert_ran();
    let Some(want) = expected_found(skip) else {
        eprintln!(
            "without {skip}: not asserted; rows found {:?}; found {:?}",
            fresh.red, fresh.found
        );
        return;
    };
    let missing: Vec<&String> = want.difference(&fresh.red).collect();
    let also: Vec<&String> = fresh.red.difference(&want).collect();
    eprintln!("without {skip}: want rows {want:?}; also found {also:?}");
    assert!(
        missing.is_empty(),
        "without the {skip} step these rows were not found: {missing:?} (found rows: {:?}; found {:?})\n{}",
        fresh.red,
        fresh.found,
        fresh.diagnosis
    );
    if want.is_empty() {
        assert!(
            fresh.red.is_empty(),
            "a row sees the {skip} step left out now ({:?}): move it into the table of expected rows",
            fresh.red
        );
    }
}

/// The bootout step is observed, not asserted: which rows stay without it is printed.
/// Like the kill, the bootout has run once already when the clean starts (the lease's end
/// is `uid::end`: bootout, then kill), so the lease's KeepAlive jobs, one labelled like
/// launchd's own, are probably gone before the clean's bootout and the agent rows are
/// absent. Asserts only that the scenario ran (`Fresh::assert_ran`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn the_bootout_step_is_observed_not_asserted_because_the_lease_end_booted_out_first() {
    finds_what_the_step_removes(Step::Bootout).await;
}

/// Without the crontab and `at` sweep, lease B finds the `crontab` and `at` rows.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_schedules_sweep_the_crontab_and_at_rows_stay() {
    finds_what_the_step_removes(Step::Schedules).await;
}

/// Without the kill step no row is found, because the lease's end has ended the uid's
/// processes already ([`expected_found`]): the freshness scenario cannot tell that step
/// from its absence, and this asserts that, until a row can see it.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_kill_step_no_row_stays_because_the_lease_end_killed_first() {
    finds_what_the_step_removes(Step::Kill).await;
}

/// The image detach is observed, not asserted: on the Mac run the `image` row was absent
/// without the step and `hdiutil info` listed no image, because the lease's end (the
/// bootout then the kill of the process that attached the image) had detached it before
/// the clean ran, as for the kill and the bootout. Asserts only that the scenario ran
/// (`Fresh::assert_ran`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn the_image_detach_is_observed_not_asserted_because_the_lease_end_detached_first() {
    finds_what_the_step_removes(Step::Images).await;
}

/// Without the IPC removal, lease B finds the `sem` row: the semaphore the last lease made.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_ipc_removal_the_sem_row_stays() {
    finds_what_the_step_removes(Step::Ipc).await;
}

/// Without the flags step no freshness row is found: no row of the freshness test locks
/// anything, and the removal clears the locks of what the uid owns itself. The flags
/// step's own test is the locked home (`single_pass.rs`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_flags_step_no_freshness_row_stays() {
    finds_what_the_step_removes(Step::Flags).await;
}

/// Without the contents removal, lease B finds the `home`, `defaults`, `keychain`,
/// `agent` (the plist in the home), `T` and `C` rows.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_contents_removal_the_home_and_folder_rows_stay() {
    finds_what_the_step_removes(Step::Contents).await;
}

/// Without the shared sweep, lease B finds the `tmp`, `var-tmp` and `shared` rows: the
/// last lease's files in the shared places.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_shared_sweep_the_tmp_rows_stay() {
    finds_what_the_step_removes(Step::Shared).await;
}

/// Without the crash sweep no freshness row is found (the scenario crashes nothing). The
/// step's own test is the crash report under another user's folder (`single_pass.rs`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_crash_sweep_no_freshness_row_stays() {
    finds_what_the_step_removes(Step::Crashes).await;
}

/// Without the restore no row is found: the contents removal leaves the home as it was,
/// and nothing in the scenario changes its owner, mode or flags.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_restore_no_freshness_row_stays() {
    finds_what_the_step_removes(Step::Restore).await;
}

/// Without the second process look no row is found: the kill already left nothing, and
/// the check's own looks are blind in these runs.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_second_look_no_freshness_row_stays() {
    finds_what_the_step_removes(Step::Look).await;
}
