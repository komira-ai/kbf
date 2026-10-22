//! SIGTERM before SIGKILL, for real, on a Mac: a lease run as a pool user whose workload
//! keeps POSIX shared memory and semaphores the way Python's `multiprocessing` does (the
//! `shm-owner` step of `kbf-act`: names made by the workload, a tracker process that
//! removes them when it is asked to end or when the workload goes away), ended while it
//! runs.
//!
//! - With the grace on (the default), the lease sends SIGTERM first, the tracker unlinks
//!   every name, and none is left. A Mac cannot list these names, so each is looked for by
//!   its own name, as root, by opening it without creating it.
//! - With a grace of zero (SIGKILL at once) the names are left: the tracker dies with the
//!   workload. This is also what shows that the check can see a leak; the test removes
//!   the names itself so that the runner stays clean.
//! - The clean of a slot at the daemon's start does the same for what a uid still runs.
//! - A workload that ignores SIGTERM is killed after the grace and named in the lease's
//!   stderr by pid and arguments, and the action's exit code is its own.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_term -- --test-threads=1
//! --nocapture` (users from uid 690, made at free uids and deleted when the test ends).

#![cfg(target_os = "macos")]

mod support;

use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::{Runtime as _, RuntimeError};
use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::SlotUser;
use kbf_testkit::shm;
use kbf_types::LeaseId;
use support::pool::{Scratch, Users, config};
use support::{MemoryCas, PROMPT, Spec};

/// The number of names `shm-owner` makes for each count: shared memory and semaphores.
const COUNT: u32 = 3;
const NAMES: usize = 2 * COUNT as usize;

fn assert_root() {
    support::pool::assert_root("slot_term");
}

/// A name prefix short enough for macOS's 31 bytes.
fn prefix(tag: &str) -> String {
    format!("kt{}{tag}", std::process::id() % 100_000)
}

/// Waits until every name of `prefix` exists.
async fn names_appear(prefix: &str) {
    let until = Instant::now() + PROMPT * 3;
    while shm::present(prefix, COUNT).len() < NAMES {
        assert!(
            Instant::now() < until,
            "the names of {prefix} never came: {:?}",
            shm::present(prefix, COUNT)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runs `shm-owner` as lease `1.1` on a pool of one user with the given grace, ends it
/// once all its names exist, and returns how the run ended and the names left when the
/// end has returned. Removes what is left.
async fn end_an_owner(tag: &str, grace: Duration) -> (Result<(), RuntimeError>, Vec<String>) {
    assert_root();
    let scratch = Scratch::new(tag);
    let users = Users::new(tag, 1);
    let mut config = config(&scratch, users.pool());
    config.term_grace = grace;
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(NativeRuntime::new(config, Arc::clone(&cas)).expect("runtime"));
    let prefix = prefix(tag);
    shm::unlink(&prefix, COUNT);
    let count = COUNT.to_string();
    let action = Spec::argv(&[&scratch.act(), "shm-owner", &prefix, &count]).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(support::work(1, action, 0)).await }
    });
    names_appear(&prefix).await;
    rt.kill(LeaseId::new(1, 1)).await;
    let left = shm::present(&prefix, COUNT);
    shm::unlink(&prefix, COUNT);
    let outcome = running.await.expect("run task").map(drop);
    (outcome, left)
}

/// Catches: a lease that SIGKILLs a pool user's processes at once, which kills the
/// tracker with the workload so that every shared-memory and semaphore name stays until
/// the machine restarts (a Mac cannot list them, so nothing could remove them later).
/// With the default grace, none is left.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_term"]
#[tokio::test]
async fn a_tracker_unlinks_every_name_when_a_pool_lease_ends() {
    let (outcome, left) = end_an_owner("term-on", kbf_driver_native::DEFAULT_TERM_GRACE).await;
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    assert_eq!(left, Vec::<String>::new(), "names left after the lease");
}

/// Catches: a grace of zero that waits or sends SIGTERM, and a check that cannot see a
/// leak: with SIGKILL at once, all of the names are left.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_term"]
#[tokio::test]
async fn with_a_grace_of_zero_the_names_are_left() {
    let (outcome, left) = end_an_owner("term-off", Duration::ZERO).await;
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    assert_eq!(left.len(), NAMES, "{left:?}");
}

/// Starts `shm-owner` as `user`, outside any lease, and waits for its names.
async fn owner_of(scratch: &Scratch, user: &SlotUser, prefix: &str) -> std::process::Child {
    shm::unlink(prefix, COUNT);
    let owner = Command::new(scratch.act())
        .args(["shm-owner", prefix, &COUNT.to_string()])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .uid(user.uid)
        .gid(user.gid)
        .spawn()
        .expect("the owner");
    names_appear(prefix).await;
    owner
}

/// Catches: a slot clean that kills at once what a uid still runs (the start of a
/// daemon whose predecessor was killed), leaving its names: with the grace the names
/// are gone after the start, with none they are left.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_term"]
#[tokio::test]
async fn the_clean_of_a_slot_gives_the_same_grace() {
    assert_root();
    let scratch = Scratch::new("term-clean");
    let users = Users::new("term-clean", 1);
    let user = &users.0[0];
    let mut outcomes = Vec::new();
    for (tag, grace) in [
        ("g", kbf_driver_native::DEFAULT_TERM_GRACE),
        ("z", Duration::ZERO),
    ] {
        let prefix = prefix(tag);
        let mut owner = owner_of(&scratch, user, &prefix).await;
        let mut config = config(&scratch, users.pool());
        config.term_grace = grace;
        let rt = NativeRuntime::new(config, Arc::new(MemoryCas::default())).expect("runtime");
        let left = shm::present(&prefix, COUNT);
        shm::unlink(&prefix, COUNT);
        let _ = owner.kill();
        let _ = owner.wait();
        drop(rt);
        outcomes.push(left);
    }
    assert_eq!(
        outcomes[0],
        Vec::<String>::new(),
        "after a clean with a grace"
    );
    assert_eq!(
        outcomes[1].len(),
        NAMES,
        "after a clean with none: {:?}",
        outcomes[1]
    );
}

/// Catches: a workload that ignores SIGTERM left out of the result, one that ends on it
/// named as if it had not, a wrong pid or no arguments in the line, the survivor left
/// alive, the action failed for it (its own exit code stands), and a grace that is not
/// waited for or not bounded.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_term"]
#[tokio::test]
async fn a_workload_that_ignores_sigterm_is_named_in_the_result() {
    assert_root();
    let scratch = Scratch::new("term-ignore");
    let users = Users::new("term-ignore", 1);
    let mut config = config(&scratch, users.pool());
    config.term_grace = Duration::from_millis(600);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config, Arc::clone(&cas)).expect("runtime");
    let act = scratch.act();
    let spec = Spec::argv(&[
        &act,
        "spawn-child",
        "sleep",
        "300",
        ";",
        "spawn-child",
        "--pid",
        "-",
        "ignore-signal",
        "15",
        ";;",
        "sleep",
        "300",
        ";",
        "sleep",
        "1",
        ";",
        "exit",
        "3",
    ]);
    let started = Instant::now();
    let result = support::run(&rt, &cas, 1, &spec).await.expect("ran");
    let took = started.elapsed();
    let out = support::stdout(&cas, &result);
    let stderr = support::stderr(&cas, &result);
    let pid: i32 = out
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("no pid in {out:?}"));
    assert_eq!(result.exit_code, 3, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "kbf: 1 process(es) ignored SIGTERM and were killed: pid {pid} "
        )),
        "{stderr}"
    );
    assert!(stderr.contains("ignore-signal 15"), "{stderr}");
    assert!(
        !support::alive(pid),
        "the survivor {pid} outlived the lease"
    );
    assert!(took >= Duration::from_millis(1500), "no grace: {took:?}");
    assert!(took < PROMPT * 2, "the grace was not bounded: {took:?}");
}
