//! The design's proof of the swap (`docs/design/frontend-proxy.md`, sections 8 and 12): two
//! children run side by side on one state store and hand over without losing a build.
//!
//! A holds the generation, serves fifty Executes (a few leased to a daemon and running, the
//! rest queued) and keeps their streams open. B is a candidate on the same store through a
//! handle of its own (another connection, as another process has). A quiesces, B takes
//! the next generation, A stands down; the daemon redials B, which finishes every action:
//! the ones A leased on the daemon's report, the ones A queued on new leases. Every stream
//! open on A ends with the result B committed, through A's follower of the `done/` rows.
//!
//! The body runs over the in-memory store in every test run and over a real Postgres
//! schema in the `state-postgres` CI job (`two_children` is its name filter there); the
//! store is the only thing that differs between the two.

use std::collections::HashMap;

use kbf_history::IndexRoot;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{Start, server_message};
use kbf_server::write_rules::{Rule, classify};
use kbf_state::postgres::{Flaw as PgFlaw, PostgresBackend, PostgresStore};
use kbf_state::recording::RecordingStore;
use support::{PROMPT, QUIET};

use super::writes::child_until;
use super::*;

/// How many builds the swap carries.
const BUILDS: usize = 50;

/// The generation the store holds now.
fn held_generation(store: &dyn Store) -> u64 {
    let row = store.get(GENERATION).expect("read").expect("a generation");
    u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
}

/// The next Start the daemon is given within `within`, if one comes.
async fn next_start(daemon: &mut support::FakeDaemon, within: Duration) -> Option<Start> {
    daemon
        .expect_within(within, |m| match m {
            server_message::Message::Start(s) => Some(s.clone()),
            _ => None,
        })
        .await
}

/// The result a client's stream ends with. It reads until the done operation with one
/// deadline for the whole wait: a build queued behind forty others says nothing for a
/// while.
async fn finished(
    mut ops: tonic::Streaming<kbf_proto::google::longrunning::Operation>,
) -> ActionResult {
    let wait = async {
        loop {
            let op = ops
                .message()
                .await
                .expect("a healthy stream")
                .expect("the stream ends only after the done operation");
            if op.done {
                return response(&op).result.expect("a result");
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), wait)
        .await
        .expect("a stream on A is answered")
}

/// Reports build `build` done on `lease` through `daemon`, with an output of its own.
async fn report(
    daemon: &mut support::FakeDaemon,
    client: &Client,
    lease: kbf_proto::worker::LeaseId,
    build: usize,
) -> ActionResult {
    let result = support::output(client, &format!("built {build}"), 0).await;
    assert!(
        daemon.report(ran(Some(lease), &result)).await.accepted,
        "build {build}: B did not take the lease it was reported for"
    );
    result
}

/// Fifty builds across a takeover, over the two handles `a_inner` and `b_inner` of one
/// store.
///
/// Catches: a takeover that loses a build (a lease taken from the daemon, a queued
/// operation forgotten, a stream on A never answered); one that runs a build twice (a
/// lease granted again for work the daemon holds, a queued operation granted by both
/// children); a result committed through B that does not reach a waiter on A (the
/// follower of `done/` rows); a takeover that does not take the generation, or takes it
/// and bumps the epoch; an old child that writes after the takeover (any committed write
/// of a fenced family, the cordon an operator asks of A included). Mutants: the takeover
/// leaves the generation where it is; the fence not checked in the write's transaction;
/// A's follower not started.
async fn fifty_builds_across_a_takeover(a_inner: Arc<dyn Store>, b_inner: Arc<dyn Store>) {
    let node = Node::start().await;
    let scratch = Scratch::new("two-children");
    let token = scratch.token_flag();
    let a_store = Arc::new(RecordingStore::new(a_inner));
    let b_store = Arc::new(RecordingStore::new(b_inner));
    let (stop_a, stopped_a) = oneshot::channel();
    let (a, a_serving) =
        child_until(&managed_flags(&node, &[&token]), a_store.clone(), stopped_a).await;
    let (stop_b, stopped_b) = oneshot::channel();
    let (b, b_serving) = child_until(
        &managed_flags(&node, &["--await-takeover"]),
        b_store.clone(),
        stopped_b,
    )
    .await;
    assert_eq!(b.role().await["role"], "candidate");

    // Fifty builds on A, each with a stream of its own; the daemon runs what it is given
    // and holds it.
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 8, 16).await;
    let jobs: Vec<Job> = (0..BUILDS)
        .map(|i| Job::new(&format!("build {i} across a takeover"), &[]))
        .collect();
    let mut waiters = Vec::new();
    for job in &jobs {
        client.upload(&job.blobs()).await;
        waiters.push(tokio::spawn(finished(client.execute(&job.action).await)));
    }
    until("every operation is queued", async || {
        a_store.scan_prefix("op/").expect("scan").len() == BUILDS
    })
    .await;
    let by_action: HashMap<String, usize> = jobs
        .iter()
        .enumerate()
        .map(|(i, job)| (job.action.proto.hash.clone(), i))
        .collect();
    let mut starts: HashMap<usize, usize> = HashMap::new();
    let mut running = Vec::new();
    while let Some(start) = next_start(&mut daemon, QUIET).await {
        let build = by_action[&start.action_digest.as_ref().expect("a digest").hash];
        *starts.entry(build).or_default() += 1;
        running.push((start.lease_id.expect("a lease id"), build));
    }
    assert!(
        (1..BUILDS).contains(&running.len()),
        "some builds run and some wait: {} run",
        running.len()
    );

    // The swap.
    let held = held_generation(a_store.as_ref());
    let epoch = a_store.get("meta/epoch").expect("read").expect("an epoch");
    assert_eq!(a.post("quiesce").await.0, 200);
    meanwhile().await;
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    a_store.clear();
    assert_eq!(took["role"], "active");
    assert_eq!(took["held_generation"], held + 1, "{took}");
    assert_eq!(held_generation(b_store.as_ref()), held + 1);
    assert_eq!(
        b_store
            .get("meta/epoch")
            .expect("read")
            .expect("an epoch")
            .value,
        epoch.value,
        "a takeover bumped the epoch"
    );

    // An operator asks the old child for a write: a fenced family, refused.
    assert_eq!(get_post(a.api, "/v1/nodes/node-a:cordon").await, 200);
    until("A sees it is fenced", async || {
        a.role().await["superseded"] == true
    })
    .await;
    assert_eq!(a.role().await["held_generation"], held);
    assert_eq!(a.role().await["generation"], held + 1);
    assert_eq!(a.post("stand-down").await.0, 200);
    assert_eq!(daemon.ended().await.code(), Code::Unavailable);

    // The daemon redials B, which kept the leases it held.
    let b_view = b.role().await;
    assert_eq!(b_view["role"], "active");
    let reached = Client::connect(b.reapi, Child::worker_of(&b_view).expect("a worker")).await;
    let mut daemon = reached.daemon("node-a", 8, 16).await;
    let leases: Vec<_> = running.iter().map(|(lease, _)| *lease).collect();
    assert_eq!(daemon.epoch, leases[0].term, "the epoch was kept");
    assert!(daemon.heartbeat(&leases).await, "B did not keep the leases");

    let mut reported: HashMap<usize, ActionResult> = HashMap::new();
    for (lease, build) in running {
        let result = report(&mut daemon, &reached, lease, build).await;
        assert!(
            reported.insert(build, result).is_none(),
            "build {build} ran twice"
        );
    }
    while reported.len() < BUILDS {
        let start = next_start(&mut daemon, PROMPT)
            .await
            .unwrap_or_else(|| panic!("B granted nothing with {} builds done", reported.len()));
        let build = by_action[&start.action_digest.as_ref().expect("a digest").hash];
        *starts.entry(build).or_default() += 1;
        let lease = start.lease_id.expect("a lease id");
        let result = report(&mut daemon, &reached, lease, build).await;
        assert!(
            reported.insert(build, result).is_none(),
            "build {build} ran twice"
        );
    }
    assert_eq!(starts.len(), BUILDS, "every build was started");
    for (build, count) in &starts {
        assert_eq!(*count, 1, "build {build} was started {count} times");
    }

    // Every stream on A ended with the result of its own build, committed through B.
    for (build, waiter) in waiters.into_iter().enumerate() {
        let result = waiter.await.expect("the task");
        assert_eq!(Some(&result), reported.get(&build), "build {build}");
    }

    // A wrote nothing of a fenced family after the takeover; the cordon it tried was
    // refused (the node is not cordoned), and the write rules name what it did write.
    let root = IndexRoot::default();
    let (mut refused, mut committed) = (0, Vec::new());
    for write in a_store.writes() {
        let rule = classify(&write.key, &root)
            .unwrap_or_else(|| panic!("{} is of no family of the table", write.key))
            .rule;
        match (rule, write.committed) {
            (Rule::Fenced, true) => panic!("A committed {} after the takeover", write.key),
            (Rule::Fenced, false) => refused += 1,
            (_, true) => committed.push(write.key),
            (_, false) => {}
        }
    }
    assert!(refused >= 1, "A's cordon was not tried or not refused");
    assert!(
        committed.is_empty(),
        "A committed after the takeover: {committed:?}"
    );
    assert_eq!(held_generation(b_store.as_ref()), held + 1);
    assert_eq!(b.role().await["role"], "active");

    for (stop, serving) in [(stop_a, a_serving), (stop_b, b_serving)] {
        stop.send(()).expect("it serves");
        tokio::time::timeout(Duration::from_secs(30), serving)
            .await
            .expect("it stops")
            .expect("its task");
    }
}

/// The swap over the in-memory store, both children on one `Fake`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_builds_across_a_takeover_over_the_fake() {
    let base = shared();
    fifty_builds_across_a_takeover(base.clone(), base).await;
}

/// The swap over a real Postgres schema, each child on a connection of its own. The schema
/// is the installer's, made for this test alone (the other tests of the binary share none
/// of it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
async fn fifty_builds_across_a_takeover_over_postgres() {
    let url = std::env::var("KBF_TEST_POSTGRES_URL")
        .expect("KBF_TEST_POSTGRES_URL names a database the test may create schemas in");
    // Declared first, so the schema is dropped after both stores are gone.
    let backend = PostgresBackend::new(&url, PgFlaw::None);
    let config = backend.fresh_config();
    let a = Arc::new(PostgresStore::open(&config).expect("A's connection"));
    let b = Arc::new(PostgresStore::open(&config).expect("B's connection"));
    fifty_builds_across_a_takeover(a, b).await;
}
