//! Tests of what the adapter keeps of each worker's stream (`facts`).

use kbf_caps::NodeCaps;
use kbf_types::{Resources, WorkerId};

use super::testing::adapter;
use crate::farm::{Reported, identity};

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn caps(os: &str, arch: &str) -> NodeCaps {
    NodeCaps::from_report([("arch", arch), ("os", os)]).expect("caps")
}

fn reported(os: &str, arch: &str, protocol: Option<u32>) -> Reported {
    Reported {
        protocol,
        ..Reported::serving(Resources::new(4_000, 8 << 30), caps(os, arch))
    }
}

/// Asserts that a worker that has registered has the `os` and `arch` of its report (read by
/// the same rule as the farm's own driver) and the protocol of its `Hello`, and that one
/// that has not registered has none.
#[test]
fn a_registered_worker_has_the_identity_and_protocol_it_reported() {
    let mut adapter = adapter();
    let linux = reported("linux", "x86_64", Some(3));

    adapter.note_registered(&worker("a"), &linux);

    let facts = adapter.worker_facts(&worker("a")).expect("facts");
    let (os, arch) = identity(&linux.caps);
    assert_eq!(facts.os, "linux");
    assert_eq!(facts.os, os);
    assert_eq!(facts.arch, arch);
    assert_eq!(facts.protocol, Some(3));
    assert_eq!(adapter.worker_facts(&worker("b")), None);
}

/// Asserts that a `Hello` that names no `os` leaves it empty and one that names no protocol
/// leaves it unknown, rather than either borrowing from another worker.
#[test]
fn a_hello_that_names_no_protocol_leaves_it_unknown() {
    let mut adapter = adapter();
    adapter.note_registered(&worker("a"), &reported("linux", "x86_64", Some(3)));
    adapter.note_registered(&worker("b"), &reported("macos", "arm64", None));

    assert_eq!(
        adapter.worker_facts(&worker("b")).expect("b").protocol,
        None
    );
    assert_eq!(adapter.worker_facts(&worker("b")).expect("b").os, "macos");
    assert_eq!(
        adapter.worker_facts(&worker("a")).expect("a").protocol,
        Some(3)
    );
}

/// Asserts that a new stream of a worker replaces all three facts, as a new link does.
#[test]
fn a_new_stream_replaces_the_identity_and_the_protocol() {
    let mut adapter = adapter();
    let a = worker("a");
    adapter.note_registered(&a, &reported("linux", "x86_64", Some(3)));

    adapter.note_registered(&a, &reported("macos", "arm64", Some(4)));

    let facts = adapter.worker_facts(&a).expect("facts");
    assert_eq!(facts.os, "macos");
    assert_eq!(facts.arch, identity(&caps("macos", "arm64")).1);
    assert_ne!(facts.arch, identity(&caps("linux", "x86_64")).1);
    assert_eq!(facts.protocol, Some(4));
}

/// Asserts that a resent `Hello` changes the `os` and `arch` and keeps the protocol the
/// first `Hello` was answered with, and that a worker that never registered is not made
/// known by one (it would have an unknown protocol on a stream nobody opened).
#[test]
fn a_resent_hello_changes_the_identity_and_keeps_the_protocol() {
    let mut adapter = adapter();
    let a = worker("a");
    adapter.note_registered(&a, &reported("linux", "x86_64", Some(3)));

    adapter.note_resized(&a, &caps("macos", "arm64"));
    adapter.note_resized(&worker("ghost"), &caps("linux", "x86_64"));

    let facts = adapter.worker_facts(&a).expect("facts");
    assert_eq!(facts.os, "macos");
    assert_eq!(facts.arch, identity(&caps("macos", "arm64")).1);
    assert_eq!(facts.protocol, Some(3));
    assert_eq!(adapter.worker_facts(&worker("ghost")), None);
}

/// Asserts that a forgotten worker has no facts, and the others keep theirs.
#[test]
fn a_forgotten_worker_has_no_facts() {
    let mut adapter = adapter();
    adapter.note_registered(&worker("a"), &reported("linux", "x86_64", Some(3)));
    adapter.note_registered(&worker("b"), &reported("linux", "x86_64", Some(3)));

    adapter.forget_worker(&worker("a"));
    adapter.forget_worker(&worker("never"));

    assert_eq!(adapter.worker_facts(&worker("a")), None);
    assert!(adapter.worker_facts(&worker("b")).is_some());
}

/// Asserts that the daemon process of a worker's newest stream is kept, that a new stream
/// replaces it (and one that names none leaves it empty, as a daemon with no process name
/// is a new session on each stream), and that a worker that never registered is not made
/// known by it.
#[test]
fn the_daemon_process_of_the_newest_stream_is_kept() {
    let mut adapter = adapter();
    let a = worker("a");
    adapter.note_registered(&a, &reported("linux", "x86_64", Some(3)));
    assert_eq!(adapter.worker_facts(&a).expect("facts").instance, "");

    adapter.note_instance(&a, "proc-1");
    adapter.note_instance(&worker("ghost"), "proc-9");
    assert_eq!(adapter.worker_facts(&a).expect("facts").instance, "proc-1");
    assert_eq!(adapter.worker_facts(&worker("ghost")), None);

    adapter.note_registered(&a, &reported("linux", "x86_64", Some(3)));
    assert_eq!(adapter.worker_facts(&a).expect("facts").instance, "");
}
