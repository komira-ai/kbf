//! When a drain stands down and what it says: only once the last stream of every build has
//! ended, saying nothing was left; or when it is ended by force, saying how many streams of how
//! many builds, and how many other calls, it cuts. Time is paused: an hour of waiting costs
//! nothing, and no assertion is about how long anything took.

use kbf_front::InvocationId;
use kbf_front::metrics::{OpenExecute, Raised};

use super::*;

/// An hour: far longer than any wait of the sequence, which only an open stream or call holds.
const HOUR: Duration = Duration::from_secs(3_600);

/// A stream open on the rig's front, of the build `build` (none when `None`), as an Execute
/// that names its build in its `RequestMetadata` makes one.
fn stream(rig: &Rig, build: Option<&str>) -> OpenExecute {
    let mut open = rig.backend.metrics().execute.execute();
    open.build(build.map(|id| InvocationId::parse(id).expect("an id kbf accepts")));
    open.streaming();
    open
}

/// What the rig's drain logs once the drain has ended: the stop `stop` returns.
async fn ended(stop: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(HOUR, stop)
        .await
        .expect("the drain ends")
        .expect("the stop's task");
}

/// Catches a drain that stands down before the last stream of every build has ended (an
/// update that cuts a running build's actions): with streams of two builds open after the stop,
/// the drain stays `draining` for hours with each but the last gone, stands down only when the
/// last one ends, and then says nothing was left to answer, with no WARN of a cut.
#[tokio::test(start_paused = true)]
async fn the_drain_stands_down_only_once_the_last_stream_of_every_build_has_ended() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(0));
    let mut open = vec![
        stream(&rig, Some("build-a")),
        stream(&rig, Some("build-b")),
        stream(&rig, Some("build-a")),
    ];
    let stop = begin(&rig, Begin::Signal, Duration::MAX);

    while let Some(last) = open.pop() {
        tokio::time::sleep(HOUR).await;
        assert_eq!(
            rig.drain.phase(),
            Phase::Draining,
            "{} open",
            open.len() + 1
        );
        assert!(!stop.is_finished(), "stood down with a stream open");
        drop(last);
    }
    ended(stop).await;
    assert_eq!(rig.drain.phase(), Phase::Closing);
    let text = log.text();
    assert!(
        text.contains("draining: nothing is left to answer, standing down"),
        "{text}"
    );
    assert!(!text.contains("ended by force"), "{text}");
    assert!(!text.contains("WARN"), "{text}");
}

/// The WARN a forced stop logs over the streams of [`forced`]: four streams, three of two
/// builds and one that named none, and one other call.
const CUT: &str = "the drain is ended by force: it cuts 4 open Execute or WaitExecution stream(s) \
                   of 2 build(s), which end UNAVAILABLE and fail their actions (1 of the streams \
                   named no build), and 1 other call(s)";

/// The streams and calls [`forced`] holds open until it is dropped.
struct Held<'a> {
    _streams: Vec<OpenExecute>,
    _calls: Vec<Raised<'a>>,
}

/// Opens on `rig` what [`CUT`] counts, and begins a stop with `timeout`: four streams, three
/// of two builds and one that named none, each also a call as an open stream is, and a fifth
/// call that is no stream (a ByteStream write).
fn forced(rig: &Rig, timeout: Duration) -> (Held<'_>, tokio::task::JoinHandle<()>) {
    let builds = [Some("build-a"), Some("build-a"), Some("build-b"), None];
    let calls = &rig.backend.metrics().execute.open_calls;
    let held = Held {
        _streams: builds.into_iter().map(|b| stream(rig, b)).collect(),
        _calls: (0..5).map(|_| calls.raise()).collect(),
    };
    (held, begin(rig, Begin::Signal, timeout))
}

/// Catches a second stop signal that cuts streams without saying so, or says it twice: the
/// close says, once, how many streams of how many builds and how many other calls it cuts, and
/// the drain then stands down saying it was ended by force.
#[tokio::test(start_paused = true)]
async fn a_second_signal_says_how_many_streams_of_how_many_builds_it_cuts() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(0));
    let (_held, stop) = forced(&rig, Duration::MAX);
    tokio::time::sleep(HOUR).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);

    // What `start::escalating` does on the second signal; the next close cuts nothing new.
    rig.drain.close();
    rig.drain.close();
    ended(stop).await;
    let text = log.text();
    assert_eq!(text.matches(CUT).count(), 1, "{text}");
    assert!(text.contains("WARN"), "{text}");
    assert!(
        text.contains("draining: ended by force, standing down"),
        "{text}"
    );
    assert!(!text.contains("nothing is left to answer"), "{text}");
}

/// Catches `--shutdown-timeout-secs` cutting streams without saying so: when it runs out with
/// streams open, the same WARN says what it cut.
#[tokio::test(start_paused = true)]
async fn a_shutdown_timeout_says_how_many_streams_of_how_many_builds_it_cuts() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(0));
    let (_held, stop) = forced(&rig, Duration::from_secs(30));
    ended(stop).await;
    let text = log.text();
    assert_eq!(text.matches(CUT).count(), 1, "{text}");
    assert!(text.contains("ended by force, standing down"), "{text}");
}

/// Catches a forced stop that says nothing of the calls it cuts when no stream is open: a
/// ByteStream write still being answered (a call, no stream) is named in the WARN.
#[tokio::test(start_paused = true)]
async fn a_forced_stop_with_only_a_call_open_says_it_cuts_that_call() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(0));
    let _write = rig.backend.metrics().execute.open_calls.raise();
    let stop = begin(&rig, Begin::Signal, Duration::MAX);
    rig.drain.close();
    ended(stop).await;
    let text = log.text();
    let cut = "it cuts 0 open Execute or WaitExecution stream(s) of 0 build(s), which end \
               UNAVAILABLE and fail their actions (0 of the streams named no build), and 1 other \
               call(s)";
    assert_eq!(text.matches(cut).count(), 1, "{text}");
}

/// Catches a forced stop that warns of a cut with nothing open: a second signal after the last
/// stream ended cuts nothing, says nothing of a cut, and the drain ends.
#[tokio::test(start_paused = true)]
async fn a_forced_stop_with_nothing_open_warns_of_no_cut() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(86_400));
    let stop = begin(&rig, Begin::Signal, Duration::MAX);
    rig.drain.close();
    ended(stop).await;
    let text = log.text();
    assert!(!text.contains("it cuts"), "{text}");
    assert!(!text.contains("WARN"), "{text}");
    assert!(text.contains("ended by force, standing down"), "{text}");
}
