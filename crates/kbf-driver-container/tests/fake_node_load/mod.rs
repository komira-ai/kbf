//! The node-load hooks of the runtime, against the stand-in `crun` (`support/fake.rs`)
//! and a plain directory standing in for the cgroup mount: a real lease runs through
//! `ContainerRuntime::run` while the test takes readings with `node_load`, on a clock the
//! test owns.
//!
//! A module of `fake_crun.rs`, not a test binary of its own, like `fake_usage`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::Runtime;
use kbf_driver_container::{MonoClock, NodeLoad};
use kbf_types::{LeaseId, Resources};

use crate::support::fake::{Fake, image};
use crate::support::{Spec, store_action, work};

/// The act steps of an action that writes `lines` to the lease cgroup's `file`.
fn write(file: &str, lines: &[&str]) -> Vec<String> {
    let mut steps = vec!["write-lines".to_owned(), format!("$CG/{file}")];
    steps.extend(lines.iter().map(|l| (*l).to_owned()));
    steps
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| (*s).to_owned()).collect()
}

/// Replaces `dir/file` whole, so a reader never sees it half written.
fn replace(dir: &Path, file: &str, text: &str) {
    let part = dir.join(format!("{file}.part"));
    std::fs::write(&part, text).expect("write");
    std::fs::rename(&part, dir.join(file)).expect("rename");
}

/// Catches the runtime not telling its load tracker when a lease's container starts
/// (the lease is never covered and has no peak) or when its run ends (the lease stays
/// covered after it is gone and its peak is never kept). The lease is held by a file the
/// test makes; the clock moves by ten on every call and counts its calls, so the reading
/// that first holds the lease is the one that read the lease's `cpu.stat` too. The lease
/// is not covered at that reading, is at the next, and is not after the run; its peak is
/// handed over once.
#[tokio::test]
async fn a_lease_is_covered_from_the_second_reading_and_leaves_with_its_peak() {
    let fake = Fake::new("node-load-hooks");
    let actions = fake.cgroup.join("actions");
    for (file, text) in [
        ("cpu.stat", "usage_usec 0\n"),
        ("memory.current", "1000\n"),
        ("memory.stat", "inactive_file 0\n"),
    ] {
        replace(&actions, file, text);
    }
    let now = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let clock: MonoClock = {
        let (now, calls) = (Arc::clone(&now), Arc::clone(&calls));
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            now.fetch_add(10, Ordering::SeqCst) + 10
        })
    };
    let runtime = fake.restart().expect("runtime").with_clock(clock);

    let spec = Spec::new(&image(), "unused");
    let action = [
        write("cpu.stat", &["usage_usec 0"]),
        write("ready", &[]),
        words(&["wait-for", "$CG/release", "600"]),
        words(&["exit", "0"]),
    ]
    .into_iter()
    .reduce(|mut all, part| {
        all.push(";".to_owned());
        all.extend(part);
        all
    })
    .expect("steps");
    fake.action(&action);
    let digest = store_action(&fake.cas, &spec);
    let lease = LeaseId::new(1, 1);
    let cgroup = fake.lease_cgroup(1);

    // The readings, while the lease runs. Nothing waits on a deadline: a broken hook shows
    // in the asserts below, and a hang is the test runner's to report.
    let readings = async {
        let before = runtime
            .node_load()
            .await
            .expect("a reading before the lease");
        assert!(before.covered.is_empty());
        while !cgroup.join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The lease is tracked from the moment `crun run` is spawned, with no await between,
        // and the action writes `ready` only once it runs: so the first reading after
        // `ready` has the lease. It takes the clock twice, once for the reading and once for
        // the lease's `cpu.stat`.
        let before_calls = calls.load(Ordering::SeqCst);
        let first = runtime.node_load().await.expect("a reading");
        assert_eq!(
            calls.load(Ordering::SeqCst) - before_calls,
            2,
            "the reading after the action ran did not read the lease: it was never tracked"
        );
        replace(&cgroup, "cpu.stat", "usage_usec 5000000\n");
        let second = runtime.node_load().await.expect("the second reading");
        std::fs::write(cgroup.join("release"), "").expect("release the action");
        (first, second)
    };
    let run = runtime.run(work(1, digest, Resources::new(2000, 1 << 30)));
    let (outcome, (first, second)): (_, (NodeLoad, NodeLoad)) = tokio::join!(run, readings);
    outcome.expect("ran");
    fake.assert_clean(1);

    assert!(
        !first.covered.contains(&lease),
        "covered at the first reading after it started: {first:?}"
    );
    assert!(second.mono_usec > first.mono_usec);
    assert!(
        second.covered.contains(&lease),
        "not covered at the second reading: {second:?}"
    );

    let after = runtime.node_load().await.expect("a reading after the run");
    assert!(
        !after.covered.contains(&lease),
        "still covered after it ended: {after:?}"
    );
    let peak = runtime.take_peak_window_millicores(lease);
    assert!(
        peak.is_some_and(|millicores| millicores > 0),
        "no peak for a lease that was read twice while it ran: {peak:?}"
    );
    assert_eq!(runtime.take_peak_window_millicores(lease), None);
}
