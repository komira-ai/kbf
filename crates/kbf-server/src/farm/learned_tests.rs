//! `--learned-sizes`: which submissions are keyed, read back from the `op/` rows the
//! farm writes, and what a restart makes of a keyed row.

use std::sync::Arc;

use kbf_front::{Described, Dispatch, Submission};
use kbf_invocation::{BookSource, Evidence};
use kbf_log::{FarmInput, Format, LogCommand};
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

fn farm_over(store: &Arc<Fake>, mode: LearnedSizes, admitted: bool) -> Farm {
    let store: Arc<dyn Store> = store.clone();
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(mode, admitted)
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
            size_key: None,
            needs: kbf_caps::Request::from_platform(self.platform.iter().copied()).unwrap(),
        };
        farm.submit(Submission {
            request,
            call: None,
            described: Described {
                arguments: vec![self.program.to_owned()],
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

/// The size keys of the `op/` rows, in operation order; `None` for an unkeyed row. Each
/// row is read as a format-6 reader reads it, and an unkeyed one must also read as a
/// format-1 reader does.
fn keys(store: &Arc<Fake>) -> Vec<Option<u64>> {
    store
        .scan_prefix("op/")
        .expect("scan")
        .into_iter()
        .map(|(_, row)| {
            let Ok(LogCommand::Farm(batch)) = kbf_log::decode(Format::V6, &row.value) else {
                panic!("an op row that does not read");
            };
            let [FarmInput::Submit(request)] = &batch.inputs[..] else {
                panic!("not a submit");
            };
            assert_eq!(
                kbf_log::decode(Format::V1, &row.value).is_ok(),
                request.size_key.is_none(),
                "an unkeyed row is format 1, a keyed one is not"
            );
            request.size_key
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
    let farm = farm_over(&store, LearnedSizes::On, true);
    for job in jobs {
        job.submit(&farm);
    }
    keys(&store)
}

/// Catches: a keyed submission that is not keyed, the key written when the flag is off
/// or shadow or the log does not admit the field (a binary before format 6 would stop on
/// the row), and a row of an unkeyed submission written in a newer format than before.
///
#[test]
fn only_on_with_the_field_admitted_keys_a_submission() {
    for (mode, admitted) in [
        (LearnedSizes::Off, true),
        (LearnedSizes::Shadow, true),
        (LearnedSizes::On, false),
        (LearnedSizes::Off, false),
    ] {
        let store = store();
        let farm = farm_over(&store, mode, admitted);
        Job::new(1).submit(&farm);
        assert_eq!(keys(&store), vec![None], "{mode:?} {admitted}");
    }
    let [Some(_)] = keyed_with(&[Job::new(1)])[..] else {
        panic!("on and admitted keys a plain action");
    };
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
///
#[test]
fn a_restart_without_on_ignores_the_size_key_of_a_row() {
    let store = store();
    let first = farm_over(&store, LearnedSizes::On, true);
    Job::new(1).submit(&first);
    assert!(keys(&store)[0].is_some());
    drop(first);
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        let farm = farm_over(&store, mode, true);
        let loaded = size_keys(&farm);
        assert_eq!(loaded, vec![None], "{mode:?}");
    }
    let farm = farm_over(&store, LearnedSizes::On, true);
    assert!(size_keys(&farm)[0].is_some());
}
