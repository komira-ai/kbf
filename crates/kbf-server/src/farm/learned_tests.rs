//! `--learned-sizes`: which submissions are keyed, read back from the `op/` rows the
//! farm writes, and what a restart makes of a keyed row.

use std::sync::Arc;

use kbf_front::{Described, Dispatch, Submission};
use kbf_invocation::{BookSource, Evidence};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};

use super::{EpochPolicy, Farm};
use crate::LearnedSizes;
use crate::state_store::StateStore;

fn store() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn farm_over(store: &Arc<Fake>, mode: LearnedSizes) -> Farm {
    let store: Arc<dyn Store> = store.clone();
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(mode)
    .with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// What a submission is like. The defaults are the keyed case: an action of unnamed
/// memory on Linux, instance `main`, program `cc`.
#[derive(Clone)]
struct Job {
    tag: u8,
    instance: &'static str,
    platform: Vec<(&'static str, &'static str)>,
    program: &'static str,
    bare: bool,
    kind: LeaseKind,
    gpus: u64,
    memory: BookSource,
}

impl Job {
    fn new(tag: u8) -> Self {
        Self {
            tag,
            instance: "main",
            platform: vec![("OSFamily", "linux")],
            program: "cc",
            bare: false,
            kind: LeaseKind::Action,
            gpus: 0,
            memory: BookSource::Default,
        }
    }

    fn submit(&self, farm: &Farm) {
        let request = kbf_sched::Request {
            key: ActionKey {
                instance: self.instance.to_owned(),
                action: Digest::new(DigestFunction::Sha256, [self.tag; 32], 1),
            },
            qos: Qos::Ci,
            kind: self.kind,
            resources: Resources::new(1_000, 4 << 30).with_gpus(self.gpus),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::from_platform(self.platform.iter().copied()).unwrap(),
        };
        farm.submit(Submission {
            request,
            call: None,
            described: Described {
                arguments: if self.bare {
                    Vec::new()
                } else {
                    vec![self.program.to_owned()]
                },
                ..Described::default()
            },
            evidence: Evidence {
                memory_source: self.memory,
                ..Evidence::default()
            },
        })
        .expect("submitted");
    }
}

/// The size key of each operation of the store, in operation order: its `opkey/` row, or
/// `None` for an operation with none. The request rows are format 1 whatever the key.
fn keys(store: &Arc<Fake>) -> Vec<Option<u64>> {
    let opkeys: std::collections::BTreeMap<String, Vec<u8>> = store
        .scan_prefix("opkey/")
        .expect("scan")
        .into_iter()
        .map(|(k, row)| (k["opkey/".len()..].to_owned(), row.value))
        .collect();
    store
        .scan_prefix("op/")
        .expect("scan")
        .into_iter()
        .map(|(k, row)| {
            assert!(
                kbf_log::decode(kbf_log::Format::V1, &row.value).is_ok(),
                "a request row is format 1"
            );
            opkeys
                .get(&k["op/".len()..])
                .map(|v| u64::from_be_bytes(v[1..9].try_into().expect("an opkey row")))
        })
        .collect()
}

/// The size keys the scheduler holds its operations with.
fn size_keys(farm: &Farm) -> Vec<Option<u64>> {
    let state = farm.lock();
    state
        .sched
        .unfinished()
        .map(|op| state.sched.size_key(op))
        .collect()
}

fn keyed_with(jobs: &[Job]) -> Vec<Option<u64>> {
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    for job in jobs {
        job.submit(&farm);
    }
    keys(&store)
}

/// Only `on` keys a submission: a plain action of unnamed memory is keyed with it, and
/// with `off` or `shadow` no row is written and nothing is named.
#[test]
fn only_on_keys_a_submission() {
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        let store = store();
        let farm = farm_over(&store, mode);
        Job::new(1).submit(&farm);
        assert_eq!(keys(&store), vec![None], "{mode:?}");
        assert_eq!(size_keys(&farm), vec![None], "{mode:?}");
    }
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    Job::new(1).submit(&farm);
    let [Some(key)] = keys(&store)[..] else {
        panic!("on keys a plain action");
    };
    assert_eq!(
        size_keys(&farm),
        vec![Some(key)],
        "the scheduler holds it too"
    );
}

/// Catches a key given to a submission the floor must not apply to: one whose memory the
/// client named (a hint equal to the default included: it is `Hint`, not `Default`),
/// whose platform names no OS, that is not an `action`, or
/// that books GPUs.
#[test]
fn only_an_unnamed_pinned_plain_action_is_keyed() {
    let named = Job {
        memory: BookSource::Hint,
        ..Job::new(1)
    };
    let unpinned = Job {
        platform: vec![],
        ..Job::new(3)
    };
    let machine = Job {
        kind: LeaseKind::WholeMachine,
        ..Job::new(4)
    };
    let gpu = Job {
        gpus: 1,
        ..Job::new(5)
    };
    assert_eq!(keyed_with(&[named, unpinned, machine, gpu]), vec![None; 4]);
}

/// Catches a key that is the shape alone: a tenant's kill would floor another tenant's
/// same-shaped action (on purpose, by sending the same argv), and a Linux kill would
/// floor a Mac's. Two instances, one shape; two platforms, one shape; and the same
/// instance and platform with another digest share one key.
#[test]
fn the_key_names_the_instance_the_platform_and_the_shape() {
    let other_instance = Job {
        instance: "other",
        ..Job::new(2)
    };
    let mac = Job {
        platform: vec![("OSFamily", "macos")],
        ..Job::new(3)
    };
    let arm = Job {
        platform: vec![("OSFamily", "linux"), ("arch", "arm64")],
        ..Job::new(4)
    };
    let other_shape = Job {
        program: "ld",
        ..Job::new(5)
    };
    let keys = keyed_with(&[
        Job::new(1),
        other_instance,
        mac,
        arm,
        other_shape,
        Job::new(6),
    ]);
    let keys: Vec<u64> = keys.into_iter().map(|k| k.expect("keyed")).collect();
    assert_eq!(keys[0], keys[5], "another digest of the same action");
    for i in 1..5 {
        assert_ne!(keys[0], keys[i], "key {i} equals the first");
    }
}

/// Catches a restart that keeps a size key it should not: `off` (and `shadow`) ignore
/// the key of a row an `on` run queued, so the operation books as before; `on` keeps it.
#[test]
fn a_restart_without_on_ignores_the_size_key_of_a_row() {
    let store = store();
    let first = farm_over(&store, LearnedSizes::On);
    Job::new(1).submit(&first);
    assert!(keys(&store)[0].is_some());
    drop(first);
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        let farm = farm_over(&store, mode);
        let loaded = size_keys(&farm);
        assert_eq!(loaded, vec![None], "{mode:?}");
    }
    let farm = farm_over(&store, LearnedSizes::On);
    assert!(size_keys(&farm)[0].is_some());
}

/// Seeds a row no build reads (a version no one wrote): `off` and `shadow` neither load
/// nor fail on it, and `on` refuses the start rather than guess.
#[test]
fn a_bad_size_key_row_stops_a_start_only_when_on() {
    use kbf_state::{Cond, Txn};
    let store = store();
    drop(farm_over(&store, LearnedSizes::On));
    store
        .txn(
            Txn::new()
                .put("opkey/0000000000000001", vec![9], Cond::Any)
                .put("keyfloor/0000000000000001", vec![9], Cond::Any),
        )
        .expect("write")
        .expect("applied");
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        drop(farm_over(&store, mode));
    }
    let on = Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(LearnedSizes::On);
    let store: Arc<dyn Store> = store;
    let refused = on.with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }));
    assert!(refused.is_err(), "on started over a bad row");
}

/// A worker `name` with room for a 16 GiB lease registered with `farm`.
fn register(farm: &Farm, name: &str) -> kbf_types::WorkerId {
    let worker = kbf_types::WorkerId::new(name);
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    // The receiver lives as long as the test: a dropped one closes the stream.
    std::mem::forget(inbox);
    farm.register(
        &worker,
        kbf_sched::DaemonInstance::new(name),
        kbf_sched::DaemonInstance::new(""),
        crate::farm::phase::Reported::serving(kbf_types::Resources::new(4_000, 64 << 30), caps),
        outbound,
        kbf_proto::worker::ServerMessage::default(),
    );
    worker
}

/// The whole path: a keyed action killed at its own limit through the farm's report
/// input writes a `keyfloor/` row with the requeue; a farm started over the same store
/// with `on` loads it, and a new digest of the same shape books the floor on its first
/// placement; started with `off` it loads none.
#[test]
fn a_kill_through_the_farm_leaves_a_floor_the_next_start_books() {
    use kbf_types::{Failure, Outcome};
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    let node = register(&farm, "node-a");
    Job::new(1).submit(&farm);
    let key = keys(&store)[0].expect("keyed");
    let (operation, lease) = {
        let state = farm.lock();
        let operation = state.sched.unfinished().next().expect("queued");
        (operation, state.sched.leases_on(&node)[0])
    };
    let report = super::Report {
        lease,
        outcome: Outcome::Failed(Failure::OutOfMemory),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, &node, operation, report);
    let rows = store.scan_prefix("keyfloor/").expect("scan");
    assert_eq!(rows.len(), 1, "one key floor row");
    assert_eq!(farm.lock().sched.key_floor(key), Some(16 << 30));
    drop(farm);

    let restarted = farm_over(&store, LearnedSizes::On);
    assert_eq!(restarted.lock().sched.key_floor(key), Some(16 << 30));
    let other = register(&restarted, "node-b");
    Job::new(2).submit(&restarted);
    // Two leases on the new node, both at 16 GiB: the killed action's retry (the store
    // holds it queued) and the new digest, which started at the floor and not at 4 GiB.
    assert_eq!(
        restarted
            .lock()
            .sched
            .booked(&other)
            .map(|b| b.memory_bytes),
        Some(32 << 30),
        "a new digest of the shape starts at the floor"
    );
    drop(restarted);

    let off = farm_over(&store, LearnedSizes::Off);
    assert_eq!(off.lock().sched.key_floor(key), None);
}
