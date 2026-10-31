//! The clock: a stamp from another boot measures nothing, and the host's clock answers.

mod support;

use kbf_step_runner::{Clock, Stamp, SystemClock};
use support::FakeClock;

/// Catches elapsed time carried across a reboot: the monotonic clock restarts, so a stamp of
/// another boot reads as no time having passed rather than a difference of unrelated counters.
#[test]
fn a_stamp_of_another_boot_has_no_elapsed_time() {
    let clock = FakeClock::new();
    let stamp = Stamp::now(&*clock);
    clock.advance(40);
    assert_eq!(stamp.elapsed_secs(&*clock), 40);
    clock.reboot("boot-b");
    clock.advance(1000);
    assert_eq!(stamp.elapsed_secs(&*clock), 0);
}

/// Catches a clock that runs backwards or has no boot id (the record's boot comparison would
/// then treat every boot alike): two reads never decrease and the id is not empty.
#[test]
fn the_system_clock_is_monotonic_and_names_the_boot() {
    let clock = SystemClock;
    let a = clock.mono_secs();
    clock.sleep_secs(0);
    assert!(clock.mono_secs() >= a);
    assert!(!clock.boot_id().is_empty());
    assert_eq!(clock.boot_id(), clock.boot_id());
}
