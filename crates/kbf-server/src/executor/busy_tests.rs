//! What a busy job service is, and how long the operations wait for it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{Refusal, RefusalCode};
use kbf_job_client::{BadId, Broken, Error, Stuck, Undecodable, Unsent};
use tonic::{Code, Status};

use super::{Busy, MAX_WAIT, Seen, busy};

const POLL: Duration = Duration::from_millis(250);

fn status(code: Code) -> Error {
    Error::Status {
        code,
        message: "m".to_owned(),
    }
}

const fn no_jitter() -> f64 {
    0.0
}

const fn most_jitter() -> f64 {
    0.999
}

fn busy_answer() -> Result<(), Error> {
    Err(status(Code::ResourceExhausted))
}

/// Catches a busy job service read as an answer about the call, and a definite answer read
/// as a busy service: a lost or reset answer, a connection never made, and the codes of the
/// service's load are busy; a refusal, a status about the request or the caller, an answer that
/// cannot be decoded and a request that could never be sent are not.
#[test]
fn a_busy_service_is_told_from_an_answer_about_the_call() {
    let lost = Error::Transport(Broken::Deadline(Duration::from_secs(5)));
    let reset = Error::Transport(Broken::Status(Status::unknown("h2 protocol error")));
    let unconnected = Error::Unreachable(Unsent::Connect(Status::unavailable("refused")));
    for error in [lost, reset, unconnected] {
        assert!(busy(&error), "{error}");
    }
    for code in [
        Code::ResourceExhausted,
        Code::Unavailable,
        Code::DeadlineExceeded,
        Code::Aborted,
        Code::Cancelled,
        Code::Unknown,
        Code::Internal,
    ] {
        assert!(busy(&status(code)), "{code:?}");
    }
    for code in [
        Code::InvalidArgument,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::FailedPrecondition,
        Code::Unimplemented,
        Code::Unauthenticated,
        Code::OutOfRange,
        Code::DataLoss,
    ] {
        assert!(!busy(&status(code)), "{code:?}");
    }
    let refusal = Refusal {
        code: RefusalCode::NoCapableHost.into(),
        detail: "none".to_owned(),
    };
    for error in [
        Error::Refused(refusal),
        Error::Decode(Undecodable::Missing("job")),
        Error::Unreachable(Unsent::BadId(BadId::Length(0))),
        Error::Unreachable(Unsent::TooLarge { len: 2, cap: 1 }),
        Error::TooMany(3),
        Error::Stuck(Stuck::EmptyPages(1)),
    ] {
        assert!(!busy(&error), "{error}");
    }
}

/// Catches a wait that does not back off, or never comes back: the poll while the service
/// answers; one poll at the first busy answer, doubling at each busy answer one wait later, held
/// at [`MAX_WAIT`]; and the poll again at the first answer.
#[tokio::test(start_paused = true)]
async fn the_wait_doubles_to_its_cap_and_an_answer_ends_it() {
    let level = Busy::new(POLL, no_jitter);
    assert_eq!(level.pause(), POLL);
    assert_eq!(level.saw("a call", &Ok::<(), Error>(())), Seen::Answered);
    assert_eq!(level.pause(), POLL);
    let mut waits = Vec::new();
    for _ in 0..9 {
        assert!(level.saw("a call", &busy_answer()).busy());
        waits.push(level.pause());
        tokio::time::advance(level.pause()).await;
    }
    let ms = |ms| Duration::from_millis(ms);
    assert_eq!(
        waits,
        [
            ms(250),
            ms(500),
            ms(1000),
            ms(2000),
            ms(4000),
            ms(8000),
            MAX_WAIT,
            MAX_WAIT,
            MAX_WAIT
        ]
    );
    let definite = level.saw("a call", &Err::<(), _>(status(Code::NotFound)));
    assert_eq!(definite, Seen::AnsweredAgain);
    assert_eq!(level.pause(), POLL, "a definite answer is an answer");
}

/// Catches every operation's busy answer raising the shared level: a thousand at one moment
/// raise it once, and the next raise waits for half the current wait.
#[tokio::test(start_paused = true)]
async fn busy_answers_within_one_wait_raise_the_level_once() {
    let level = Busy::new(POLL, no_jitter);
    for _ in 0..1000 {
        level.saw("a call", &busy_answer());
    }
    assert_eq!(level.pause(), POLL);
    tokio::time::advance(POLL / 2 - Duration::from_millis(1)).await;
    level.saw("a call", &busy_answer());
    assert_eq!(level.pause(), POLL, "not half a wait yet");
    tokio::time::advance(Duration::from_millis(1)).await;
    level.saw("a call", &busy_answer());
    assert_eq!(level.pause(), POLL * 2);
}

/// Catches a service that answers some calls and not others taken for one that answers: an
/// answer within a wait of a busy answer leaves the level where it is, so busy answers in
/// between go on doubling the wait, and nothing turns; once a whole wait passes with no busy
/// answer, an answer ends it.
#[tokio::test(start_paused = true)]
async fn answers_between_busy_answers_do_not_end_the_level() {
    let level = Busy::new(POLL, no_jitter);
    let mut seen = Vec::new();
    let mut waits = Vec::new();
    for _ in 0..4 {
        seen.push(level.saw("reading the result of a job", &busy_answer()));
        tokio::time::advance(level.pause() / 2).await;
        seen.push(level.saw("reading a job", &Ok::<(), Error>(())));
        waits.push(level.pause());
        tokio::time::advance(level.pause() / 2).await;
    }
    let ms = |ms| Duration::from_millis(ms);
    assert_eq!(waits, [ms(250), ms(500), ms(1000), ms(2000)]);
    let mut expected = vec![Seen::TurnedBusy, Seen::Answered];
    for _ in 0..3 {
        expected.extend([Seen::StillBusy, Seen::Answered]);
    }
    assert_eq!(seen, expected);
    tokio::time::advance(level.pause()).await;
    assert_eq!(
        level.saw("reading a job", &Ok::<(), Error>(())),
        Seen::AnsweredAgain
    );
    assert_eq!(level.pause(), POLL);
}

/// Catches a jitter that lengthens a wait or cuts it to nothing: it takes off at most half.
#[tokio::test(start_paused = true)]
async fn a_wait_is_jittered_down_by_at_most_half() {
    let level = Busy::new(POLL, most_jitter);
    assert_eq!(level.pause(), POLL, "no jitter while the service answers");
    level.saw("a call", &busy_answer());
    let wait = level.pause();
    assert!(wait < POLL && wait > POLL / 2, "{wait:?}");
}

/// Catches a busy service logged at every failed call: the first busy answer turns the
/// service busy, the next ones are still busy, and the first answer a wait later is the one
/// that says it answers again; an answer after that is plain, and a busy answer after the turn
/// back turns it busy again.
#[tokio::test(start_paused = true)]
async fn a_busy_service_turns_once_and_answers_again_once() {
    let level = Busy::new(POLL, no_jitter);
    let mut seen = vec![level.saw("reading a job", &busy_answer())];
    for _ in 0..49 {
        seen.push(level.saw("reading a job", &busy_answer()));
    }
    tokio::time::advance(level.pause()).await;
    seen.push(level.saw("reading a job", &Ok::<(), Error>(())));
    seen.push(level.saw("reading a job", &Ok::<(), Error>(())));
    seen.push(level.saw("reading a job", &busy_answer()));
    let mut expected = vec![Seen::TurnedBusy];
    expected.extend([Seen::StillBusy; 49]);
    expected.extend([Seen::AnsweredAgain, Seen::Answered, Seen::TurnedBusy]);
    assert_eq!(seen, expected);
}

/// Catches a turn to busy logged without the call that saw it, or logged as a failure: the
/// WARN says which call failed, that calls wait and back off, and that no run ends for it, and
/// fifty busy answers log it once.
#[tokio::test(start_paused = true)]
async fn the_turn_to_busy_is_logged_once_at_warn() {
    let log = Log::default();
    let level = Busy::new(POLL, no_jitter);
    log.capture(|| {
        for _ in 0..50 {
            level.saw("reading a job", &busy_answer());
        }
    });
    let text = log.text();
    let warned: Vec<&str> = text.lines().filter(|line| line.contains("WARN")).collect();
    assert_eq!(warned.len(), 1, "{text}");
    let message = warned[0]
        .split_once("kbf_server::executor::busy: ")
        .map(|(_, m)| m);
    assert_eq!(
        message,
        Some(
            "the job service is busy or cannot be reached (reading a job failed): its calls \
             wait and are sent again, backing off up to 10s; no run ends for it \
             error=the job service answered ResourceExhausted: m"
        ),
        "{text}"
    );
}

/// What a test's log holds, as text.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Runs `f` with everything it logs written here.
    fn capture<T>(&self, f: impl FnOnce() -> T) -> T {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f)
    }
}
