//! The run routes through the client, against the fake's admission rules and the failures a
//! host and a restart cause.

mod support;

use std::time::Duration;

use kbf_lease_client::fake::FakeLeaseServer;
use kbf_lease_client::{ErrorCode, Isolation, Outcome, RunId, RunState};
use support::{GIB, IMAGE, fake, lease, raw_json, refused, run_of};

/// Asserts a run's life as the client sees it: queued, then what the host does (started,
/// result reported, listing dropped), with the outcome and exit code only once the host has
/// reported one, and the list in order of queueing. It catches a client that loses the exit
/// code or reads `exited` as `ended`, and a list that is not oldest first.
#[tokio::test]
async fn a_run_goes_from_queued_through_exited_to_ended() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let first = client.queue_run(id, &run_of(1000, 1)).await.unwrap();
    assert_eq!(first.state, RunState::Queued);
    assert_eq!(first.run_id.to_string(), "run-4-1");
    let second = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;

    let shown = client.run(id, &first.run_id).await.unwrap();
    assert_eq!(
        (shown.state, shown.outcome, shown.exit_code),
        (RunState::Queued, None, None)
    );
    assert!(server.run_started(&first.run_id));
    assert!(
        !server.run_started(&first.run_id),
        "only a queued run starts"
    );
    assert_eq!(
        client.run(id, &first.run_id).await.unwrap().state,
        RunState::Running
    );
    assert!(server.finish_run(&first.run_id, Outcome::Failed, Some(3), Some("exit 3")));
    let shown = client.run(id, &first.run_id).await.unwrap();
    assert_eq!(shown.state, RunState::Exited);
    assert_eq!(
        (shown.outcome, shown.exit_code),
        (Some(Outcome::Failed), Some(3))
    );
    assert_eq!(shown.detail.as_deref(), Some("exit 3"));
    assert!(server.end_run(&first.run_id));
    assert!(!server.end_run(&first.run_id), "only an exited run ends");
    assert_eq!(server.run_state(&first.run_id), Some(RunState::Ended));

    let runs = client.runs(id).await.unwrap();
    let listed: Vec<&RunId> = runs.iter().map(|run| &run.run_id).collect();
    assert_eq!(listed, [&first.run_id, &second]);
    assert_eq!(runs[0].state, RunState::Ended);
    assert_eq!(runs[1].state, RunState::Queued);
    let unknown: RunId = "run-4-99".parse().unwrap();
    assert!(!server.finish_run(&unknown, Outcome::Killed, None, None));
    assert!(!server.run_started(&unknown));
    assert!(server.run_wall_ms(&unknown).is_none());
}

/// Asserts the budget rule that makes `409 no-capacity` a retry and not a failure: a run's
/// limits stay booked after it exits and come back only when it has ended. With the host
/// full, finishing a run frees nothing; ending it frees exactly its limits. Memory counts
/// as well as cpu. It catches a fake or client that frees at exit (the executor would never
/// see the retry case) and one that sums only cpu.
#[tokio::test]
async fn limits_come_back_when_a_run_ends_not_when_it_exits() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let neighbour = lease(&client).await;
    client
        .queue_run(neighbour, &run_of(16_000, 64))
        .await
        .unwrap();
    let mut runs = Vec::new();
    for _ in 0..4 {
        runs.push(
            client
                .queue_run(id, &run_of(4000, 16))
                .await
                .unwrap()
                .run_id,
        );
    }
    let full = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(full), (409, ErrorCode::NoCapacity));

    assert!(server.run_started(&runs[0]));
    assert!(server.finish_run(&runs[0], Outcome::Succeeded, Some(0), None));
    let still = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(
        refused(still),
        (409, ErrorCode::NoCapacity),
        "exited still holds its limits"
    );
    assert!(server.end_run(&runs[0]));
    let memory_bound = client.queue_run(id, &run_of(1000, 17)).await.unwrap_err();
    assert_eq!(
        refused(memory_bound),
        (409, ErrorCode::NoCapacity),
        "17 GiB > 16 GiB back"
    );
    let cpu_bound = client.queue_run(id, &run_of(4001, 1)).await.unwrap_err();
    assert_eq!(
        refused(cpu_bound),
        (409, ErrorCode::NoCapacity),
        "4001 > 4000 back"
    );
    client.queue_run(id, &run_of(4000, 16)).await.unwrap();
}

/// Asserts the 256-run cap of one host. It catches a fake with no cap, which would let the
/// executor's queue grow past what the real server takes.
#[tokio::test]
async fn a_host_takes_at_most_256_live_runs() {
    let (_server, client) = fake().await;
    let id = lease(&client).await;
    for _ in 0..256 {
        client
            .queue_run(id, &run_of(1, 0).with_memory(1))
            .await
            .unwrap();
    }
    let error = client
        .queue_run(id, &run_of(1, 0).with_memory(1))
        .await
        .unwrap_err();
    assert_eq!(refused(error), (409, ErrorCode::NoCapacity));
}

trait WithMemory {
    fn with_memory(self, bytes: u64) -> Self;
}

impl WithMemory for kbf_lease_client::RunSpec {
    fn with_memory(mut self, bytes: u64) -> Self {
        self.limits.memory_bytes = bytes;
        self
    }
}

/// Asserts each rule of the run contract with its status and code: a job above the shape on
/// its own (cpu, memory, a GPU index the host lacks) is `422 limits-exceed-shape`, an image
/// not pinned by digest is `422 image-digest-required`, a zero limit is `422 invalid-job`,
/// and a run that may change the host is `422 host-jobs-not-allowed`. It catches a client
/// that cannot tell a rule from a full host: a 422 is never worth retrying.
#[tokio::test]
async fn a_job_that_breaks_the_contract_is_refused_with_its_code() {
    let (_server, client) = fake().await;
    let id = lease(&client).await;
    let mut tag_only = run_of(1000, 1);
    tag_only.image = "reg.example/kbf-task:v1".to_owned();
    let mut short_digest = run_of(1000, 1);
    short_digest.image = "reg.example/kbf-task@sha256:abc".to_owned();
    let mut non_hex = run_of(1000, 1);
    non_hex.image = format!("reg.example/kbf-task@sha256:{}", "g".repeat(64));
    let mut nameless = run_of(1000, 1);
    nameless.image = format!("@sha256:{}", "0".repeat(64));
    let mut gpu = run_of(1000, 1);
    gpu.gpu_indices = vec![0];
    let mut host = run_of(1000, 1);
    host.isolation = Isolation::Host;
    let mut zero_wall = run_of(1000, 1);
    zero_wall.limits.wall_ms = Some(0);
    for (spec, status, code) in [
        (tag_only, 422, ErrorCode::ImageDigestRequired),
        (short_digest, 422, ErrorCode::ImageDigestRequired),
        (non_hex, 422, ErrorCode::ImageDigestRequired),
        (nameless, 422, ErrorCode::ImageDigestRequired),
        (run_of(16_001, 1), 422, ErrorCode::LimitsExceedShape),
        (run_of(1000, 65), 422, ErrorCode::LimitsExceedShape),
        (gpu, 422, ErrorCode::LimitsExceedShape),
        (run_of(0, 1), 422, ErrorCode::InvalidJob),
        (run_of(1000, 0), 422, ErrorCode::InvalidJob),
        (zero_wall, 422, ErrorCode::InvalidJob),
        (host, 422, ErrorCode::HostJobsNotAllowed),
    ] {
        let label = format!("{spec:?}");
        let error = client.queue_run(id, &spec).await.unwrap_err();
        assert_eq!(refused(error), (status, code), "{label}");
    }
}

/// Asserts the disk a run books is passed to the server as asked and checked against the
/// host's shape: a run whose disk is within the shape is accepted and the server holds the
/// number it was sent, one over it is `422 limits-exceed-shape`, and a pool that reports no
/// disk refuses none. It catches a client that drops the disk, and a fake whose shape check
/// ignores it (the executor's refusal arm would then never be reached).
#[tokio::test]
async fn a_runs_disk_is_checked_against_the_hosts_shape() {
    let mut pool = support::pool();
    pool.shape.disk_bytes = Some(100 * GIB);
    let server = FakeLeaseServer::start(pool).await;
    let client = server.client().unwrap();
    let id = lease(&client).await;
    let mut within = run_of(1000, 1);
    within.limits.disk_bytes = Some(100 * GIB);
    let run = client.queue_run(id, &within).await.unwrap().run_id;
    assert_eq!(server.run_disk_bytes(&run), Some(100 * GIB));
    let mut over = run_of(1000, 1);
    over.limits.disk_bytes = Some(100 * GIB + 1);
    let error = client.queue_run(id, &over).await.unwrap_err();
    assert_eq!(refused(error), (422, ErrorCode::LimitsExceedShape));
    let (unshaped, client) = fake().await;
    let id = lease(&client).await;
    let run = client.queue_run(id, &over).await.unwrap().run_id;
    assert_eq!(unshaped.run_disk_bytes(&run), Some(100 * GIB + 1));
    let none = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert_eq!(unshaped.run_disk_bytes(&none), None);
}

/// Asserts that a run's wall limit is the one asked for, cut to what the lease has left, and
/// that it does not grow when the lease is renewed later. The executor sets each run's wall
/// to its action's timeout and relies on the server never giving it more than the lease
/// has left. It catches a fake that passes the asked wall through, and a cut that follows
/// renewals.
#[tokio::test]
async fn a_run_is_cut_to_the_time_the_lease_has_left() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let mut asked = run_of(1000, 1);
    asked.limits.wall_ms = Some(7_200_000);
    let cut = client.queue_run(id, &asked).await.unwrap().run_id;
    assert_eq!(server.run_wall_ms(&cut), Some(600_000));
    asked.limits.wall_ms = Some(60_000);
    let short = client.queue_run(id, &asked).await.unwrap().run_id;
    assert_eq!(server.run_wall_ms(&short), Some(60_000));
    let unasked = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert_eq!(server.run_wall_ms(&unasked), Some(600_000));

    client
        .renew(id, Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    assert_eq!(
        server.run_wall_ms(&cut),
        Some(600_000),
        "a renewal does not stretch a queued run"
    );
    let after = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert_eq!(server.run_wall_ms(&after), Some(3_600_000));
}

/// Asserts stop: a queued run is given up (`never-started`, its limits back), a running run is
/// `stopping` until the host reports and drops it, and a run that has ended stays as it is,
/// all with 202. A run of another lease, or one that never was, is `404 unknown-run`. It
/// catches a stop that frees a running run's budget at once (the host still runs it) and a
/// lookup that ignores the lease in the path.
#[tokio::test]
async fn stop_gives_up_a_queued_run_and_asks_a_running_one() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let other = lease(&client).await;
    let big = client
        .queue_run(id, &run_of(16_000, 64))
        .await
        .unwrap()
        .run_id;
    let stopped = client.stop_run(id, &big).await.unwrap();
    assert_eq!(stopped.state, RunState::NeverStarted);
    let again = client
        .queue_run(id, &run_of(16_000, 64))
        .await
        .unwrap()
        .run_id;
    assert!(server.run_started(&again));
    let asked = client.stop_run(id, &again).await.unwrap();
    assert_eq!(
        (asked.run_id.clone(), asked.state),
        (again.clone(), RunState::Stopping)
    );
    let full = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(
        refused(full),
        (409, ErrorCode::NoCapacity),
        "a stopping run still holds its limits"
    );
    assert!(server.finish_run(&again, Outcome::Killed, None, None));
    assert!(server.end_run(&again));
    assert_eq!(
        client.stop_run(id, &again).await.unwrap().state,
        RunState::Ended
    );
    assert_eq!(
        client.stop_run(id, &big).await.unwrap().state,
        RunState::NeverStarted
    );

    let foreign = client.run(other, &again).await.unwrap_err();
    assert_eq!(refused(foreign), (404, ErrorCode::UnknownRun));
    let never: RunId = "run-4-99".parse().unwrap();
    let missing = client.stop_run(id, &never).await.unwrap_err();
    assert_eq!(refused(missing), (404, ErrorCode::UnknownRun));
}

/// Asserts what a server restart does to runs, as the client sees it: every run of the old
/// boot is `410 run-forgotten` (get and stop), the lease itself survives and renews, a new
/// run is refused `409 host-not-registered` until the host says hello, and a run queued after
/// has a new id from the new boot. It catches a client that reads 410 as 404 (a run that
/// never existed is not a run that must be resubmitted) and a restart that keeps runs alive.
#[tokio::test]
async fn a_restart_forgets_every_run_but_not_the_lease() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let old = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert!(server.run_started(&old));

    server.restart();
    let get = client.run(id, &old).await.unwrap_err();
    assert_eq!(refused(get), (410, ErrorCode::RunForgotten));
    let stop = client.stop_run(id, &old).await.unwrap_err();
    assert_eq!(refused(stop), (410, ErrorCode::RunForgotten));
    assert!(client.runs(id).await.unwrap().is_empty());
    client.renew(id, None).await.unwrap();
    let silent = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(silent), (409, ErrorCode::HostNotRegistered));

    assert!(server.host_hello("node1"));
    let fresh = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert_eq!(fresh.to_string(), "run-5-1");
    let still = client.run(id, &old).await.unwrap_err();
    assert_eq!(refused(still), (410, ErrorCode::RunForgotten));
    assert_eq!(
        client.run(id, &fresh).await.unwrap().state,
        RunState::Queued
    );
}

/// Asserts that a lease released while a restart happens is gone with its runs (nothing
/// is left releasing that no host will ever finish). It catches a restart that leaves a
/// releasing lease booking its host forever.
#[tokio::test]
async fn a_restart_ends_a_release_that_was_waiting_for_forgotten_runs() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert!(server.run_started(&run));
    client.release(id).await.unwrap();
    server.restart();
    assert!(client.leases().await.unwrap().is_empty());
}

/// Asserts the refusals that come from the host: a lost host takes no run (`host-lost`) and
/// its runs keep their last state, a tainted host takes none (`host-tainted`), and a host
/// that is back takes runs again. It catches a client that cannot tell a lost host from a
/// full one, which need opposite handling (re-lease against retry).
#[tokio::test]
async fn a_lost_or_tainted_host_takes_no_run_and_keeps_the_ones_it_has() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert!(server.run_started(&run));

    assert!(server.set_lost("node1", true));
    let lost = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(lost), (409, ErrorCode::HostLost));
    assert_eq!(client.run(id, &run).await.unwrap().state, RunState::Running);
    assert!(server.set_lost("node1", false));
    assert!(server.taint_host("node1", "scratch-not-clean"));
    let tainted = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(tainted), (409, ErrorCode::HostTainted));
    assert_eq!(
        client.run(id, &run).await.unwrap().state,
        RunState::Running,
        "live runs finish"
    );
}

/// Asserts that runs are refused on a lease that is not there: unknown (404), and a lease
/// whose ttl has run out before the queue (the server has dropped it, so also
/// `404 unknown-lease`). It catches a fake that takes work for a lease with no time left,
/// which the real server refuses.
#[tokio::test]
async fn a_run_needs_a_lease_that_is_held_with_time_left() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let unknown = kbf_lease_client::LeaseId::new(4, 99);
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    for error in [
        client
            .queue_run(unknown, &run_of(1000, 1))
            .await
            .unwrap_err(),
        client.runs(unknown).await.unwrap_err(),
        client.run(unknown, &run).await.unwrap_err(),
        client.stop_run(unknown, &run).await.unwrap_err(),
    ] {
        assert_eq!(refused(error), (404, ErrorCode::UnknownLease));
    }
    server.advance(Duration::from_secs(1000));
    let gone = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(
        refused(gone),
        (404, ErrorCode::UnknownLease),
        "an expired lease is dropped"
    );
}

/// Asserts that a run spec never prints its arguments (they may hold a presigned URL) in its
/// `Debug`, which is what a log line of a failed call would print, and that they do reach
/// the server unchanged. It catches a derived `Debug`, and a client that drops or rewrites
/// the arguments.
#[tokio::test]
async fn arguments_reach_the_server_and_never_a_log_line() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let mut spec = run_of(1000, 1);
    spec.args = vec!["https://store.example/put?X-Amz-Signature=SECRETSIG".to_owned()];
    spec.mounts = vec![kbf_lease_client::Mount {
        root_name: "tools".to_owned(),
        rel: "tool".to_owned(),
        ro: true,
    }];
    let printed = format!("{spec:?}");
    assert!(!printed.contains("SECRETSIG"), "{printed}");
    assert!(printed.contains("<1 redacted>"), "{printed}");
    assert!(printed.contains(IMAGE));
    let queued = client.queue_run(id, &spec).await.unwrap().run_id;
    assert_eq!(server.run_args(&queued), Some(spec.args.clone()));
    assert_eq!(server.run_mounts(&queued), Some(spec.mounts.clone()));
}

/// Asserts that a run's labels come back on the run and on the list, and that a run with
/// none shows none. A caller that lost the 202 of a queue call finds its run again by the
/// label it put on it. It catches a client that drops the labels it sends or the ones it
/// reads.
#[tokio::test]
async fn a_run_shows_the_labels_it_was_queued_with() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let mut spec = run_of(1000, 1);
    spec.labels
        .insert("kbf-descriptor".to_owned(), "d-1".to_owned());
    let labelled = client.queue_run(id, &spec).await.unwrap().run_id;
    let bare = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert_eq!(
        client.run(id, &labelled).await.unwrap().labels["kbf-descriptor"],
        "d-1"
    );
    assert!(client.run(id, &bare).await.unwrap().labels.is_empty());
    let listed = client.runs(id).await.unwrap();
    assert_eq!(listed[0].labels["kbf-descriptor"], "d-1");
    assert!(listed[1].labels.is_empty());
    let (_, body) = raw_json(&server, "GET", &format!("/v1/leases/{id}/runs/{bare}"), "").await;
    assert!(body.get("labels").is_none(), "no labels, no field: {body}");
}

/// Asserts the order the server checks a queue call in: the lease, then the host's record,
/// then the job, then the host's health. A lost or tainted host with an invalid job is a
/// `422` (the job is wrong whoever runs it), a silent host is `409 host-not-registered`
/// before the job is read, and an unreadable lease id is `404` before an unreadable body is
/// `400`. It catches a fake that checks the host first, which would give the executor a
/// retryable 409 for a job that can never run.
#[tokio::test]
async fn a_queue_call_is_checked_in_the_servers_order() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let mut bad = run_of(1000, 1);
    bad.image = "reg.example/kbf-task:v1".to_owned();
    assert!(server.set_lost("node1", true));
    let lost = client.queue_run(id, &bad).await.unwrap_err();
    assert_eq!(refused(lost), (422, ErrorCode::ImageDigestRequired));
    let lost = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(lost), (409, ErrorCode::HostLost));
    assert!(server.set_lost("node1", false));
    assert!(server.taint_host("node1", "scratch-not-clean"));
    let tainted = client.queue_run(id, &run_of(0, 1)).await.unwrap_err();
    assert_eq!(refused(tainted), (422, ErrorCode::InvalidJob));
    server.restart();
    let silent = client.queue_run(id, &bad).await.unwrap_err();
    assert_eq!(refused(silent), (409, ErrorCode::HostNotRegistered));

    let (status, _) = raw_json(&server, "POST", "/v1/leases/not-a-lease/runs", "{").await;
    assert_eq!(status, 404, "the lease id is read before the body");
    let (status, _) = raw_json(&server, "POST", &format!("/v1/leases/{id}/runs"), "{").await;
    assert_eq!(status, 400);
}

/// Asserts the fake's refusal of runs short of a taint: a host told to refuse with a code
/// answers every new run with it, keeps the runs it has, and takes runs again when told to.
/// It catches a client that cannot read `quota-unavailable` as a code of its own.
#[tokio::test]
async fn a_host_refusing_with_a_code_takes_no_run_until_told_to() {
    let (server, client) = fake().await;
    let id = lease(&client).await;
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    assert!(server.run_started(&run));
    assert!(server.refuse_posts("node1", Some("quota-unavailable")));
    let refusal = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
    assert_eq!(refused(refusal), (409, ErrorCode::QuotaUnavailable));
    assert_eq!(client.run(id, &run).await.unwrap().state, RunState::Running);
    assert!(server.refuse_posts("node1", None));
    client.queue_run(id, &run_of(1000, 1)).await.unwrap();
    assert!(!server.refuse_posts("nowhere", None));
}
