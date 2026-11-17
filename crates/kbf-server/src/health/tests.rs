//! The checks of `/readyz` answered from the last answers of their probes
//! ([`Checks::begin`]): the clock is paused, so a wait is virtual and no assertion depends on
//! speed.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use futures::FutureExt;

use super::*;

const INTERVAL: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(5);

const UP: u8 = 0;
const DOWN: u8 = 1;
const HANG: u8 = 2;

/// A dependency whose answer the test sets, counting the times it is asked.
struct Switch {
    mode: Arc<AtomicU8>,
    asked: Arc<AtomicUsize>,
}

impl Probe for Switch {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.load(Ordering::SeqCst);
        Box::pin(async move {
            match mode {
                UP => Ok(()),
                DOWN => Err("it is down".to_owned()),
                _ => std::future::pending().await,
            }
        })
    }
}

struct Rig {
    mode: Arc<AtomicU8>,
    asked: Arc<AtomicUsize>,
    readiness: Arc<Readiness>,
    checks: Arc<Checks>,
}

fn rig(mode: u8) -> Rig {
    let mode = Arc::new(AtomicU8::new(mode));
    let asked = Arc::new(AtomicUsize::new(0));
    let readiness = Arc::new(Readiness::default());
    let checks = Checks::new(Arc::clone(&readiness)).probing(
        "dependency",
        Arc::new(Switch {
            mode: Arc::clone(&mode),
            asked: Arc::clone(&asked),
        }),
    );
    Rig {
        mode,
        asked,
        readiness,
        checks: Arc::new(checks),
    }
}

/// Starts the background asks of `checks` and returns the task that runs them.
async fn begun(checks: &Arc<Checks>) -> tokio::task::JoinHandle<()> {
    let keeping = checks.begin(INTERVAL, TIMEOUT).await;
    let task = tokio::spawn(keeping);
    // Run it to its first wait, so the interval counts from here.
    tokio::task::yield_now().await;
    task
}

/// What `/readyz` answers now, which must need no wait: the failing checks as
/// `name: reason`.
fn answer_now(checks: &Checks) -> Vec<String> {
    checks
        .failing()
        .now_or_never()
        .expect("answered without waiting for a probe")
        .into_iter()
        .map(|failing| format!("{}: {}", failing.check, failing.reason))
        .collect()
}

/// Lets the tasks of the test run until `done` holds.
async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the condition never held");
}

/// Asserts that, while a probe is hanging in the background, `/readyz` answers at once with
/// what the last ask found, and asks no probe itself: every call returns without a wait, the
/// probe is asked by the background only, and not once more however often `/readyz` is read.
#[tokio::test(start_paused = true)]
async fn readyz_answers_at_once_while_a_probe_hangs_and_asks_no_probe() {
    let rig = rig(UP);
    let keeping = begun(&rig.checks).await;
    assert_eq!(rig.asked.load(Ordering::SeqCst), 1, "the first ask");
    assert!(answer_now(&rig.checks).is_empty());

    rig.mode.store(HANG, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| rig.asked.load(Ordering::SeqCst) == 2).await;
    for _ in 0..3 {
        assert!(
            answer_now(&rig.checks).is_empty(),
            "the answer of the last ask, while the next one hangs"
        );
    }
    assert_eq!(rig.asked.load(Ordering::SeqCst), 2, "a read asked a probe");
    keeping.abort();
}

/// Asserts that a probe that has not answered within its timeout shows as failed, with the
/// wait named, and that the wait is no part of any `/readyz`: the checks begin, with the first
/// ask hanging, and the first answer is the failure.
#[tokio::test(start_paused = true)]
async fn a_probe_with_no_answer_in_its_timeout_shows_as_failed() {
    let rig = rig(HANG);
    let keeping = begun(&rig.checks).await;
    assert_eq!(
        answer_now(&rig.checks),
        ["dependency: the dependency probe had no answer within 5000 ms"]
    );

    // Hanging at a later ask too: the failure of its timeout replaces the last answer.
    rig.mode.store(UP, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| answer_now(&rig.checks).is_empty()).await;
    rig.mode.store(HANG, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| rig.asked.load(Ordering::SeqCst) == 3).await;
    assert!(
        answer_now(&rig.checks).is_empty(),
        "still within its timeout"
    );
    tokio::time::advance(TIMEOUT).await;
    until(|| !answer_now(&rig.checks).is_empty()).await;
    assert_eq!(
        answer_now(&rig.checks),
        ["dependency: the dependency probe had no answer within 5000 ms"]
    );
    keeping.abort();
}

/// Asserts that the answer follows the probe: its own reason when it fails, and ready again
/// when it answers.
#[tokio::test(start_paused = true)]
async fn a_failing_probe_shows_its_reason_and_recovers() {
    let rig = rig(UP);
    let keeping = begun(&rig.checks).await;
    assert!(answer_now(&rig.checks).is_empty());

    rig.mode.store(DOWN, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| !answer_now(&rig.checks).is_empty()).await;
    assert_eq!(answer_now(&rig.checks), ["dependency: it is down"]);

    rig.mode.store(UP, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| answer_now(&rig.checks).is_empty()).await;
    keeping.abort();
}

/// Asserts that a drain turns `/readyz` to 503 at once, with the probes' last answers still
/// cached and no new ask: the stop signal, the scheduler role and the hand-over are read at
/// each request, and a skipped probe is not listed.
#[tokio::test(start_paused = true)]
async fn the_flags_change_the_answer_at_once() {
    let rig = rig(DOWN);
    let keeping = begun(&rig.checks).await;
    let asked = rig.asked.load(Ordering::SeqCst);
    assert_eq!(answer_now(&rig.checks), ["dependency: it is down"]);

    rig.readiness.stop();
    assert_eq!(
        answer_now(&rig.checks),
        [
            "stopping: a stop signal was received; the server is shutting down",
            "dependency: it is down"
        ]
    );
    rig.readiness.set_leader(false);
    assert_eq!(answer_now(&rig.checks).len(), 3);
    let skipped = rig
        .checks
        .failing_except(&["dependency"])
        .now_or_never()
        .expect("answered without waiting");
    assert_eq!(
        skipped
            .iter()
            .map(|failing| failing.check)
            .collect::<Vec<_>>(),
        ["stopping", "leader"]
    );

    rig.readiness.set_handed_over();
    assert_eq!(
        answer_now(&rig.checks),
        [
            "stopping: a stop signal was received; the server is shutting down",
            "leader: this server does not hold the scheduler role"
        ],
        "a handed-over server lists no probe"
    );
    assert_eq!(
        rig.asked.load(Ordering::SeqCst),
        asked,
        "a flag asked a probe"
    );
    keeping.abort();
}

/// Asserts that checks that were not begun ask their probes on each request, as the server's
/// own `/readyz` does.
#[tokio::test(start_paused = true)]
async fn checks_not_begun_ask_their_probes_on_each_request() {
    let rig = rig(DOWN);
    assert_eq!(rig.checks.failing().await.len(), 1);
    rig.mode.store(UP, Ordering::SeqCst);
    assert!(rig.checks.failing().await.is_empty());
    assert_eq!(rig.asked.load(Ordering::SeqCst), 2);
}

/// Asserts that a probe that hangs delays no other: each has its own interval and timeout, so
/// the answer of the one that works keeps being renewed while the other waits out its timeout.
#[tokio::test(start_paused = true)]
async fn a_hanging_probe_delays_no_other() {
    let hung = rig(HANG);
    let other_mode = Arc::new(AtomicU8::new(UP));
    let other_asked = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(
        Checks::new(Arc::clone(&hung.readiness))
            .probing(
                "hangs",
                Arc::new(Switch {
                    mode: Arc::clone(&hung.mode),
                    asked: Arc::clone(&hung.asked),
                }),
            )
            .probing(
                "works",
                Arc::new(Switch {
                    mode: Arc::clone(&other_mode),
                    asked: Arc::clone(&other_asked),
                }),
            ),
    );
    let keeping = begun(&checks).await;
    other_mode.store(DOWN, Ordering::SeqCst);
    tokio::time::advance(INTERVAL).await;
    until(|| answer_now(&checks).len() == 2).await;
    assert_eq!(
        answer_now(&checks),
        [
            "hangs: the hangs probe had no answer within 5000 ms",
            "works: it is down"
        ]
    );
    keeping.abort();
}

/// What `GET path` answers from `router`: the status and the JSON body.
async fn get_json(router: &mut Router, path: &str) -> (StatusCode, serde_json::Value) {
    use tower_service::Service as _;
    let request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .expect("a request");
    let answer = router.call(request).await.expect("an answer");
    let code = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    (code, serde_json::from_slice(&bytes).expect("JSON"))
}

/// Asserts that `/readyz/reapi` fails with the check `no_executor` (and its reason) on a server
/// that has no executor, while `/readyz` of the same server answers ready: an Execute there
/// would be refused, so the REAPI balancer must not send it any; and that the check is gone
/// when the server has an executor, the other checks still apply to it, and the check follows
/// a failing probe's own name.
#[tokio::test]
async fn readyz_reapi_fails_no_executor_without_an_executor_and_passes_with_one() {
    let rig = rig(UP);
    let mut without = routes(&rig.checks, false, None);
    let (code, body) = get_json(&mut without, "/readyz/reapi").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["ready"], false);
    assert_eq!(body["failing"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["failing"][0]["check"], "no_executor");
    assert_eq!(
        body["failing"][0]["reason"],
        "this server runs no executor (--executor js-agent)"
    );
    let (code, body) = get_json(&mut without, "/readyz").await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let mut with = routes(&rig.checks, true, None);
    let (code, body) = get_json(&mut with, "/readyz/reapi").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["ready"], true);
    assert_eq!(body["failing"].as_array().map(Vec::len), Some(0), "{body}");

    rig.mode.store(DOWN, Ordering::SeqCst);
    let (code, body) = get_json(&mut with, "/readyz/reapi").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["failing"][0]["check"], "dependency");
    let (_, body) = get_json(&mut without, "/readyz/reapi").await;
    let checks: Vec<&str> = body["failing"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|f| f["check"].as_str())
        .collect();
    assert_eq!(checks, ["dependency", "no_executor"]);
}

/// Asserts that a stop signal turns `/readyz/reapi` to 503 on a server with an executor, so a
/// drain takes the server out of the REAPI balancer's rotation.
#[tokio::test]
async fn readyz_reapi_follows_the_stop_signal_with_an_executor() {
    let rig = rig(UP);
    let mut with = routes(&rig.checks, true, None);
    rig.readiness.stop();
    let (code, body) = get_json(&mut with, "/readyz/reapi").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["failing"][0]["check"], "stopping");
}
