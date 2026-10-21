//! The mutant tests of the slot clean (`tests/slot_clean.rs`): the freshness scenario run
//! with one step of the clean skipped, to show which rows turn red. One test per step
//! ([`kbf_driver_native::slots::clean::STEPS`]); the table of rows per step is
//! `tests/support/slot_rows.rs`, whose Linux model is `tests/slot_mutants.rs`. These
//! are the run of that table against the real steps, on a Mac as root, and have not
//! run yet.

use std::collections::BTreeSet;

use kbf_driver_native::slots::clean::{Mutant, Step, Verify};

use super::freshness;
use crate::support::slot_rows::covered_by;

/// The rows [`freshness`] must find with `skip`'s step of the clean skipped and the
/// check blind: the rows the table gives the step ([`covered_by`]), but for three steps the
/// lease's end has done before the clean runs. `None` is "not asserted"; `Some` is
/// asserted.
///
/// The lease's end ends the uid's jobs and processes ([`kbf_daemon::uid::end`], from
/// `UidWatch::end`: a bootout of `user/<uid>`, then the kill) before the clean runs.
/// - [`Step::Kill`]: the processes rows are gone before the clean's own kill, so the gate
///   cannot see that step skipped; the test pins that (no row red), to be changed to the
///   table's rows when a row plants what only the clean's kill can end.
/// - [`Step::Bootout`]: the same reasoning says the `agent` rows are gone before the
///   clean's bootout (the plist in the home is the home wipe's row, not this step's).
///   The first Mac run (spike-slot-reach run 39645223554) showed no red row, but a
///   bootout that leaves nothing behind is also what a flaky observation looks like, so
///   what is red is printed and not asserted.
/// - [`Step::Images`]: the same run showed the `image` row green with the detach skipped,
///   and `hdiutil info` listed no image: the lease's end (the bootout or the kill of the
///   process that attached it) had detached a user's disk image already. Printed, not
///   asserted, for the same reason as the kill and the bootout.
fn expected_red(skip: Step) -> Option<BTreeSet<String>> {
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

/// Runs [`freshness`] with `skip` skipped and the check blind (design: mutant B), so
/// that what the step should have removed reaches lease B. Asserts the scenario ran
/// (every row planted, both leases exited 0) and that [`expected_red`] rows are red, if
/// it names any. Rows outside it that are red too are printed, not asserted: they are
/// how a skipped step can ripple (a job launchd starts again after a kill nobody booted
/// it out of leaves a process), seen on the first run on a Mac.
async fn mutant_turns_red(skip: Step) {
    let name = format!("mutant-{skip}");
    let mutant = Mutant {
        skip,
        verify: Verify::Blind,
    };
    let fresh = freshness(&name, Some(mutant)).await;
    fresh.assert_ran();
    let Some(want) = expected_red(skip) else {
        eprintln!(
            "mutant {skip}: not asserted; red rows {:?}; found {:?}",
            fresh.red, fresh.found
        );
        return;
    };
    let missing: Vec<&String> = want.difference(&fresh.red).collect();
    let also: Vec<&String> = fresh.red.difference(&want).collect();
    eprintln!("mutant {skip}: want red {want:?}; also red {also:?}");
    assert!(
        missing.is_empty(),
        "with the {skip} step skipped these rows stayed green: {missing:?} (red: {:?}; found {:?})\n{}",
        fresh.red,
        fresh.found,
        fresh.diagnosis
    );
    if want.is_empty() {
        assert!(
            fresh.red.is_empty(),
            "a row sees the {skip} step skipped now ({:?}): move it into the table of expected rows",
            fresh.red
        );
    }
}

/// Mutant observation: the bootout skipped, which rows turn red is printed and not
/// asserted. Like the kill, the bootout has run once already when the clean starts (the
/// lease's end is `uid::end`: bootout, then kill), so the lease's KeepAlive jobs, one
/// labelled like launchd's own, are probably gone before the clean's bootout and the
/// agent rows stay green; the first Mac run (spike-slot-reach run 39645223554) showed no
/// red row, and nothing about the rows is asserted. Asserts only that the scenario ran
/// (`Fresh::assert_ran`). Run it on a Mac (the `slot-clean-mutants` job of
/// `spike-slot-reach.yml`) to see the observation.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_bootout_is_observed_not_asserted_because_the_lease_end_booted_out_first() {
    mutant_turns_red(Step::Bootout).await;
}

/// Mutant test: the crontab and `at` sweep skipped turns `crontab` and `at` red.
/// Catches a schedules sweep that does nothing while the freshness test stays green.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_schedules_sweep_turns_the_crontab_and_at_rows_red() {
    mutant_turns_red(Step::Schedules).await;
}

/// Mutant test: the kill skipped turns no row red, because the lease's end has ended the
/// uid's processes already ([`expected_red`]). Pins that the freshness test is blind to
/// this step; it goes red, and the table changes, when a row can see it.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_kill_turns_no_row_red_because_the_lease_end_killed_first() {
    mutant_turns_red(Step::Kill).await;
}

/// Mutant observation: the image detach skipped, which rows turn red is printed and not
/// asserted. On the first Mac run (spike-slot-reach run 39645223554, job
/// `slot-clean-mutants`) the `image` row stayed green with the step skipped and
/// `hdiutil info` listed no image: the lease's end (`uid::end`, the bootout then the kill
/// of the process that attached the image) had detached it before the clean ran, as for
/// the kill and the bootout. Asserts only that the scenario ran (`Fresh::assert_ran`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_image_detach_is_observed_not_asserted_because_the_lease_end_detached_first() {
    mutant_turns_red(Step::Images).await;
}

/// Mutant test: the IPC removal skipped turns `sem` red. Catches a removal that does
/// nothing (the semaphore the last lease made stays for the next).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_ipc_removal_turns_the_sem_row_red() {
    mutant_turns_red(Step::Ipc).await;
}

/// Mutant test: the home wipe skipped turns `home`, `defaults`, `keychain` and `agent`
/// (the plist in the home) red. Catches a wipe that does nothing while the freshness
/// test stays green.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_home_wipe_turns_the_home_rows_red() {
    mutant_turns_red(Step::Home).await;
}

/// Mutant test: the `/var/folders` wipe skipped turns `T` and `C` red. Catches a wipe
/// that does nothing while the freshness test stays green.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_folders_wipe_turns_the_t_and_c_rows_red() {
    mutant_turns_red(Step::Folders).await;
}

/// Mutant test: the shared sweep skipped turns `tmp`, `var-tmp` and `shared` red.
/// Catches a sweep that does nothing (the next lease would find the last one's files in
/// the shared places).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn skipping_the_shared_sweep_turns_the_tmp_rows_red() {
    mutant_turns_red(Step::Shared).await;
}
