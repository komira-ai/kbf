//! A storage node applies the cold-store registration at its start
//! (`kbf_server::cold_apply`): it registers with a real worker listener over mutual TLS,
//! fetches the registered store, probes its keys against an S3 service that refuses any
//! other key pair, opens its cache on them, and reports what became of the registration
//! in its heartbeats, which a real `Acks` reads back.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/cold_node.rs"]
mod node;
#[path = "support/store_config.rs"]
mod support;

use std::path::{Path, PathBuf};

use kbf_objstore::s3::fake::FakeS3;
use kbf_server::StartFailure;
use kbf_server::cold_apply::ColdApplyError;
use kbf_server::store_config::acks::NodeStatus;
use kbf_state::Store as _;
use node::{
    BEAT, ID, Rig, SECRET, bucket, class, dir, dir_of, flags, no_keys, own_keys, promoted,
    registered, reported, reported_status, rig, round_trip, started, until,
};
use support::{Source, farm_beating};

/// Catches a node that ignores the registration (mutant: `registered` returns the node's
/// own store, which this node does not have: it cannot start), one that opens its cache
/// on the flag's keys instead of the registered ones (the service refuses any other
/// pair, so the write fails), one whose heartbeats omit the two fields (mutant:
/// `for_beat` returns `(0, Unspecified)`: the frontend shows the node `unsupported`), one
/// that reports the generation before it ran the probe (the probe object is gone and the
/// signed requests are the registered pair's), and a probe that leaves its object (mutant:
/// the delete is skipped).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_with_no_keys_of_its_own_runs_on_the_registered_ones_and_reports_it() {
    let rig = rig(3, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let half = started(&flags(&rig, "registered", &bucket(&rig), no_keys))
        .await
        .expect("a node with only the registration starts");
    round_trip(&half, "written on the registered keys").await;
    let _serving = tokio::spawn(half.serve());

    let got = reported(&rig, 3).await;
    let node = &got.nodes[0];
    assert_eq!(node.status, NodeStatus::Ok, "{got:?}");
    assert_eq!(node.applied_generation, Some(3));
    assert!(
        got.converged && got.applied == 1 && got.expected == 1,
        "{got:?}"
    );

    let keys = until("the node's write in the store", || {
        let keys = rig.s3.keys();
        keys.iter().any(|k| k.starts_with("kbf/")).then_some(keys)
    })
    .await;
    assert!(
        keys.iter().all(|k| !k.contains(".kbf-probe")),
        "the probe object is gone: {keys:?}"
    );
    let probes: Vec<String> = rig
        .s3
        .requests()
        .into_iter()
        .filter(|r| r.contains(".kbf-probe"))
        .collect();
    assert_eq!(
        probes.len(),
        3,
        "one write, one read, one delete: {probes:?}"
    );
    assert!(probes.iter().all(|r| r.contains("/bkt/kbf/.kbf-probe/")));
}

/// Catches a node that reports the registered generation although its keys failed the
/// probe (mutant: `registered` returns `Applied::running` before the probe's result is
/// read), and one that opens the cache on the keys that failed (the service takes only the
/// flag's pair): the node keeps the store its own keys open, writes and reads through it,
/// and tells the frontend `probe_failed` with nothing applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_keys_that_fail_the_probe_leave_the_node_on_its_own_store() {
    let rig = rig(4, |s3| registered(s3, "bkt", ID, "wrong-secret")).await;
    let half = started(&flags(&rig, "badkeys", &bucket(&rig), own_keys))
        .await
        .expect("the node keeps its own store");
    round_trip(&half, "written on the node's own keys").await;
    let _serving = tokio::spawn(half.serve());

    let got = reported(&rig, 4).await;
    assert!(!got.converged, "{got:?}");
    assert_eq!(got.nodes[0].status, NodeStatus::ProbeFailed);
    assert_eq!(
        got.nodes[0].applied_generation,
        Some(0),
        "nothing of generation 4 was applied"
    );
    let probes: Vec<String> = rig
        .s3
        .requests()
        .into_iter()
        .filter(|r| r.contains(".kbf-probe"))
        .collect();
    assert_eq!(
        probes.len(),
        1,
        "the refused write, and nothing after: {probes:?}"
    );
    assert!(rig.s3.keys().iter().all(|k| !k.contains(".kbf-probe")));
}

/// Catches a node that starts without any working keys (mutant: the failed probe falls
/// through to the cache): it refuses, with the probe's step, as an error a restart can
/// clear (75), and holds the wrong secret in no text of the error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_keys_that_fail_the_probe_and_no_keys_of_its_own_is_a_start_to_retry() {
    let rig = rig(4, |s3| registered(s3, "bkt", ID, "wrong-secret-SENTINEL")).await;
    let Err(error) = started(&flags(&rig, "nokeys", &bucket(&rig), no_keys)).await else {
        panic!("a node with nothing that works starts");
    };
    let refusal = error
        .downcast_ref::<ColdApplyError>()
        .unwrap_or_else(|| panic!("an apply error: {error}"));
    assert!(matches!(refusal, ColdApplyError::Probe(_)), "{refusal:?}");
    assert_eq!(class(error.as_ref()), StartFailure::NotReady);
    assert!(!format!("{error:?}{error}").contains("SENTINEL"));
}

/// Catches a node re-pointed at the registered store when its flag names another
/// (mutant: `same_store` returns true): the node keeps its flag store and reports
/// `identity_mismatch`; the registered bucket gets no request at all, not even a probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_for_another_store_leaves_the_node_on_its_flag() {
    let rig = rig(5, |s3| registered(s3, "other-bucket", ID, SECRET)).await;
    let half = started(&flags(&rig, "mismatch", &bucket(&rig), own_keys))
        .await
        .expect("the node keeps its flag store");
    round_trip(&half, "written on the flag's store").await;
    let _serving = tokio::spawn(half.serve());

    let got = reported(&rig, 5).await;
    assert_eq!(got.nodes[0].status, NodeStatus::IdentityMismatch);
    assert_eq!(got.nodes[0].applied_generation, Some(0));
    let requests = rig.s3.requests();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|r| !r.contains("other-bucket")),
        "{requests:?}"
    );
    assert!(requests.iter().all(|r| !r.contains(".kbf-probe")));
}

/// Catches a node with no keys of its own and a mismatching flag that starts anyway
/// (mutant: `MismatchNoKeys` replaced by opening the registered store): it has nothing to
/// run on that its flag names, so it refuses, and a restart reads the same (2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_for_another_store_and_no_keys_of_its_own_is_refused() {
    let rig = rig(5, |s3| registered(s3, "other-bucket", ID, SECRET)).await;
    let Err(error) = started(&flags(&rig, "mismatch-nokeys", &bucket(&rig), no_keys)).await else {
        panic!("a node with nothing to run on starts");
    };
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::MismatchNoKeys)
    ));
    assert_eq!(class(error.as_ref()), StartFailure::Refused);
}

/// Catches a node with a `file://` cold store that takes an S3 registration (mutant:
/// `Fixed::registered` reports `running`): it keeps its directory and says
/// `identity_mismatch`; with nothing registered it says nothing is wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_on_a_local_directory_reports_a_registration_as_not_its_store() {
    let rig = rig(6, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let local = format!("file://{}", dir("local-cold").join("cold").display());
    let half = started(&flags(&rig, "local", &local, no_keys))
        .await
        .expect("starts on its directory");
    round_trip(&half, "written to the directory").await;
    let _serving = tokio::spawn(half.serve());
    let got = reported(&rig, 6).await;
    assert_eq!(got.nodes[0].status, NodeStatus::IdentityMismatch);
    assert!(
        rig.s3.requests().is_empty(),
        "the node never touched the S3 service"
    );

    let quiet = rig_with_nothing().await;
    let half = started(&flags(
        &quiet,
        "local-quiet",
        &local_dir("local-quiet2"),
        no_keys,
    ))
    .await
    .expect("starts");
    let _serving = tokio::spawn(half.serve());
    let got = reported(&quiet, 0).await;
    assert_eq!(
        got.nodes[0].status,
        NodeStatus::Ok,
        "nothing to apply is not a fault"
    );
}

fn local_dir(name: &str) -> String {
    format!("file://{}", dir(name).join("cold").display())
}

/// A frontend with nothing registered.
async fn rig_with_nothing() -> Rig {
    rig(0, |s3| registered(s3, "bkt", ID, SECRET)).await
}

/// Catches a frontend that went back to a lower generation being ignored by a node
/// (mutant: `for_beat` marks only a higher announcement pending): a node that runs on 5
/// is told the frontend is at 2, says `pending` for it and still names what it runs on; a
/// node started after the drop fetches 2 whole, and the farm shows it applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lowered_generation_is_pending_for_a_running_node_and_applied_by_a_new_one() {
    let rig = rig(5, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let half = started(&flags(&rig, "lower-a", &bucket(&rig), no_keys))
        .await
        .expect("starts");
    let _serving = tokio::spawn(half.serve());
    assert_eq!(reported(&rig, 5).await.nodes[0].status, NodeStatus::Ok);
    assert_eq!(reported_status(&rig, 1), Some(1), "the node says ok at 5");

    rig.source.set(2);
    let behind = until("the node's report of the lowered generation", || {
        let map = promoted(&rig)?;
        let got = rig.farm.acks.convergence(&map, 2).ok()?;
        (got.nodes.first()?.status == NodeStatus::Pending).then_some(got)
    })
    .await;
    assert_eq!(behind.nodes[0].applied_generation, Some(5));
    assert!(!behind.converged);
    // The frontend would judge an `ok` at 5 against 2 as pending by itself; the node's own
    // word is what is pinned: it says pending.
    until("the node's own pending", || {
        (reported_status(&rig, 1) == Some(2)).then_some(())
    })
    .await;

    let second = started(&flags(&rig, "lower-b", &bucket(&rig), no_keys))
        .await
        .expect("a node started at the lowered generation");
    let _serving = tokio::spawn(second.serve());
    let two = until("the second node to be applied", || {
        let map = promoted(&rig)?;
        let got = rig.farm.acks.convergence(&map, 2).ok()?;
        let applied = got
            .nodes
            .get(1)
            .is_some_and(|node| node.status == NodeStatus::Ok);
        applied.then_some(got)
    })
    .await;
    assert_eq!(two.nodes[1].applied_generation, Some(2));
}

/// Catches a node that reports before it has opened its store (mutant: a heartbeat sent
/// from `register`), or whose crash half way claims a generation: while the fetch is held
/// the frontend has the registration of the node and no ack of it; the start is dropped
/// (a crash), the frontend still has no ack; a node that starts afterwards fetches again
/// and is applied. Nothing the first attempt did needs undoing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_cut_off_between_the_fetch_and_the_report_claims_nothing() {
    let rig = rig(7, |s3| registered(s3, "bkt", ID, SECRET)).await;
    rig.source.hold();
    let first = flags(&rig, "cut-a", &bucket(&rig), no_keys);
    let mut starting = Box::pin(started(&first));
    let held = async {
        until("the node to register", || {
            rig.farm.members.current_map().ok().flatten()
        })
        .await;
        tokio::time::sleep(BEAT * 5).await;
    };
    tokio::select! {
        done = &mut starting => panic!("the start finished while the fetch was held: {:?}", done.map(drop)),
        () = held => {}
    }
    assert!(
        rig.farm
            .ack_store
            .fake
            .scan_prefix("objstore/ack/")
            .expect("scan")
            .is_empty(),
        "no report before the store is open"
    );
    // The crash: the start is dropped half way.
    drop(starting);
    rig.source.release();
    tokio::time::sleep(BEAT * 5).await;
    assert!(
        rig.farm
            .ack_store
            .fake
            .scan_prefix("objstore/ack/")
            .expect("scan")
            .is_empty(),
        "a start that was cut off reported nothing"
    );

    let again = started(&flags(&rig, "cut-b", &bucket(&rig), no_keys))
        .await
        .expect("a later start fetches again");
    let _serving = tokio::spawn(again.serve());
    let got = until("the second start's report", || {
        let map = promoted(&rig)?;
        let got = rig.farm.acks.convergence(&map, 7).ok()?;
        got.nodes
            .iter()
            .any(|n| n.status == NodeStatus::Ok && n.applied_generation == Some(7))
            .then_some(got)
    })
    .await;
    assert!(got.nodes.iter().any(|n| n.status == NodeStatus::Ok));
}

/// Catches keys written where they can be found (mutant: the registered secret written to
/// a cache file in the node's directory), and keys in the process's arguments or
/// environment. The test first reads the secret from the answer and from the store's
/// requests (signed with it, so the service accepted it), so a scan that finds nothing is
/// not a scan of nothing; then it reads every file under the test's directory, the
/// process's command line and its environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registered_secret_is_in_no_file_argument_or_variable() {
    let secret = "SENTINEL-registered-secret-71ab";
    let s3 = FakeS3::start().await;
    s3.require_credentials(ID, secret);
    let source = Source::new(8);
    source.serve(registered(&s3, "bkt", ID, secret));
    let farm = farm_beating(source.clone(), false, None, BEAT * 3).await;
    let rig = Rig { farm, s3, source };
    let node = flags(&rig, "scan", &bucket(&rig), no_keys);
    let half = started(&node).await.expect("starts on the registration");
    round_trip(&half, "bytes that need the registered keys").await;
    assert!(
        rig.s3.requests().iter().any(|r| r.starts_with("PUT")),
        "the service took signed writes, so the node holds the secret"
    );
    let _serving = tokio::spawn(half.serve());
    reported(&rig, 8).await;

    // Every node directory of this binary, and whatever else was left beside them.
    let root = dir_of("scan").parent().expect("a parent").to_owned();
    let mut found = Vec::new();
    scan(&root, secret.as_bytes(), &mut found);
    assert!(found.is_empty(), "the secret is on disk in {found:?}");
    let args: Vec<String> = std::env::args().collect();
    assert!(args.iter().all(|a| !a.contains(secret)));
    assert!(std::env::vars().all(|(k, v)| !k.contains(secret) && !v.contains(secret)));
    assert!(!format!("{node:?}").contains(secret));
}

fn scan(dir: &Path, needle: &[u8], found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, needle, found);
        } else if std::fs::read(&path)
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
        {
            found.push(path);
        }
    }
}
