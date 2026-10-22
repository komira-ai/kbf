//! The profile canaries for real: a pool user, under the exact profile generated for a
//! lease, is refused a write outside the four allowed paths and a hard link of a
//! root-owned file into the lease directory; a profile that lacks `(deny file-write*)`
//! fails them, and one that only lets a link through fails the link canary alone; a slot
//! that fails is set aside.
//!
//! The canary latency is printed as `CANARY-LATENCY` lines (a run took 45 to 125
//! ms on the macOS runner).
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_canary`, with the users of
//! `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kbf_driver_native::network::{Isolation, Network, WeakenedProfile};
use kbf_driver_native::slots::canary::Report;
use kbf_driver_native::slots::macos::{Slot, Slots};
use kbf_driver_native::slots::{LEASES_ROOT_MODE, Pool};
use support::pool::{Scratch, Users};

fn assert_root() {
    support::pool::assert_root("slot_canary");
}

/// A pool of one user, and a slot of it with a lease directory the user owns, as a
/// lease's is when its action is about to start. Returns the lease's real path.
async fn leased(slots: &Slots, scratch: &Scratch) -> (Slot, PathBuf) {
    let leases = scratch.leases();
    std::fs::create_dir_all(&leases).expect("leases");
    std::fs::set_permissions(&leases, std::fs::Permissions::from_mode(LEASES_ROOT_MODE))
        .expect("chmod");
    let dir = leases.join("lease-1-1");
    std::fs::create_dir(&dir).expect("lease dir");
    let slot = slots.take().await.expect("a slot");
    slot.hand_over(&dir).await.expect("hand over");
    let real = std::fs::canonicalize(&dir).expect("real path");
    (slot, real)
}

fn start(name: &str, change: Option<WeakenedProfile>) -> (Users, Scratch, Slots) {
    assert_root();
    let users = Users::new(name, 1);
    let scratch = Scratch::new(name);
    let pool = Pool::new(users.0.clone()).expect("pool");
    let within = Duration::from_secs(10);
    let slots = match change {
        None => Slots::start(&pool, within),
        Some(c) => Slots::start_with_profile_change(&pool, within, c),
    };
    (users, scratch, slots.expect("slots"))
}

/// The names of the canaries that were not refused.
fn failed(report: &Report) -> Vec<&'static str> {
    report.failed.iter().map(|f| f.name).collect()
}

/// Leftovers of an open door in /private/tmp.
fn left_in_tmp() -> Vec<String> {
    std::fs::read_dir("/private/tmp")
        .expect("tmp")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".kbf-canary-"))
        .collect()
}

/// Catches a generated profile that lets a pool user write outside the lease, `/dev`,
/// its home and its `/var/folders` entry, or hard-link a root-owned file into the lease
/// directory (both with and without the network), and a canary that cannot run on the
/// Mac at all (a probe mode the binary does not answer, a launch that fails): every
/// canary must be refused, and each run is timed. Weakened profiles seen red on the runner: the
/// other tests of this file.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn the_canaries_are_refused_under_the_real_profile() {
    let (_users, scratch, slots) = start("canary-real", None);
    let (slot, lease) = leased(&slots, &scratch).await;
    let isolation = Isolation::detect();
    assert!(matches!(isolation, Isolation::Sandbox(_)), "sandbox-exec");
    for network in [Network::On, Network::Off] {
        for round in 0..3 {
            let started = Instant::now();
            let report = slot.canary(&isolation, network, &lease).await.expect("ran");
            println!(
                "CANARY-LATENCY network={network:?} round={round} both_canaries_ms={}",
                started.elapsed().as_millis()
            );
            assert!(report.all_refused(), "{network:?}: {}", report.failures());
            assert_eq!(report.passed.len(), 2);
        }
    }
    assert!(
        !lease.join(".kbf-canary-link").exists(),
        "the link canary left its link"
    );
    slot.release().await;
}

/// Catches canaries that pass whatever the profile says (a probe that always exits 0, a
/// judge that reads the exit code alone): the generated profile loses its
/// `(deny file-write*)` line, so the user can write anywhere, and the write canary must
/// be reported as not refused, naming the canary and the deny; with the network off
/// too. The file the open door made is removed by the probe.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_its_write_deny_fails_the_canaries() {
    let weakened = WeakenedProfile::Drop("(deny file-write*)");
    let (_users, scratch, slots) = start("canary-weakened", Some(weakened));
    let (slot, lease) = leased(&slots, &scratch).await;
    let isolation = Isolation::detect();
    for network in [Network::On, Network::Off] {
        let report = slot.canary(&isolation, network, &lease).await.expect("ran");
        println!(
            "CANARY-WEAKENED drop-file-write-deny network={network:?}: {}",
            report.failures()
        );
        assert!(
            failed(&report).contains(&"write-outside"),
            "{}",
            report.failures()
        );
        assert!(
            report
                .failures()
                .contains("canary write-outside ((deny file-write*)): NOT refused"),
            "{}",
            report.failures()
        );
    }
    assert!(
        left_in_tmp().is_empty(),
        "the open door's file stayed: {:?}",
        left_in_tmp()
    );
    slot.release().await;
}

/// Catches a link canary that cannot fail on its own (it only follows the write canary's
/// result, or the check stops at the first failure): the profile keeps
/// `(deny file-write*)` but allows writes under `/private/etc`, where the link's source
/// is (the runner showed the link is refused through its source's write rule: an
/// `(allow file-link)` changed nothing), so the write canary must still be refused and
/// the link canary alone reported.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_that_allows_writes_to_the_link_source_fails_the_link_canary_alone() {
    let weakened = WeakenedProfile::Add("(allow file-write* (subpath \"/private/etc\"))");
    let (_users, scratch, slots) = start("canary-link", Some(weakened));
    let (slot, lease) = leased(&slots, &scratch).await;
    let isolation = Isolation::detect();
    for network in [Network::On, Network::Off] {
        let report = slot.canary(&isolation, network, &lease).await.expect("ran");
        println!(
            "CANARY-WEAKENED allow-write-to-the-link-source network={network:?}: {}",
            report.failures()
        );
        assert_eq!(failed(&report), ["link-outside"], "{}", report.failures());
        assert_eq!(report.passed.len(), 1);
    }
    assert!(
        !lease.join(".kbf-canary-link").exists(),
        "the open door's link stayed"
    );
    slot.release().await;
}

/// Catches a slot that fails its canaries and stays offered, stays counted, is never
/// cleaned again, or is handed out again without its canaries run: it is marked dirty as
/// a failed clean marks it (the count of slots drops to 0 and a lease waits, as when none
/// is free), the recovery thread cleans it and offers it again (the count is 1 again),
/// and the canaries run again at the next hand-out, where the same weakened profile fails them
/// again.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_slot_that_fails_its_canaries_is_dirty_and_recovery_offers_it_to_be_canaried_again() {
    let weakened = WeakenedProfile::Drop("(deny file-write*)");
    let (_users, scratch, slots) = start("canary-aside", Some(weakened));
    let (slot, lease) = leased(&slots, &scratch).await;
    let isolation = Isolation::detect();
    assert_eq!(slots.count(), 1);
    let report = slot
        .canary(&isolation, Network::On, &lease)
        .await
        .expect("ran");
    assert!(!report.all_refused());
    slot.set_aside(&report.failures());
    assert_eq!(slots.count(), 0, "a dirty slot is not counted");
    let waited = tokio::time::timeout(Duration::from_secs(1), slots.take()).await;
    assert!(waited.is_err(), "a dirty slot must not be offered");
    // The recovery thread (5 s, then doubling) cleans it and offers it again.
    let until = Instant::now() + Duration::from_secs(60);
    while slots.count() == 0 {
        assert!(
            Instant::now() < until,
            "the recovery thread never freed the slot"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // The clean removed the uid's files under /private/tmp, the lease directory too.
    std::fs::create_dir_all(&lease).expect("lease dir again");
    let again = tokio::time::timeout(Duration::from_secs(5), slots.take()).await;
    let again = again.expect("offered").expect("a slot");
    again.hand_over(&lease).await.expect("hand over");
    let report = again
        .canary(&isolation, Network::On, &lease)
        .await
        .expect("ran");
    assert!(!report.all_refused(), "canaried again, and failed again");
    again.set_aside(&report.failures());
    assert_eq!(slots.count(), 0);
    assert!(left_in_tmp().is_empty(), "{:?}", left_in_tmp());
}
