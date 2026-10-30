//! What reaping the action's leader reports (`kbf_driver_native::leader`): the CPU of the
//! leader and of the descendants it reaped, the same figure the kernel adds to this
//! process's `RUSAGE_CHILDREN`. This binary holds this one test, so nothing else in the
//! process reaps a child and the two can be compared exactly (`cargo test` and nextest
//! both run it alone).

mod support;

use std::process::Command;

use kbf_driver_native::leader::Leader;

fn children_micros() -> u64 {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // FFI-BOUNDARY: `ru` is this frame's; getrusage fills it and keeps no pointer.
    // SAFETY: `ru` is a writable rusage; RUSAGE_CHILDREN is a valid `who`.
    let done = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, ru.as_mut_ptr()) };
    assert_eq!(done, 0);
    // SAFETY: the call succeeded, so it filled the struct.
    let ru = unsafe { ru.assume_init() };
    let micros = |t: libc::timeval| {
        u64::try_from(t.tv_sec).expect("seconds") * 1_000_000
            + u64::try_from(t.tv_usec).expect("microseconds")
    };
    micros(ru.ru_utime) + micros(ru.ru_stime)
}

/// Catches: a leader reaped without its usage (zero CPU reported for an action that
/// burned some), and a figure that is not the kernel's. The leader burns CPU for 0.2 s,
/// then runs a child of its own that burns for 0.2 s and waits for it: the figure is the
/// leader's own CPU plus the child's it reaped, and equals the kernel's count for the
/// reap.
#[tokio::test]
async fn the_reaped_leader_reports_the_cpu_of_itself_and_the_children_it_reaped() {
    let act = support::act();
    let child = Command::new(&act)
        .args(["burn", "0.2", ";", "run"])
        .arg(&act)
        .args(["burn", "0.2"])
        .spawn()
        .expect("spawn");
    let mut leader = Leader::new(child);
    let before = children_micros();
    let reaped = leader.wait().await.expect("reap");
    let kernel = children_micros() - before;
    assert!(reaped.status.success());
    assert!(kernel > 0, "the burns used no CPU the kernel counted");
    assert_eq!(reaped.user_micros + reaped.system_micros, kernel);
}
