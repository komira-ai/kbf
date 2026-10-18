//! A frontend that serves, asked to hand over by successors the tests play.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use kbf_front::Stage;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::*;
use crate::handover::tests::{Rig, request, submit};
use crate::handover::wire::WireError;

const LONG: Duration = Duration::from_secs(600);

/// The REAPI address a successor says it listens on.
const SUCCESSOR_REAPI: &str = "127.0.0.1:18990";

fn hello(change: impl FnOnce(&mut Message)) -> Message {
    let mut hello = Message::Hello {
        version: PROTOCOL,
        instance: Some(Instance::B),
        reapi: SUCCESSOR_REAPI.parse().expect("address"),
        state: PathBuf::from("/var/lib/kbf/state.db"),
    };
    change(&mut hello);
    hello
}

async fn connect(rig: &Rig) -> Link {
    Link::new(
        UnixStream::connect(rig.dir.socket())
            .await
            .expect("connect"),
    )
}

/// Plays a successor as far as `released`: hello, takes the offer, ready. The link, to
/// say `serving` on or to close.
async fn take_over(rig: &Rig) -> Result<(Link, Message), WireError> {
    let mut link = connect(rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    let offer = link.hear().await?;
    assert!(matches!(offer, Message::Offer { .. }), "{offer:?}");
    link.say(&Message::Ready).await.expect("ready");
    let answer = link.hear().await?;
    Ok((link, answer))
}

/// A holder serving the rig's control socket, and the receiver that says how it ended.
fn serve(
    holder: Holder,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<Result<(), String>>,
) {
    let (departing, departed) = oneshot::channel();
    (tokio::spawn(holder.run(departing)), departed)
}

/// Whether the rig's frontend has not been touched: not handed over, not fenced, its
/// store not given up.
fn untouched(rig: &Rig) -> bool {
    !rig.readiness.is_handed_over()
        && !kbf_front::Handed::handed(&*rig.farm)
        && rig.store.demoted.load(Ordering::SeqCst) == 0
}

/// Waits until the rig's frontend has gone not ready, for at most twenty seconds.
async fn until_unready(rig: &Rig) {
    let waiting = async {
        while !rig.drain.is_unready() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), waiting)
        .await
        .expect("the frontend never went not ready");
}

/// Gives a holder that should not have acted yet the time it would have taken to.
async fn meanwhile() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Catches: a holder that stands down in the wrong order, leaves a step out, or does a
/// step that Envoy's rotation must not see yet. When the writer role is given up,
/// everything before it is done (handed over, the farm fenced, the sweeper aborted, the
/// worker listener stopped, the daemons' streams ended), and the successor is told only
/// after, with the generation of the store. The holder stays ready through all of it (not
/// draining, no GOAWAY, readiness not stopping), names its successor for the relay, and
/// goes not ready only after the successor says it serves. A holder that fences after
/// giving up the store, lets the daemons' streams run on, goes unready at the cut, or
/// sends GOAWAY fails here.
#[tokio::test]
async fn a_holder_stands_down_in_order_and_leaves_only_once_its_successor_serves() {
    let rig = Rig::new();
    let seen = Arc::new(Mutex::new(None));
    let during = Arc::clone(&seen);
    let (drain, readiness, worker, sweeper, farm) = (
        Arc::clone(&rig.drain),
        Arc::clone(&rig.readiness),
        rig.worker.clone(),
        rig.sweeper.clone(),
        Arc::clone(&rig.farm),
    );
    rig.store.on_demote(move || {
        let fenced = submit(&farm, request(9)).is_err_and(|e| e.code() == tonic::Code::Unavailable);
        *during.lock().expect("lock") = Some([
            readiness.is_handed_over(),
            fenced,
            sweeper.is_aborted(),
            worker.fired().now_or_never().is_some(),
            !drain.is_draining() && !readiness.is_stopping(),
        ]);
    });
    let (outbound, mut inbox) = mpsc::unbounded_channel();
    let node = kbf_types::WorkerId::new("node-a");
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    rig.farm.register(
        &node,
        kbf_sched::DaemonInstance::new("daemon"),
        kbf_sched::DaemonInstance::new(""),
        crate::farm::Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    assert!(inbox.try_recv().is_ok(), "the Welcome");

    let (served, mut departed) = serve(rig.holder(Rig::uid(), LONG));
    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    assert_eq!(
        link.hear().await.expect("the offer"),
        Message::Offer { generation: 7 }
    );
    assert!(untouched(&rig), "the holder moved before ready");

    link.say(&Message::Ready).await.expect("ready");
    assert_eq!(link.hear().await.expect("released"), Message::Released);
    assert_eq!(*seen.lock().expect("lock"), Some([true; 5]));
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 1);
    let ended = inbox
        .try_recv()
        .expect("the stream is ended")
        .expect_err("with a status");
    assert_eq!(ended.code(), tonic::Code::Unavailable);

    // Released, and still in Envoy's rotation: the successor has not said it serves.
    meanwhile().await;
    assert!(
        !rig.drain.is_unready(),
        "not ready before the successor serves"
    );
    assert!(!rig.drain.is_draining(), "GOAWAY in an update");
    assert!(
        departed.try_recv().is_err(),
        "left before the successor served"
    );

    link.say(&Message::Serving).await.expect("serving");
    served.await.expect("the holder");
    assert_eq!(departed.await.expect("told"), Ok(()));
    assert!(rig.drain.is_unready());
}

/// Catches: a holder that hands over to a process started with other flags (another state
/// file, the same instance, no instance, a REAPI address that is not loopback, or a
/// certificate it cannot verify), or to one that speaks another protocol or opens with
/// something else than a hello; and a holder started without `--instance`, which cannot
/// tell its peer from itself. Each refusal names why and leaves the frontend untouched
/// (nothing is handed over, and no successor is named for the relay), and a good
/// successor still takes over after them all. Mutants: a comparison dropped.
#[tokio::test]
async fn a_successor_started_with_other_flags_is_refused_and_nothing_changes() {
    let rig = Rig::new();
    let (served, _departed) = serve(rig.holder(Rig::uid(), LONG));
    let cases: Vec<(Message, &str)> = vec![
        (
            hello(|h| {
                if let Message::Hello { version, .. } = h {
                    *version = 1;
                }
            }),
            "protocol 2, not 1",
        ),
        (
            hello(|h| {
                if let Message::Hello { instance, .. } = h {
                    *instance = Some(Instance::A);
                }
            }),
            "instance a like this frontend",
        ),
        (
            hello(|h| {
                if let Message::Hello { instance, .. } = h {
                    *instance = None;
                }
            }),
            "successor was started without --instance",
        ),
        (
            hello(|h| {
                if let Message::Hello { reapi, .. } = h {
                    *reapi = "192.0.2.1:9".parse().expect("address");
                }
            }),
            "not loopback",
        ),
        (
            hello(|h| {
                if let Message::Hello { state, .. } = h {
                    *state = PathBuf::from("/other.db");
                }
            }),
            "--state-store",
        ),
        (Message::Ready, "must be hello"),
    ];
    for (message, why) in cases {
        let mut link = connect(&rig).await;
        link.say(&message).await.expect("say");
        match link.hear().await.expect("an answer") {
            Message::Refused(said) => assert!(said.contains(why), "{said}"),
            other => panic!("{message:?} was answered {other:?}"),
        }
        assert!(untouched(&rig), "{message:?}");
    }
    let (mut link, released) = take_over(&rig).await.expect("a good successor");
    assert_eq!(released, Message::Released);
    link.say(&Message::Serving).await.expect("serving");
    served.await.expect("the holder");
}

/// Catches: a holder that goes on with a successor whose certificate it cannot verify
/// (the relay would have no channel, or a plain one to a TLS listener), and one started
/// without `--instance` (it cannot tell its peer from itself): each refuses with the
/// reason it was given.
#[tokio::test]
async fn a_holder_that_cannot_reach_or_tell_its_successor_refuses() {
    let rig = Rig::new();
    let peer = rig.peer(Err("needs --handover-peer-name"));
    let (served, _departed) = serve(rig.holder_with(Rig::uid(), LONG, peer, LONG));
    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    match link.hear().await.expect("an answer") {
        Message::Refused(said) => assert!(said.contains("--handover-peer-name"), "{said}"),
        other => panic!("{other:?}"),
    }
    assert!(untouched(&rig));
    served.abort();

    let mut rig = Rig::new();
    rig.facts.instance = None;
    let (served, _departed) = serve(rig.holder(Rig::uid(), LONG));
    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    match link.hear().await.expect("an answer") {
        Message::Refused(said) => assert!(said.contains("without --instance"), "{said}"),
        other => panic!("{other:?}"),
    }
    assert!(untouched(&rig));
    served.abort();
}

/// Catches: a handover that is half done when its successor fails or goes away before
/// `ready` (the frontend would be handed over with nobody to serve): a successor that
/// closes after its hello, after the offer, says something else than `ready`, or sends
/// what is not a message. In each the frontend is untouched, and the next successor takes
/// over. Mutant: the stand down starting at the offer.
#[tokio::test]
async fn a_successor_that_fails_before_ready_changes_nothing() {
    let rig = Rig::new();
    let (served, _departed) = serve(rig.holder(Rig::uid(), LONG));

    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    drop(link);

    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    link.hear().await.expect("the offer");
    drop(link);

    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    link.hear().await.expect("the offer");
    link.say(&Message::Released).await.expect("something else");

    let mut link = connect(&rig).await;
    link.say_raw(b"not a message\n").await;

    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 0);
    let (mut link, released) = take_over(&rig).await.expect("a good successor");
    assert_eq!(released, Message::Released);
    assert!(
        rig.store.demoted.load(Ordering::SeqCst) == 1,
        "only the good one stood the frontend down"
    );
    link.say(&Message::Serving).await.expect("serving");
    served.await.expect("the holder");
}

/// Catches: a control socket that serves another user (any local process could take the
/// frontend's work): a peer that is not the user the holder runs as is closed on without
/// an answer, however many come.
#[tokio::test]
async fn a_peer_that_is_another_user_is_not_answered() {
    let rig = Rig::new();
    let (served, _departed) = serve(rig.holder(Rig::uid() + 1, LONG));
    for _ in 0..2 {
        let mut link = connect(&rig).await;
        let _ = link.say(&hello(|_| {})).await;
        let closed = link.hear().await.expect_err("no answer");
        assert!(
            matches!(closed, WireError::Closed | WireError::Io(_)),
            "{closed}"
        );
        assert!(untouched(&rig));
    }
    served.abort();
}

/// Catches: a holder that gives up the writer role while a call is between its request
/// and its queueing, or a result is being taken (the result could be lost, or the call
/// answered by neither process), and one that waits for ever for such a call: it stands
/// down only once the farm is quiet, or the wait is up. The farm still takes work until
/// then.
#[tokio::test]
async fn a_holder_waits_for_the_farm_to_be_quiet_for_a_bounded_time() {
    let rig = Rig::new();
    let busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    let (served, _departed) = serve(rig.holder(Rig::uid(), LONG));
    let waiting = tokio::spawn({
        let readiness = Arc::clone(&rig.readiness);
        async move {
            while !readiness.is_handed_over() {
                tokio::task::yield_now().await;
            }
        }
    });
    let mut link = connect(&rig).await;
    link.say(&hello(|_| {})).await.expect("hello");
    link.hear().await.expect("the offer");
    link.say(&Message::Ready).await.expect("ready");
    waiting.await.expect("the stand down began");
    assert!(
        submit(&rig.farm, request(1)).is_ok(),
        "fenced while a call was in flight"
    );
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 0);
    drop(busy);
    assert_eq!(link.hear().await.expect("released"), Message::Released);
    assert!(submit(&rig.farm, request(2)).is_err());
    served.abort();

    // A wait that is up stands down with the farm still busy.
    let rig = Rig::new();
    let _busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    let (served, _departed) = serve(rig.holder(Rig::uid(), Duration::ZERO));
    let (_link, released) = take_over(&rig).await.expect("released");
    assert_eq!(released, Message::Released);
    served.abort();
}

/// Catches: a holder that cannot give up the writer role and says it did (the successor
/// would open a store the holder still writes), or goes on serving half stood down: the
/// successor is told `failed` with the reason, the drain is closed so the frontend ends
/// its streams, and the process is told to exit with the reason (not as if it held
/// nothing).
#[tokio::test]
async fn a_holder_that_cannot_give_up_the_store_says_failed_and_closes_its_drain() {
    let rig = Rig::new();
    rig.store.refuses_to_demote.store(true, Ordering::SeqCst);
    let (served, departed) = serve(rig.holder(Rig::uid(), LONG));
    let (_link, answer) = take_over(&rig).await.expect("an answer");
    match answer {
        Message::Failed(why) => assert!(why.contains("the writer cannot stop"), "{why}"),
        other => panic!("{other:?}"),
    }
    served.await.expect("the task ends");
    let told = departed.await.expect("told");
    assert!(
        told.expect_err("an error")
            .contains("the writer cannot stop")
    );
    rig.drain.closed().await;
}

/// Catches: a control socket that cannot replace the one a dead frontend left (the next
/// start would fail with "address in use"), is readable by other users, or that is made
/// where it cannot be.
#[tokio::test]
async fn the_control_socket_replaces_a_stale_one_and_is_for_this_user() {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    let rig = Rig::new();
    let path = rig.dir.socket();
    std::fs::write(&path, b"left behind").expect("a stale file");
    let _holder = rig.holder(Rig::uid(), LONG);
    let meta = std::fs::metadata(&path).expect("the socket");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let nowhere = rig.dir.0.join("missing").join("control.sock");
    let error = Holder::bind(
        &nowhere,
        Rig::uid(),
        rig.facts.clone(),
        rig.stand(LONG),
        rig.peer(Ok(())),
        rig.leave(LONG),
    )
    .err()
    .expect("no directory");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// Catches: an offer that names the wrong generation: the store's own, 0 for a store
/// that has none, and 0 (not a panic) for a row that is not a number.
#[tokio::test]
async fn the_offered_generation_is_the_stores() {
    let rig = Rig::new();
    let holder = rig.holder(Rig::uid(), LONG);
    assert_eq!(holder.generation().expect("generation"), 7);
    rig.store.put(kbf_state::GENERATION, vec![1, 2, 3]);
    assert_eq!(holder.generation().expect("generation"), 0);
    rig.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(holder.generation().is_err());
}

/// Plays what a holder does after `released` with a successor the test holds the other end
/// of; the link of the successor, and the holder's way of saying it left.
fn released_holder(
    rig: &Rig,
    watchdog: Duration,
) -> (
    Link,
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<Result<(), String>>,
) {
    let (ours, theirs) = UnixStream::pair().expect("a pair");
    let leaving = Leaving {
        drain: Arc::clone(&rig.drain),
        farm: Arc::clone(&rig.farm),
        leave: rig.leave(watchdog),
    };
    let (departing, departed) = oneshot::channel();
    let task = tokio::spawn(async move {
        leaving.run(Link::new(ours)).await;
        let _ = departing.send(Ok(()));
    });
    (Link::new(theirs), task, departed)
}

/// Catches: a holder that is still in Envoy's rotation when its successor is gone (T-12:
/// it would go on being chosen while it can start nothing): the successor's connection
/// closing before it served makes the holder go not ready at once, and a holder that
/// then holds nothing leaves. Mutant: no watch on the connection.
#[tokio::test]
async fn a_successor_that_closes_its_connection_makes_the_holder_unready() {
    let rig = Rig::new();
    let (link, task, _departed) = released_holder(&rig, LONG);
    meanwhile().await;
    assert!(
        !rig.drain.is_unready(),
        "unready with a successor that may yet serve"
    );
    drop(link);
    until_unready(&rig).await;
    task.await.expect("a holder that holds nothing leaves");
}

/// Catches: a holder that waits for ever for a successor that is alive and never serves
/// (T-12: the successor wedged, or never reached ready): when `--handover-watchdog-secs`
/// runs out the holder goes not ready by itself, and leaves when it holds nothing.
/// Mutant: a watchdog that never fires.
#[tokio::test]
async fn a_successor_that_never_serves_is_given_up_on_at_the_watchdog() {
    let rig = Rig::new();
    let (_link, task, _departed) = released_holder(&rig, Duration::from_millis(20));
    until_unready(&rig).await;
    task.await.expect("the holder leaves");
}

/// Catches: a successor that says something else where `serving` belongs being taken for
/// serving (the holder would wait out the settle for a successor that is not ready), so
/// it is given up on at once.
#[tokio::test]
async fn something_other_than_serving_is_not_serving() {
    let rig = Rig::new();
    let (mut link, task, _departed) = released_holder(&rig, LONG);
    link.say(&Message::Ready).await.expect("say");
    until_unready(&rig).await;
    task.await.expect("the holder leaves");
}

/// Catches: a holder that exits while it still holds a stream, a relay or a caller (a build
/// fails: the relay is the only path of a forwarded call), and one that never exits once
/// it holds none. Each of the three is held alone, after the holder went not ready; it
/// leaves only when that one is let go. Mutants: the exit check without the relays, or
/// without the open streams, or without the callers.
#[tokio::test]
async fn a_holder_leaves_only_when_it_holds_no_stream_no_relay_and_no_caller() {
    for held in ["a stream", "a relay", "a caller"] {
        let rig = Rig::new();
        let mut ticket =
            (held == "a caller").then(|| submit(&rig.farm, request(1)).expect("queued"));
        rig.farm.fence();
        let metrics = rig.backend.metrics();
        let stream = (held == "a stream").then(|| metrics.execute.open_executions.raise());
        let relay = (held == "a relay").then(|| metrics.handover_relays_open.raise());
        let (link, task, mut departed) = released_holder(&rig, LONG);
        drop(link);
        until_unready(&rig).await;

        meanwhile().await;
        assert!(departed.try_recv().is_err(), "left holding {held}");
        drop((stream, relay));
        if let Some(ticket) = &mut ticket {
            let finished = kbf_front::Finished::Failed(kbf_proto::google::rpc::Status {
                code: 9,
                message: "the successor's answer".to_owned(),
                details: Vec::new(),
            });
            rig.store
                .put(&format!("done/{:016x}", 0), finished.to_row());
            let answered = ticket
                .stage
                .wait_for(|stage| matches!(stage, Stage::Done(_)))
                .await
                .expect("the stage changes");
            assert_eq!(*answered, Stage::Done(finished));
        }
        task.await
            .expect("the holder leaves with the last thing it held");
    }
}

/// Catches: a follower that does not read the store again after it failed to (one bad
/// read ends the answers) or that leaves with a read failing (the holder would exit
/// with callers it could not answer): reads fail, then recover, and only then is the
/// holder allowed to leave.
#[tokio::test]
async fn a_holder_whose_store_cannot_be_read_goes_on_looking() {
    let rig = Rig::new();
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.fence();
    rig.store.reads_fail.store(true, Ordering::SeqCst);
    let (link, task, mut departed) = released_holder(&rig, LONG);
    drop(link);
    while rig.store.reads_failed.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    assert!(
        departed.try_recv().is_err(),
        "left with an unreadable store"
    );
    task.abort();
}
