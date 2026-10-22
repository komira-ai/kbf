//! How a Mac behaves with a pool of 100 and of 1000 users (a measurement spike; it
//! asserts nothing about the numbers, only that the harness ran and left nothing behind).
//!
//! As root, it makes the users `kbf-scale-0` to `kbf-scale-999` (and 1000 to 1019 for one
//! comparison) in the fixed uid block 61000 to 62023 (the way the pool's users are made:
//! one `dscl` per attribute, a home each), and at 100 and again at 1000 users prints
//! `MEASURE` lines for:
//!
//! - `create`: how long making them took, per user and in all, and how many programs;
//! - `create_one_dscl_process`: at 1000, ten more users one `dscl` each against ten written
//!   through one `dscl .` process reading its commands from standard input (users 1000 to
//!   1019, which the clean-up removes with the rest);
//! - `visible`: how long until `getpwnam` in this process answers for every one;
//! - `lookup`: directory lookups (`getpwnam` hit and miss, `id`, `dscl -read`) against a
//!   program that does nothing, with no test users and at each count; `dscl -list` and
//!   `-readall` of every user; the size of `opendirectoryd`;
//! - `check`: what the pool's per-user check spends (`kbf-mac-provision` `pool.rs`);
//! - `reset`: one full reset of today's slot clean (`Slot::release`) for each of 20 users,
//!   with state planted and without, after the start (`Slots::start`) that resolves and
//!   cleans each once.
//!
//! Then it removes every user, home and `/var/folders` entry it made, and what a reset's
//! recreate moved aside of them (`.kbf-recreated-kbf-scale-<i>-*`, root's, beside the home
//! and in `/var/folders/<xx>/`). It does so when the test ends, when it panics (a drop
//! guard), and at the start of the next run, which finds its own leftovers by name, uid,
//! real name and home and refuses to run, touching nothing, if anything else holds a name
//! or id of the block, or if the directory of the homes is a symbolic link (`block.rs`;
//! the rules are tested against a fake Mac on every platform, `block/tests.rs`). Every
//! program it starts (`dscl` first) is killed, and the phase fails, if it has not exited
//! within a minute (`spawn.rs`).
//!
//! Run by hand on a branch (`workflow_dispatch` of spike-slot-reach.yml, job
//! `pool-scale`), or as root with `cargo xtask sudo-test -p kbf-driver-native --test pool_scale_spike -- --nocapture`.
//! The test lives with `slot_clean.rs` because the reset it times is `Slots`, which only
//! this crate reaches.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod block;
mod dscl;
#[cfg(target_os = "macos")]
mod mac;
mod spawn;
#[cfg(target_os = "macos")]
mod spike;
mod stats;

// The `__as-user` step `Slots::start` runs through this test binary, and the keepers.
#[path = "../support/mod.rs"]
mod support;

#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs root and makes up to 1000 real users: cargo xtask sudo-test -p kbf-driver-native --test pool_scale_spike -- --nocapture"]
fn pool_scale_spike() {
    support::pool::assert_root("pool_scale_spike");
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();
    let mac = mac::RealMac::default();
    // What an earlier run left goes first; anything not ours stops the run untouched.
    let before = block::start(&mac).unwrap_or_else(|why| panic!("{why}"));
    println!(
        "MEASURE start_sweep users={} homes={} folders={} group={}",
        before.users, before.homes, before.folders, before.group
    );
    // Made before anything is: the users go when this test ends however it ends.
    let guard = block::Guard::new(&mac);
    let mut spike = spike::Spike::new(&mac);
    spike.run();
    let cleaned = guard.finish();
    let mut extra = Vec::new();
    let mut problems = std::mem::take(&mut spike.failures);
    match &cleaned {
        Ok(s) => {
            println!(
                "MEASURE cleanup users={} homes={} folders={} group={} took_s={:.2} errors={:?} foreign={:?}",
                s.users,
                s.homes,
                s.folders,
                s.group,
                s.took.as_secs_f64(),
                s.errors,
                s.foreign
            );
            extra.push((
                "cleanup_s".to_owned(),
                format!("{:.1}", s.took.as_secs_f64()),
            ));
            problems.extend(s.errors.iter().cloned());
        }
        Err(why) => problems.push(format!("cleanup: {why}")),
    }
    let left = block::leftovers(&mac);
    problems.extend(left.iter().map(|l| format!("left behind: {l}")));
    spike.failures = problems.clone();
    spike.print_summary(&extra);
    assert!(
        problems.is_empty(),
        "the harness did not run clean:\n{}",
        problems.join("\n")
    );
}
