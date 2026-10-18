//! `kbf frontend` taking over from a frontend that serves (`kbf_server::handover`), the
//! way it runs behind Envoy: two frontends in this process, instances a and b, each on
//! listeners of its own and on one state file, the first with a build in flight and a
//! daemon connected, a fake daemon playing the farm's node. Envoy's own part (choosing a
//! host by its health) is played by the tests reading `/readyz` of each instance.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use kbf_server::frontend::start_or_take_over;
use kbf_server::{Drain, ServeError};
use tokio::task::JoinHandle;

use support::done_within_quiet;
use tonic::Code;

use super::*;

/// A directory of the test's own: the state file, the control socket.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("kbf-frontend-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The flags of a frontend that hands over as instance `instance` (`a` or `b`), on new
/// loopback ports, with Envoy's check set to a millisecond (so the settle is three).
fn handover_flags(node: &Node, dir: &Dir, instance: &str, more: &[&str]) -> FrontendFlags {
    let mut argv = vec![
        format!("--storage=http://{}", node.addr),
        format!("--handover-socket={}/control.sock", dir.0.display()),
        format!("--instance={instance}"),
        "--handover-follow-interval-ms=10".to_owned(),
        "--heartbeat-interval-ms=100".to_owned(),
    ];
    if !more
        .iter()
        .any(|m| m.starts_with("--envoy-check-interval-ms="))
    {
        argv.extend(
            [
                "--envoy-check-interval-ms=1",
                "--envoy-check-timeout-ms=1",
                "--envoy-unhealthy-threshold=1",
            ]
            .map(str::to_owned),
        );
    }
    if !more.iter().any(|m| m.starts_with("--state-store=")) {
        argv.push(format!(
            "--state-store=sqlite://{}/state.db",
            dir.0.display()
        ));
    }
    argv.extend(more.iter().map(|m| (*m).to_owned()));
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    flags(&argv).expect("flags")
}

/// A frontend serving in this process.
struct Proc {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
    drain: Arc<Drain>,
    /// Dropped to stop the frontend.
    _stop: oneshot::Sender<()>,
    served: JoinHandle<Result<(), ServeError>>,
}

impl Proc {
    fn serving(handle: FrontendHandle, stop: oneshot::Sender<()>) -> Self {
        let (reapi, worker, api) = (handle.reapi, handle.worker, handle.api);
        let drain = handle.drain();
        Self {
            reapi,
            worker,
            api: api.expect("the operator API"),
            drain,
            _stop: stop,
            served: tokio::spawn(handle.serve()),
        }
    }

    /// A start with no predecessor.
    async fn cold(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = start_frontend(flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        Self::serving(handle, stop)
    }

    /// A start that takes over from the frontend serving, or starts alone if none is.
    async fn taking_over(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = taking_over(flags, stopped).await.expect("takes over");
        Self::serving(handle, stop)
    }

    /// Waits for this frontend to end, which it does once it has left Envoy's rotation
    /// and holds no stream, no relay and no caller.
    async fn ended(self) {
        let served = tokio::time::timeout(Duration::from_secs(20), self.served)
            .await
            .expect("the frontend ends")
            .expect("the task");
        served.expect("served without an error");
    }
}

async fn taking_over(
    flags: &FrontendFlags,
    stopped: oneshot::Receiver<()>,
) -> Result<FrontendHandle, Box<dyn std::error::Error>> {
    start_or_take_over(flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
}

/// Waits until `look` says yes, for at most twenty seconds.
async fn until(what: &str, mut look: impl AsyncFnMut() -> bool) {
    for _ in 0..1000 {
        if look().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never happened: {what}");
}

/// The status code `GET /readyz` of the operator API at `api` answers.
async fn readyz(api: SocketAddr) -> u16 {
    get(api, "/readyz").await.0
}

/// A counter or gauge of `GET /metrics` at `api`.
async fn metric(api: SocketAddr, name: &str) -> u64 {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).await.expect("read");
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {name} in the metrics"))
        .parse()
        .expect("a number")
}

/// Gives a frontend that should not have acted yet the time it would have taken to.
async fn meanwhile() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Catches: a build failed or hung by a frontend update. The first frontend (instance a)
/// has a daemon connected and an Execute stream open with its action running. A second
/// (instance b, on ports of its own) takes over: the first ends the daemon's stream (the
/// daemon dials again and reaches the second), keeps the client's stream open, and the
/// second, which kept the epoch, takes the result of the lease the first granted. The
/// client's stream, on the first frontend, is answered with that result; a call made
/// after the handover is the second's, and is answered from the cache the result went to.
/// The first never sends GOAWAY, goes not ready once the second serves, and ends once it
/// holds nothing. Mutants: `Keep` replaced by a bump of the epoch (the lease is not
/// taken), the follower ignoring `done/` rows (the stream is never answered), the stand
/// down not ending the daemon's stream (the daemon never reaches the second frontend), a
/// holder that sends GOAWAY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_in_flight_is_finished_across_a_handover() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build across the update", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let lease = start.lease_id.expect("a lease id");

    let new = Proc::taking_over(&handover_flags(&node, &dir, "b", &[])).await;
    assert_ne!(new.reapi, old.reapi, "the successor binds its own ports");
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");

    let reached = Client::connect(new.reapi, new.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(daemon.heartbeat(&[lease]).await);
    assert!(
        !done_within_quiet(&mut ops).await,
        "the stream was answered before the result"
    );
    let result = support::output(&reached, "built across the update", 0).await;
    assert!(
        daemon.report(ran(Some(lease), &result)).await.accepted,
        "the successor did not take the lease of the frontend before it"
    );
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    assert!(!response(&last).cached_result);

    let mut again = reached.execute(&job.action).await;
    assert!(response(&done(&mut again).await).cached_result);
    until("the first frontend leaves Envoy's rotation", async || {
        old.drain.is_unready()
    })
    .await;
    assert!(!old.drain.is_draining(), "an update sent GOAWAY");
    drop((ops, again, client));
    old.ended().await;
    drop(new);
}

/// Catches: a call that Envoy still sends to the first frontend after the cut being
/// refused, dropped or answered by a scheduler that no longer runs (T-6): twelve calls
/// through the first, a cached one and a new action whose daemon is on the second, all
/// complete with no error, are counted as relayed in `kbf_handover_relayed_total`, none
/// is counted on the second, and none is left open. Envoy's check is set to an hour so
/// the first stays in rotation throughout. Mutants: the relay replaced by a refusal, by an
/// answer from the first's own scheduler, or not counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_that_still_reach_the_first_frontend_are_relayed_with_no_error() {
    let node = Node::start().await;
    let dir = Dir::new();
    let slow = ["--envoy-check-interval-ms=3600000"];
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &slow)).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let job = Job::new("run once on the second", &[]);
    let other = Job::new("run through the first", &[]);
    client.upload(&job.blobs()).await;
    client.upload(&other.blobs()).await;

    let new = Proc::taking_over(&handover_flags(&node, &dir, "b", &slow)).await;
    let reached = Client::connect(new.reapi, new.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    let mut ops = reached.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&reached, "run once", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // Twelve cached calls through the first, some at once.
    for _ in 0..6 {
        let mut ops = client.execute(&job.action).await;
        assert!(response(&done(&mut ops).await).cached_result);
    }
    let calls = (0..6).map(|_| async {
        let mut ops = client.execute(&job.action).await;
        response(&done(&mut ops).await).cached_result
    });
    assert!(
        futures::future::join_all(calls)
            .await
            .into_iter()
            .all(|c| c)
    );

    // A call that is not cached: the first relays it, the second's daemon runs it.
    let mut open = client.execute(&other.action).await;
    let start = daemon.start().await;
    let built = support::output(&reached, "run through the first", 0).await;
    assert!(daemon.report(ran(start.lease_id, &built)).await.accepted);
    let last = done(&mut open).await;
    assert_eq!(response(&last).result, Some(built));

    assert_eq!(metric(old.api, "kbf_handover_relayed_total").await, 13);
    assert_eq!(metric(old.api, "kbf_handover_relays_open").await, 0);
    assert_eq!(metric(new.api, "kbf_handover_relayed_total").await, 0);
    assert!(!old.drain.is_unready(), "left Envoy's rotation early");
}

/// Catches: a first frontend that leaves Envoy's rotation before the second can answer
/// (Envoy would have no host that can take a new stream: Buck2's open would fail), or
/// before it is told the second serves (T-D). Between the successor's start and its
/// serving the first stays ready, `/readyz` 200 though it has handed its roles over; it
/// goes not ready only once the second answers `/readyz` 200 itself. Mutants: `unready`
/// at the cut; `unready` when the successor has only started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_frontend_goes_unready_only_after_the_second_is_ready() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;
    assert_eq!(readyz(old.api).await, 200);
    // A build the first holds, so it is still there to be asked when it has left.
    let client = Client::connect(old.reapi, old.worker).await;
    let job = Job::new("held by the first", &[]);
    client.upload(&job.blobs()).await;
    let _held = client.execute(&job.action).await;

    // The second has the roles, and has not been served: it cannot answer yet.
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&handover_flags(&node, &dir, "b", &[]), stopped)
        .await
        .expect("takes over");
    meanwhile().await;
    assert!(
        !old.drain.is_unready(),
        "unready before the successor served"
    );
    assert_eq!(readyz(old.api).await, 200, "not ready at the cut");

    let new = Proc::serving(handle, stop);
    until("the second is ready", async || readyz(new.api).await == 200).await;
    until("the first leaves Envoy's rotation", async || {
        old.drain.is_unready()
    })
    .await;
    assert_eq!(
        readyz(new.api).await,
        200,
        "the first left before the second was ready"
    );
    assert_eq!(readyz(old.api).await, 503);
    drop(new);
}

/// Catches: a failed attempt to take over that changes the frontend serving. The second
/// frontend is started with a policy file that does not exist: it fails before it asks the
/// first (its flags and listeners are built first). The first goes on serving (still
/// ready, not handed over, its daemon's stream open, a new Execute answered), and a
/// second attempt, with good flags, takes over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_fails_before_ready_leaves_the_frontend_serving() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;

    let missing = dir.0.join("no-such-policy.json");
    let bad = handover_flags(
        &node,
        &dir,
        "b",
        &[&format!("--reapi-auth-policy={}", missing.display())],
    );
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&bad, stopped)
        .await
        .err()
        .expect("the start fails");
    assert!(error.to_string().contains("no-such-policy"), "{error}");
    meanwhile().await;
    assert!(!old.drain.is_unready() && !old.drain.is_draining());
    assert_eq!(readyz(old.api).await, 200);

    let job = Job::new("served by the first", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "still the first", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));

    let new = Proc::taking_over(&handover_flags(&node, &dir, "b", &[])).await;
    assert_ne!(new.reapi, old.reapi);
    drop((ops, client));
    old.ended().await;
}

/// Catches: a frontend that takes over from one started with other flags (another state
/// file, the same instance, or no instance at all): the frontend serving refuses, the
/// start fails with its reason, and the frontend serving is untouched. Also a stop signal
/// that arrives before the handover is agreed ends the start and leaves the other
/// frontend alone, and flags that ask for a handover with no state store are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_that_cannot_take_over_says_why_and_changes_nothing() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;

    let elsewhere = dir.0.join("other");
    let other_state = handover_flags(
        &node,
        &dir,
        "b",
        &[&format!(
            "--state-store=sqlite://{}/state.db",
            elsewhere.display()
        )],
    );
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&other_state, stopped)
        .await
        .err()
        .expect("refused");
    assert!(
        error.to_string().contains("refused the handover"),
        "{error}"
    );
    assert!(error.to_string().contains("--state-store"), "{error}");

    let same_instance = handover_flags(&node, &dir, "a", &[]);
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&same_instance, stopped)
        .await
        .err()
        .expect("refused");
    assert!(
        error.to_string().contains("instance a like this frontend"),
        "{error}"
    );

    let state = format!("--state-store=sqlite://{}/state.db", dir.0.display());
    let socket = format!("--handover-socket={}/control.sock", dir.0.display());
    let url = format!("--storage=http://{}", node.addr);
    let no_instance = flags(&[&url, &state, &socket]).expect("flags");
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&no_instance, stopped)
        .await
        .err()
        .expect("refused");
    assert!(error.to_string().contains("without --instance"), "{error}");

    let good = handover_flags(&node, &dir, "b", &[]);
    let error = start_or_take_over(&good, || Ok(std::future::ready(())))
        .await
        .err()
        .expect("stopped");
    assert!(
        error.to_string().contains("stopped before the handover"),
        "{error}"
    );
    meanwhile().await;
    assert!(!old.drain.is_unready() && !old.drain.is_draining());

    let mut no_state = good.clone();
    no_state.server.state_store = None;
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&no_state, stopped)
        .await
        .err()
        .expect("no state store");
    assert!(error.to_string().contains("needs --state-store"), "{error}");
}

/// Catches (T-12): a successor that dies after the frontend serving has let go (the window
/// the handover cannot undo) leaving the farm without a frontend that works, or the
/// callers it kept without an answer, or Envoy still choosing a frontend that can start
/// nothing: the first goes not ready as soon as its successor's connection closes, a
/// start of the successor again finds nobody serving on the control socket (its file is
/// stale), takes the store cold (the epoch is bumped, the daemon's runs are lost and run
/// again), and the first, which kept the client's stream open, answers it from the result
/// the new one writes, and ends. Mutants: no watch on the successor's connection; the
/// holder exiting at the release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_dies_after_the_release_is_started_again_and_the_streams_finish() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build whose successor dies", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let first = daemon.start().await;

    // It has taken over and dies before it serves.
    let flags = handover_flags(&node, &dir, "b", &[]);
    let (_stop, stopped) = oneshot::channel::<()>();
    let dying = taking_over(&flags, stopped).await.expect("takes over");
    drop(dying);
    assert!(daemon.ended().await.code() == Code::Unavailable);
    until("the first frontend goes not ready", async || {
        old.drain.is_unready()
    })
    .await;

    let new = Proc::taking_over(&flags).await;
    let reached = Client::connect(new.reapi, new.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_ne!(
        daemon.epoch,
        first.lease_id.expect("a lease id").term,
        "a cold start bumps the epoch"
    );
    // A daemon that sees a new epoch kills the runs it had and lists none.
    assert!(daemon.heartbeat(&[]).await);
    let second = daemon.start().await;
    assert_ne!(second.lease_id, first.lease_id, "the operation runs again");
    let result = support::output(&reached, "run again", 0).await;
    assert!(daemon.report(ran(second.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
    drop((ops, client));
    old.ended().await;
}

/// Catches (T-12): a first frontend that stays in Envoy's rotation for a successor that is
/// alive and never serves (it holds the store and has said nothing since): when
/// `--handover-watchdog-secs` runs out the first goes not ready by itself, so Envoy has a
/// clean answer to give. The successor here is started and not served, with its control
/// connection open. Mutant: a watchdog that never fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_never_serves_is_given_up_on_at_the_watchdog() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(
        &node,
        &dir,
        "a",
        &["--handover-watchdog-secs=1"],
    ))
    .await;
    let (_stop, stopped) = oneshot::channel::<()>();
    let waiting = taking_over(&handover_flags(&node, &dir, "b", &[]), stopped)
        .await
        .expect("takes over");
    assert!(!old.drain.is_unready());
    until("the watchdog runs out", async || old.drain.is_unready()).await;
    drop(waiting);
    old.ended().await;
}

/// Catches: a stop without a handover that is not what it was before the update path
/// existed (the golden): a frontend that has the handover flags but is asked to stop, not
/// to hand over, drains as `kbf-server` does: not ready, new Execute refused, GOAWAY sent
/// (so `is_draining`), and it ends once its connections have closed. The control socket
/// and the relay change none of it. Mutant: the stop path going through `hand_over`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_without_a_handover_drains_as_before() {
    let node = Node::start().await;
    let dir = Dir::new();
    let flags = handover_flags(&node, &dir, "a", &[]);
    let old = Proc::cold(&flags).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let job = Job::new("refused while the frontend stops", &[]);
    client.upload(&job.blobs()).await;
    assert_eq!(readyz(old.api).await, 200);

    let Proc {
        drain,
        served,
        _stop,
        ..
    } = old;
    drop(_stop);
    until("the drain begins", async || drain.is_draining()).await;
    assert!(drain.is_unready());
    // The call is not taken: no operation comes back. How it fails is the transport's to
    // choose, not the service's. UNAVAILABLE when the service refuses it; but the connection
    // is idle, so the listener's GOAWAY and close can reach the client before or with the
    // request: CANCELLED ("connection closed"), or INTERNAL (an h2 GOAWAY, NO_ERROR); and
    // a channel that reconnects to the closed listener's port can find another process's
    // HTTP server on it (the stream then ends UNIMPLEMENTED, from a 404). None of these cuts
    // an open call: there is none. What stays fixed is that no Operation is delivered.
    let refused = client.execute_with(&job.action, false).await;
    if let Ok(mut operations) = refused {
        let first = tokio::time::timeout(Duration::from_secs(20), operations.message())
            .await
            .expect("the call is answered");
        assert!(
            !matches!(first, Ok(Some(_))),
            "a stopping frontend took an Execute: {first:?}"
        );
    }
    drop(client);
    tokio::time::timeout(Duration::from_secs(20), served)
        .await
        .expect("the frontend ends")
        .expect("the task")
        .expect("served without an error");
}

/// Catches: the Execute service taking a new call while the drain refuses (the staged
/// `Drain::refuse`, with no GOAWAY): the call gets UNAVAILABLE from the service itself, on
/// the connection that is still open, so no transport race can stand in for it (the stop
/// test above accepts any failure). Mutant: the draining check dropped from `execute`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusing_frontend_answers_a_new_execute_unavailable() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, "a", &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let job = Job::new("refused while the frontend refuses", &[]);
    client.upload(&job.blobs()).await;
    assert!(old.drain.refuse(), "the first refusal");
    assert!(!old.drain.is_draining(), "refusing sends no GOAWAY");
    let refused = client.execute_with(&job.action, false).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.code() == Code::Unavailable),
        "{refused:?}"
    );
    drop(client);
    drop(old._stop);
    tokio::time::timeout(Duration::from_secs(20), old.served)
        .await
        .expect("the frontend ends")
        .expect("the task")
        .expect("served without an error");
}

/// Catches: `READY=1` sent before the frontend serves, never sent after, or sent by a
/// frontend that cannot reach its storage node: the service manager's socket gets it
/// only once `serve` runs and the probes pass, for a start with no predecessor and for
/// one that took over alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_manager_is_told_when_the_frontend_serves() {
    let node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let expect_ready = |manager: &UnixDatagram| {
        manager
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let mut buffer = [0_u8; 16];
        let got = manager.recv(&mut buffer).expect("READY=1");
        assert_eq!(&buffer[..got], b"READY=1");
    };

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&handover_flags(&node, &dir, "a", &[]), stopped)
        .await
        .expect("a start with no predecessor");
    let handle = handle.notifying(Some(manager_path.clone().into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    let mut buffer = [0_u8; 16];
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let old = Proc::serving(handle, stop);
    expect_ready(&manager);

    let (stop, stopped) = oneshot::channel::<()>();
    let flags = handover_flags(&node, &dir, "b", &[]);
    let handle = taking_over(&flags, stopped).await.expect("takes over");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let new = Proc::serving(handle, stop);
    expect_ready(&manager);
    old.ended().await;
    drop(new);
}

/// Catches: `READY=1` (and so the successor's "serving" signal) waiting on the storage
/// node. The `--storage` node is stopped before the frontend starts, as at a cold power-up
/// where the node comes up after the frontend: the manager still gets `READY=1` (a frontend
/// that is never ready is killed at `TimeoutStartSec` and restarts with a new epoch, in a
/// loop), the listeners stay bound (the same REAPI address answers), and `/readyz` says
/// 503 with `storage` failing until the node is back, then 200. Mutant: `Ready::run` waits
/// on every probe again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_does_not_wait_for_a_storage_node_that_is_down() {
    let mut node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let flags = handover_flags(&node, &dir, "a", &[]);
    node.stop().await;

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&flags, stopped).await.expect("starts");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    let old = Proc::serving(handle, stop);
    manager
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut buffer = [0_u8; 16];
    let got = manager
        .recv(&mut buffer)
        .expect("READY=1 with the storage node down");
    assert_eq!(&buffer[..got], b"READY=1");
    assert_eq!(readyz(old.api).await, 503, "ready with no storage");
    assert!(
        std::net::TcpStream::connect(old.reapi).is_ok(),
        "the listener is gone"
    );
    assert!(!old.served.is_finished(), "the frontend ended");
}

/// Catches: flags for a handover that parse without what they need or with what they do
/// not: a socket needs a state store, the instance and the timings need the socket, the
/// defaults are the documented ones; and a start without the flag is a start as
/// `start_frontend` makes it (`start_or_take_over` with nothing to take over from).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handover_flags_need_each_other_and_a_start_without_them_is_a_plain_start() {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    for lone in [
        "--handover-socket=/run/kbf/control.sock",
        "--handover-quiesce-secs=5",
        "--handover-follow-interval-ms=5",
        "--instance=a",
    ] {
        let error = flags(&[&url, lone]).expect_err(lone);
        assert!(error.to_string().contains("--"), "{lone}: {error}");
    }
    assert!(
        flags(&[
            &url,
            "--handover-socket=/s",
            "--state-store=sqlite:///x.db",
            "--handover-follow-interval-ms=0"
        ])
        .is_err()
    );
    let given =
        flags(&[&url, "--handover-socket=/s", "--state-store=sqlite:///x.db"]).expect("flags");
    let handover = given.handover.expect("a handover");
    assert_eq!(handover.socket, PathBuf::from("/s"));
    assert_eq!(handover.quiesce, Duration::from_secs(10));
    assert_eq!(handover.follow_interval, Duration::from_millis(200));
    let tuned = flags(&[
        &url,
        "--handover-socket=/s",
        "--state-store=sqlite:///x.db",
        "--handover-quiesce-secs=3",
        "--handover-follow-interval-ms=50",
    ])
    .expect("flags");
    let handover = tuned.handover.expect("a handover");
    assert_eq!(
        (handover.quiesce, handover.follow_interval),
        (Duration::from_secs(3), Duration::from_millis(50))
    );
    assert!(flags(&[&url]).expect("flags").handover.is_none());

    let (stop, stopped) = oneshot::channel::<()>();
    let plain = taking_over(&flags(&[&url]).expect("flags"), stopped)
        .await
        .expect("a plain start");
    let served = tokio::spawn(plain.serve());
    drop(stop);
    served.await.expect("the task").expect("served");
}

/// Catches: a frontend that starts without its control socket (it would be unable to hand
/// over, and the operator would find out at the update): a socket that cannot be made
/// (here, in a directory that does not exist) fails the start, with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_socket_that_cannot_be_made_fails_the_start() {
    let node = Node::start().await;
    let dir = Dir::new();
    let nowhere = dir.0.join("missing").join("control.sock");
    let state = format!("--state-store=sqlite://{}/state.db", dir.0.display());
    let socket = format!("--handover-socket={}", nowhere.display());
    let url = format!("--storage=http://{}", node.addr);
    let flags = flags(&[&url, &state, &socket]).expect("flags");
    let error = start_frontend(&flags, || Ok(std::future::pending::<()>()))
        .err()
        .expect("no control socket");
    assert!(error.to_string().contains("No such file"), "{error}");
}
