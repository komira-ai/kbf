//! The actor on its thread: its limits, its tick and its death. The routes through it are in
//! `router_tests.rs`, the whole handler in `handler_tests.rs`.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use hyper::StatusCode;
use tokio::time::timeout;

use super::actor::{ActorTiming, QUEUE, spawn};
use super::core::{ApiError, Crash, CrashHook, CrashPoint, Reply};
use super::fakes::{Rig, req, staged};
use crate::proxy::model::Verb;
use crate::testkit::B;

const LONG: Duration = Duration::from_secs(10);

/// Waits until `done` is true, without blocking the runtime.
pub(super) async fn wait_until(mut done: impl FnMut() -> bool) {
    timeout(LONG, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the condition did not come true");
}

fn unavailable(answer: Result<Reply, ApiError>) -> String {
    match answer {
        Err(e) if e.status == StatusCode::SERVICE_UNAVAILABLE => e.message,
        other => panic!("expected a 503, got {other:?}"),
    }
}

fn on_death(tx: mpsc::Sender<()>) -> impl FnOnce() + Send + 'static {
    move || {
        let _ = tx.send(());
    }
}

#[tokio::test]
async fn the_actor_ticks_a_staging_update_forward_with_nobody_asking() {
    let rig = Rig::new("u8b-a-tick");
    let timing = ActorTiming {
        tick: Duration::from_millis(5),
        ..ActorTiming::default()
    };
    let api = spawn(rig.core(), timing, || {}).unwrap();
    api.post(req(B, "r1")).await.unwrap();
    assert_eq!(rig.sup.starts(), 0);
    rig.set_status(staged(B));
    // No call is made from here on, so no call reads the status: only the tick can hand
    // the staged update to the supervisor.
    wait_until(|| rig.sup.starts() == 1).await;
}

#[tokio::test]
async fn a_tick_of_zero_waits_on_the_timer_and_does_not_spin() {
    let rig = Rig::new("u8b-a-zero-tick");
    let timing = ActorTiming {
        tick: Duration::ZERO,
        ..ActorTiming::default()
    };
    let api = spawn(rig.core(), timing, || {}).unwrap();
    // The update is staging from here on, so every tick reads status.json.
    api.post(req(B, "r1")).await.unwrap();
    let stall = rig.status.stall();
    let before = stall.reads();
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reads = stall.reads() - before;
    let millis = started.elapsed().as_millis();
    // The runtime's timer has a granularity of a millisecond and a deadline is rounded up
    // to it, so even a tick of zero allows one read per millisecond at most (and one in
    // flight, and one between the two counts). A loop that does not wait on the timer
    // reads as fast as the thread can, which is thousands in the same time.
    assert!(
        reads as u128 <= millis + 2,
        "{reads} reads of status.json in {millis} ms"
    );
}

#[tokio::test]
async fn a_full_queue_is_a_503_at_once() {
    let rig = Rig::new("u8b-a-full");
    let stall = rig.status.stall();
    stall.close();
    let api = spawn(rig.core(), ActorTiming::default(), || {}).unwrap();
    // The first call is in the actor's hands, stuck reading status.json: the queue is empty.
    let first = tokio::spawn({
        let api = api.clone();
        async move { api.post(req(B, "r1")).await }
    });
    stall.arrived(1).await;
    // Each of these is queued (so not answered) and its caller leaves at once.
    for _ in 0..QUEUE {
        let queued = timeout(Duration::from_millis(1), api.status()).await;
        assert!(
            queued.is_err(),
            "a call was answered while the actor is stuck"
        );
    }
    let started = Instant::now();
    let message = unavailable(api.status().await);
    assert!(message.contains("overloaded"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(1));
    stall.open();
    assert!(first.await.unwrap().is_ok(), "the call in hand finishes");
}

#[tokio::test]
async fn a_call_the_actor_does_not_answer_in_time_is_a_503() {
    let rig = Rig::new("u8b-a-deadline");
    let stall = rig.status.stall();
    stall.close();
    let timing = ActorTiming {
        deadline: Duration::from_millis(100),
        ..ActorTiming::default()
    };
    let api = spawn(rig.core(), timing, || {}).unwrap();
    let started = Instant::now();
    let message = unavailable(api.post(req(B, "r1")).await);
    assert!(message.contains("in time"), "{message}");
    // The stall lets go of the call after 3 s: a caller with no deadline waits that long.
    assert!(started.elapsed() < Duration::from_secs(2));
    stall.open();
}

#[tokio::test]
async fn a_call_whose_caller_gave_up_is_not_run() {
    let rig = Rig::new("u8b-a-gaveup");
    let stall = rig.status.stall();
    stall.close();
    let api = spawn(rig.core(), ActorTiming::default(), || {}).unwrap();
    // A POST is in the actor's hands, and its caller leaves. It cannot be stopped: it will
    // finish and write its order.
    let post = tokio::spawn({
        let api = api.clone();
        async move { api.post(req(B, "r1")).await }
    });
    stall.arrived(1).await;
    post.abort();
    assert!(post.await.unwrap_err().is_cancelled());
    // An abort queues behind it, and its caller leaves too. Nobody can see its end, so it
    // must not be run: it would drop the update the POST opened.
    let abort = timeout(Duration::from_millis(50), api.abort()).await;
    assert!(
        abort.is_err(),
        "the abort was answered while the actor is stuck"
    );
    stall.open();
    // The queue is in order: this is answered after the POST and the abort were taken.
    api.status().await.unwrap();
    let verbs: Vec<Verb> = rig
        .orders
        .written
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.verb)
        .collect();
    assert_eq!(verbs, [Verb::FrontendUpdate], "the abandoned abort ran");
}

#[tokio::test]
async fn an_actor_that_crashes_is_a_503_and_runs_on_death_once() {
    let rig = Rig::new("u8b-a-crash");
    let (tx, died) = mpsc::channel();
    let api = spawn(rig.core(), ActorTiming::default(), on_death(tx)).unwrap();
    *rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    let message = unavailable(api.post(req(B, "r1")).await);
    assert!(message.contains("not running"), "{message}");
    assert_eq!(died.recv_timeout(LONG), Ok(()), "on_death did not run");
    unavailable(api.status().await);
    // The closure ran and is gone: it did not run a second time.
    assert_eq!(
        died.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Disconnected)
    );
}

struct Boom;

impl CrashHook for Boom {
    fn at(&mut self, _point: CrashPoint) -> Result<(), Crash> {
        panic!("the hook blew up")
    }
}

#[tokio::test]
async fn an_actor_that_panics_is_a_503_and_runs_on_death() {
    let rig = Rig::new("u8b-a-panic");
    let (tx, died) = mpsc::channel();
    let mut core = rig.core();
    core.set_hook(Box::new(Boom));
    let api = spawn(core, ActorTiming::default(), on_death(tx)).unwrap();
    unavailable(api.post(req(B, "r1")).await);
    assert_eq!(died.recv_timeout(LONG), Ok(()), "on_death did not run");
    unavailable(api.status().await);
}

#[tokio::test]
async fn an_actor_nobody_can_call_any_more_ends_without_on_death() {
    let rig = Rig::new("u8b-a-closed");
    let (tx, died) = mpsc::channel();
    let api = spawn(rig.core(), ActorTiming::default(), on_death(tx)).unwrap();
    api.status().await.unwrap();
    drop(api);
    // The actor ends and drops the closure unused: the sender goes without a message.
    assert_eq!(
        died.recv_timeout(LONG),
        Err(RecvTimeoutError::Disconnected),
        "on_death ran for an actor that was only left"
    );
}
