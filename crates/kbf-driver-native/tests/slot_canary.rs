//! The profile canaries for real (`~/workplace/mac-pool-experiments-scratch/08_revised_design.md`,
//! section 1.6): a pool user, under the exact profile generated for a lease, is refused
//! a write outside the four allowed paths and a hard link of a root-owned file into the
//! lease directory, and a profile that lacks `(deny file-write*)` fails them.
//!
//! The canary latency is printed as `CANARY-LATENCY` lines (the design measured 64 to 125
//! ms for a run).
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_canary`, with the users of
//! `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_driver_native::network::{Isolation, Network, ProfileMutant};
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

fn start(name: &str, mutant: Option<ProfileMutant>) -> (Users, Scratch, Slots) {
    assert_root();
    let users = Users::new(name, 1);
    let scratch = Scratch::new(name);
    let pool = Pool::new(users.0.clone()).expect("pool");
    let within = Duration::from_secs(10);
    let slots = match mutant {
        None => Slots::start(&pool, within),
        Some(m) => Slots::start_profile_mutated(&pool, within, m),
    };
    (users, scratch, slots.expect("slots"))
}

/// Catches a generated profile that lets a pool user write outside the lease, `/dev`,
/// its home and its `/var/folders` entry, or hard-link a root-owned file into the lease
/// directory (both with and without the network), and a canary that cannot run on the
/// Mac at all (a probe mode the binary does not answer, a launch that fails): every
/// canary must be refused, and each run is timed. Mutant seen red on the runner:
/// [`a_profile_without_its_write_deny_fails_the_canaries`].
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
            slot.canary(&isolation, network, &lease)
                .await
                .unwrap_or_else(|e| panic!("{network:?}: {e}"));
            println!(
                "CANARY-LATENCY network={network:?} round={round} both_canaries_ms={}",
                started.elapsed().as_millis()
            );
        }
    }
    assert!(
        !Path::new(&lease).join(".kbf-canary-link").exists(),
        "the link canary left its link"
    );
    slot.release().await;
}

/// Catches canaries that pass whatever the profile says (a probe that always exits 0, a
/// judge that reads the exit code alone): the generated profile loses its
/// `(deny file-write*)` line, so the user can write anywhere, and the first canary must
/// report the write as not refused, naming the canary and the deny; with the network
/// off too. The file the open door made is removed by the probe.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_its_write_deny_fails_the_canaries() {
    let mutant = ProfileMutant("(deny file-write*)");
    let (_users, scratch, slots) = start("canary-mutant", Some(mutant));
    let (slot, lease) = leased(&slots, &scratch).await;
    let isolation = Isolation::detect();
    for network in [Network::On, Network::Off] {
        let failed = slot
            .canary(&isolation, network, &lease)
            .await
            .expect_err("the canaries must fail");
        let text = failed.to_string();
        println!("CANARY-MUTANT network={network:?}: {text}");
        assert!(
            text.contains("canary write-outside ((deny file-write*)): NOT refused"),
            "{text}"
        );
    }
    let left: Vec<_> = std::fs::read_dir("/private/tmp")
        .expect("tmp")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".kbf-canary-"))
        .collect();
    assert!(left.is_empty(), "the open door's file stayed: {left:?}");
    slot.release().await;
}
