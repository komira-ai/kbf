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

use kbf_daemon::{LeaseReading, LeaseUsage, NodeReading, Runtime};
use kbf_driver_container::{MonoClock, NodeLoad};
use kbf_proto::worker;
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
/// handed over once. Also catches the reading a heartbeat carries losing, swapping or
/// misnaming a field: a reading through the driver's `Runtime` hook while the lease runs
/// must hold every value of the actions cgroup's files and the covered lease. And catches the
/// reading not naming the running lease with the start the tracker took and the counters
/// of its own cgroup (its `cpu.stat`, and `memory.current` less `inactive_file`), or taking
/// the clock a second time for the lease's memory files.
#[tokio::test]
async fn a_lease_is_covered_from_the_second_reading_and_leaves_with_its_peak() {
    let fake = Fake::new("node-load-hooks");
    let actions = fake.cgroup.join("actions");
    for (file, text) in [
        ("cpu.stat", "usage_usec 4242\nthrottled_usec 3\n"),
        ("memory.current", "1000\n"),
        ("memory.stat", "inactive_file 0\nactive_file 7\n"),
        ("cpu.max", "200000 100000\n"),
        (
            "cpu.pressure",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=9\n",
        ),
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
        write("memory.current", &["3000"]),
        write("memory.stat", &["inactive_file 1000", "active_file 5"]),
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
        // The reading as a heartbeat carries it, through the driver's `Runtime` hook.
        let sent = Runtime::node_load(&runtime)
            .await
            .expect("a heartbeat reading")
            .expect("the container driver has a reading");
        std::fs::write(cgroup.join("release"), "").expect("release the action");
        (first, second, sent)
    };
    let run = runtime.run(work(1, digest, Resources::new(2000, 1 << 30)));
    let (outcome, (first, second, sent)): (_, (NodeLoad, NodeLoad, NodeReading)) =
        tokio::join!(run, readings);
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

    assert_eq!(
        sent.load,
        worker::NodeLoad {
            seq: second.seq + 1,
            // The clock moves by ten on each call: the second reading took it twice (itself,
            // then the lease's `cpu.stat`), so this reading's own call is twenty on.
            mono_usec: second.mono_usec + 20,
            cpu_usage_usec: 4242,
            memory_working_set_bytes: 1000,
            covered: vec![worker::LeaseId { term: 1, seq: 1 }],
            actions_cpu_limited: true,
            cpu_throttled_usec: Some(3),
            cpu_pressure_some_total_usec: Some(9),
            memory_active_file_bytes: 7,
            lease_clocks: Vec::new(),
            minutes: Vec::new(),
        }
    );

    // The lease's start is the clock when `crun run` was spawned: before the first reading
    // that held it, and the same at every reading. Its cpu is what its `cpu.stat` said at
    // each reading, and its working set 3000 - 1000.
    let started = first
        .leases
        .first()
        .expect("the first reading names the lease")
        .started_usec;
    assert!(started <= first.mono_usec);
    let lease_at = |cpu_usage_usec| LeaseReading {
        lease,
        started_usec: started,
        usage: Some(LeaseUsage {
            cpu_usage_usec,
            working_set_bytes: 2000,
        }),
    };
    assert_eq!(first.leases, [lease_at(0)]);
    assert_eq!(second.leases, [lease_at(5_000_000)]);
    assert_eq!(sent.leases, [lease_at(5_000_000)]);

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
