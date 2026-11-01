//! `--cold-store registered`: a storage node whose object store is the frontend's
//! registration and nothing else (`kbf_server::cold_apply`). It registers with a real
//! worker listener over mutual TLS, takes the registered endpoint, bucket, prefix and keys,
//! probes them against S3 services that refuse any other key pair, and reports what it
//! applied in its heartbeats, which a real `Acks` reads back.
//!
//! No test here sleeps or sets a time limit: a wait is a loop that yields until the thing
//! waited for has happened, and a hang is cut by the test run.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/cold_node.rs"]
mod node;
#[path = "support/store_config.rs"]
mod support;

use std::sync::Arc;

use clap::Parser as _;
use kbf_objstore::s3::fake::FakeS3;
use kbf_server::cold_apply::ColdApplyError;
use kbf_server::history::{HistoryError, with_history};
use kbf_server::storage_node::{command, parse, start_storage};
use kbf_server::store_config::acks::{Convergence, NodeStatus};
use kbf_server::{Args, EpochPolicy, StartFailure, StateStore};
use kbf_state::ManualClock;
use kbf_state::Store;
use kbf_state::fake::{Fake, Flaw};
use node::{
    ID, Rig, SECRET, class, dir, flags, flags_at, never, next_port, no_keys, own_keys, promoted,
    registered, reported_status, rig, round_trip, spin_until, started,
};

const REGISTERED: &str = "registered";

/// What a host with its own S3 settings in the environment gives: other keys than the
/// registered ones, and an endpoint that nothing listens on.
fn ambient(name: &str) -> Option<String> {
    match name {
        "AWS_ACCESS_KEY_ID" => Some("AKIAAMBIENT".to_owned()),
        "AWS_SECRET_ACCESS_KEY" => Some("ambient-secret-0d7e".to_owned()),
        "AWS_ENDPOINT_URL" | "AWS_ENDPOINT_URL_S3" => Some("http://127.0.0.1:1".to_owned()),
        _ => None,
    }
}

/// What the frontend's acks say about `generation`, once the first node has reported.
async fn reported_at(rig: &Rig, generation: u64) -> Convergence {
    spin_until(|| {
        let map = promoted(rig)?;
        let got = rig.farm.acks.convergence(&map, generation).ok()?;
        let seen = got
            .nodes
            .first()
            .is_some_and(|node| node.last_seen.is_some());
        seen.then_some(got)
    })
    .await
}

/// Catches a registered node that takes any part of its store from somewhere else than the
/// registration (the environment's endpoint or key pair, or the node's own flags, which
/// name nothing): the environment names other keys and an endpoint that nothing listens on,
/// the registration names the service and the one pair it accepts, and the node writes and
/// reads through its cache on that service, reporting the generation applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_node_runs_on_the_frontends_store_and_nothing_else() {
    let rig = rig(3, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let half = started(&flags(&rig, "only-registered", REGISTERED, ambient))
        .await
        .expect("a node that names no store starts on the registration");
    round_trip(&half, "written on the registered store").await;
    let _serving = tokio::spawn(half.serve());

    let got = reported_at(&rig, 3).await;
    assert_eq!(got.nodes[0].status, NodeStatus::Ok, "{got:?}");
    assert_eq!(got.nodes[0].applied_generation, Some(3));
    assert!(got.converged, "{got:?}");
    let written = spin_until(|| {
        let keys = rig.s3.keys();
        keys.iter().any(|k| k.starts_with("kbf/")).then_some(keys)
    })
    .await;
    assert!(
        written.iter().all(|k| !k.contains(".kbf-probe")),
        "{written:?}"
    );
    let probes = rig
        .s3
        .requests()
        .into_iter()
        .filter(|r| r.contains(".kbf-probe"))
        .count();
    assert_eq!(probes, 3, "one write, one read, one delete");
}

/// Catches a registered node that starts on something when nothing is registered (defect:
/// a guess from the environment, a local directory), one that gives up (an error a restart
/// is needed to retry), and one that joins the farm before it has a store: with no
/// generation the node asks again and again, touches no store and is no member; when a
/// registration appears it starts on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_node_waits_for_a_registration() {
    let rig = rig(0, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let node = flags(&rig, "waits", REGISTERED, own_keys);
    let mut starting = Box::pin(started(&node));
    let asked_twice = spin_until(|| (rig.source.asked() >= 2).then_some(()));
    tokio::select! {
        done = &mut starting => panic!("the node started with nothing registered: {:?}", done.map(drop)),
        () = asked_twice => {}
    }
    assert!(rig.s3.requests().is_empty(), "no store was touched");
    assert!(
        rig.farm.members.current_map().ok().flatten().is_none(),
        "the node joined the farm before it had a store"
    );

    rig.source.set(3);
    let half = starting.await.expect("starts once something is registered");
    round_trip(&half, "written on the store registered later").await;
    let _serving = tokio::spawn(half.serve());
    let got = reported_at(&rig, 3).await;
    assert_eq!(got.nodes[0].status, NodeStatus::Ok, "{got:?}");
    assert_eq!(got.nodes[0].applied_generation, Some(3));
}

/// Catches a registered node that moves to a new registration while it runs (defect: the
/// store swapped without a restart), one that does not say it has not (defect: `ok` for
/// the old generation), and one that starts again on the store it ran on: while it runs it
/// writes to the first service and reports `pending` for generation 4; started again over
/// the same directory it runs on the second service, which gets its probe and its writes,
/// and the first is not probed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_node_takes_a_new_registration_at_its_next_start() {
    let rig = rig(3, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let second = FakeS3::start().await;
    second.require_credentials(ID, SECRET);
    let (d, port) = (dir("swap"), next_port());

    let first = started(&flags_at(&rig, (d.clone(), port), REGISTERED, no_keys))
        .await
        .expect("starts on the first registration");
    round_trip(&first, "written on the first store").await;
    let serving = tokio::spawn(first.serve());
    let got = reported_at(&rig, 3).await;
    assert_eq!(got.nodes[0].applied_generation, Some(3), "{got:?}");

    rig.source.serve(registered(&second, "bkt", ID, SECRET));
    rig.source.set(4);
    spin_until(|| (reported_status(&rig, 1) == Some(2)).then_some(())).await;
    assert!(
        second.requests().is_empty(),
        "a running node does not move to the new store"
    );
    let probed = |s3: &FakeS3| {
        s3.requests()
            .into_iter()
            .filter(|r| r.contains(".kbf-probe"))
            .count()
    };
    assert_eq!(
        probed(&rig.s3),
        3,
        "the first store was probed at the start"
    );

    serving.abort();
    let _ = serving.await;
    let again = started(&flags_at(&rig, (d, port), REGISTERED, no_keys))
        .await
        .expect("starts on the new registration");
    round_trip(&again, "written on the second store").await;
    let _serving = tokio::spawn(again.serve());

    let done = spin_until(|| {
        let map = promoted(&rig)?;
        let got = rig.farm.acks.convergence(&map, 4).ok()?;
        let node = got.nodes.first()?;
        (node.status == NodeStatus::Ok && node.applied_generation == Some(4)).then_some(got)
    })
    .await;
    assert!(done.converged, "{done:?}");
    let written = spin_until(|| {
        let keys = second.keys();
        keys.iter().any(|k| k.starts_with("kbf/")).then_some(keys)
    })
    .await;
    assert!(
        written.iter().all(|k| !k.contains(".kbf-probe")),
        "{written:?}"
    );
    assert_eq!(
        probed(&second),
        3,
        "the new store was probed once, at the start"
    );
    assert_eq!(probed(&rig.s3), 3, "the first store was not probed again");
}

/// Catches `--cold-store-credentials-file` accepted beside `registered` (defect: the check
/// skips a registered store), where it would be a second place a store's keys come from: the
/// start is refused before it asks the frontend or touches a store, exit 2, and names the
/// flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_with_a_credentials_file_is_refused() {
    let rig = rig(3, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let mut node = flags(&rig, "with-file", REGISTERED, own_keys);
    node.server.cold_store_credentials_file = Some(dir("with-file-key").join("cold.json"));
    let Err(error) = started(&node).await else {
        panic!("a registered node started with a credentials file");
    };
    let said = error.to_string();
    assert!(said.contains("--cold-store-credentials-file"), "{said}");
    assert!(said.contains("registered"), "{said}");
    assert_eq!(class(&*error), StartFailure::Refused, "{said}");
    assert_eq!(rig.source.asked(), 0, "the frontend was not asked");
    assert!(rig.s3.requests().is_empty());
}

/// Catches a registered node that falls back to keys it finds (defect: the environment's pair
/// tried when the probe fails): the registration holds a wrong secret and the environment
/// holds the pair the service accepts; the node does not start, the error is the probe's
/// (a restart can clear it: exit 75) and holds no secret, and the service saw the one refused
/// write and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_keys_that_fail_the_probe_never_fall_back() {
    let rig = rig(4, |s3| registered(s3, "bkt", ID, "wrong-secret-SENTINEL")).await;
    let Err(error) = started(&flags(&rig, "no-fallback", REGISTERED, own_keys)).await else {
        panic!("a registered node started on keys that fail the probe");
    };
    let refusal = error
        .downcast_ref::<ColdApplyError>()
        .unwrap_or_else(|| panic!("an apply error: {error}"));
    assert!(matches!(refusal, ColdApplyError::Probe(_)), "{refusal:?}");
    assert_eq!(class(&*error), StartFailure::NotReady);
    assert!(!format!("{error:?}{error}").contains("SENTINEL"));
    let probes: Vec<String> = rig
        .s3
        .requests()
        .into_iter()
        .filter(|r| r.contains(".kbf-probe"))
        .collect();
    assert_eq!(probes.len(), 1, "the refused write only: {probes:?}");
}

/// Catches a registered node with nothing to ask (defect: it waits for ever on no
/// frontend): without `--frontend` the start is refused at once, exit 2, naming the flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_without_a_frontend_is_refused() {
    let d = dir("no-frontend");
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store={REGISTERED}"),
        format!("--meta-dir={}", d.join("meta").display()),
    ];
    let flags = parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken");
    let Err(error) = started(&flags).await else {
        panic!("a registered node started with no frontend");
    };
    let said = error.to_string();
    assert!(said.contains("--frontend"), "{said}");
    assert_eq!(class(&*error), StartFailure::Refused, "{said}");
}

/// Catches `kbf storage` (not the storage half of `storage-and-daemon`, which has its own
/// start) not taking the registered store: the same node, started as `kbf storage` starts
/// it, runs on the registered service and reports it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kbf_storage_takes_the_registered_store_too() {
    let rig = rig(5, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let node = flags(&rig, "kbf-storage", REGISTERED, ambient);
    let handle = start_storage(&node, never)
        .await
        .expect("starts on the registration");
    let _serving = tokio::spawn(handle.serve());
    let got = reported_at(&rig, 5).await;
    assert_eq!(got.nodes[0].status, NodeStatus::Ok, "{got:?}");
    assert_eq!(got.nodes[0].applied_generation, Some(5));
    assert!(
        rig.s3.requests().iter().any(|r| r.contains(".kbf-probe")),
        "the registered service was probed"
    );
}

/// Catches a server that is not a storage node taking `registered` (defect: it opens a store
/// named by nothing): the frontend and all-in-one starts refuse it, exit 2, saying what takes
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_is_not_a_storage_node_refuses_registered() {
    let d = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cold-registered-server");
    let args = Args::try_parse_from([
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store={REGISTERED}"),
        format!("--meta-dir={}", d.join("meta").display()),
    ])
    .expect("flags");
    let Err(error) = kbf_server::start(&args, || Ok(std::future::pending::<()>())).await else {
        panic!("a server with no storage role started on registered");
    };
    let said = error.to_string();
    assert!(said.contains("--cold-store registered"), "{said}");
    assert!(said.contains("storage node"), "{said}");
    assert_eq!(class(&*error), StartFailure::Refused, "{said}");
}

/// Catches `registered` accepted as the history store (defect: the history match falls
/// through to the cold store's arm): history is an object store of its own, so the word is
/// refused naming the flag.
#[test]
fn registered_is_not_a_history_store() {
    let args = Args::try_parse_from([
        "kbf-server",
        "--state-store",
        "sqlite:///var/state.db",
        "--history-objects",
        REGISTERED,
    ])
    .expect("parses");
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    };
    let Err(refused) = with_history(Some(state), &args.history) else {
        panic!("history opened on registered");
    };
    let said = refused.to_string();
    assert!(said.contains("--history-objects"), "{said}");
    assert!(said.contains("registered"), "{said}");
    assert!(!matches!(refused, HistoryError::NeedsStateStore), "{said}");
}
