use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use js_proto::v1::{
    Job, JobResult, JobState, Refusal, RefusalCode, ResultReason, StartFailureCode,
};
use kbf_front::ERROR_DOMAIN;
use kbf_job_client::{Broken, Error, Stuck, Undecodable, Unsent};
use kbf_proto::google::rpc::ErrorInfo;
use prost::Message;
use tonic::Code;

use super::{OUT_OF_MEMORY_REASON, RESUBMITS, Settled, Verdict, forgotten, judge, refused};
use crate::executor::LostObject;

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

fn job(state: JobState, failure: StartFailureCode, detail: &str) -> Job {
    Job {
        id: "k0123abcd-4-9".to_owned(),
        state: state.into(),
        start_failure: failure.into(),
        start_failure_detail: detail.to_owned(),
        ..Job::default()
    }
}

fn ended(state: JobState) -> Job {
    job(state, StartFailureCode::Unspecified, "")
}

fn refused_to_start(detail: &str) -> Job {
    job(
        JobState::FailedToStart,
        StartFailureCode::AgentRefused,
        detail,
    )
}

fn failed_to_start(code: StartFailureCode) -> Job {
    job(JobState::FailedToStart, code, "")
}

fn result(reason: ResultReason, exit_code: Option<i32>) -> JobResult {
    JobResult {
        reason: reason.into(),
        exit_code,
        ..JobResult::default()
    }
}

/// The result of a job that succeeded, as the job service reports one: its state is
/// succeeded too.
fn succeeded(reason: ResultReason, exit_code: Option<i32>) -> JobResult {
    JobResult {
        state: JobState::Succeeded.into(),
        ..result(reason, exit_code)
    }
}

fn refusal(code: RefusalCode) -> Error {
    Error::Refused(Refusal {
        code: code.into(),
        detail: "why".to_owned(),
    })
}

fn infra(verdict: Verdict) -> String {
    match verdict {
        Verdict::Settled(Settled::Infra(why)) => why,
        other => panic!("not infra: {other:?}"),
    }
}

/// How a case is made to end.
enum Case {
    /// A job that ended, with its result (boxed: a job is far larger than the other cases).
    Judged(Box<(Job, JobResult)>),
    /// A submit that failed.
    Refused(Error),
    /// A job the service forgot.
    Forgotten,
}

/// One case for each row of the table at the top of `outcome.rs`, by the row's first column.
fn cases() -> Vec<(&'static str, Case)> {
    use Case::{Forgotten, Refused};
    fn judged(job: Job, result: JobResult) -> Case {
        Case::Judged(Box::new((job, result)))
    }
    let none = JobResult::default();
    vec![
        (
            "succeeded, its result succeeded, reason exit, exit code 0",
            judged(
                ended(JobState::Succeeded),
                succeeded(ResultReason::Exit, Some(0)),
            ),
        ),
        (
            "succeeded with a nonzero exit code",
            judged(
                ended(JobState::Succeeded),
                succeeded(ResultReason::Exit, Some(3)),
            ),
        ),
        (
            "succeeded with no exit code",
            judged(
                ended(JobState::Succeeded),
                succeeded(ResultReason::Exit, None),
            ),
        ),
        (
            "succeeded for a reason other than exit",
            judged(
                ended(JobState::Succeeded),
                succeeded(ResultReason::Unspecified, Some(0)),
            ),
        ),
        (
            "succeeded, its result in another state",
            judged(
                ended(JobState::Succeeded),
                JobResult {
                    state: JobState::Failed.into(),
                    ..result(ResultReason::Exit, Some(0))
                },
            ),
        ),
        (
            "failed, reason wall limit",
            judged(
                ended(JobState::Failed),
                result(ResultReason::WallLimit, None),
            ),
        ),
        (
            "failed, reason memory",
            judged(ended(JobState::Failed), result(ResultReason::Memory, None)),
        ),
        (
            "failed, reason exit",
            judged(
                ended(JobState::Failed),
                result(ResultReason::Exit, Some(12)),
            ),
        ),
        (
            "failed, any other reason",
            judged(
                ended(JobState::Failed),
                result(ResultReason::Unspecified, None),
            ),
        ),
        (
            "cancelled by anyone else",
            judged(ended(JobState::Cancelled), none.clone()),
        ),
        ("lost", judged(ended(JobState::Lost), none.clone())),
        ("not found after it was seen", Forgotten),
        (
            "failed to start quota-unavailable",
            judged(refused_to_start("quota-unavailable"), none.clone()),
        ),
        (
            "failed to start files-unsupported",
            judged(refused_to_start("files-unsupported"), none.clone()),
        ),
        (
            "failed to start invalid-file",
            judged(refused_to_start("invalid-file"), none.clone()),
        ),
        (
            "failed to start no-capacity",
            judged(failed_to_start(StartFailureCode::NoCapacity), none.clone()),
        ),
        (
            "failed to start start-timeout",
            judged(
                failed_to_start(StartFailureCode::StartTimeout),
                none.clone(),
            ),
        ),
        (
            "failed to start exclusive-not-ready",
            judged(
                failed_to_start(StartFailureCode::ExclusiveNotReady),
                none.clone(),
            ),
        ),
        (
            "failed to start input-unavailable",
            judged(
                failed_to_start(StartFailureCode::InputUnavailable),
                none.clone(),
            ),
        ),
        (
            "failed to start host-unsafe",
            judged(refused_to_start("host-unsafe"), none.clone()),
        ),
        (
            "failed to start egress-unchecked",
            judged(refused_to_start("egress-unchecked"), none.clone()),
        ),
        (
            "failed to start, any code, its detail naming an object of its tree the store answered 404 for",
            judged(
                job(
                    JobState::FailedToStart,
                    StartFailureCode::InputUnavailable,
                    &format!("cannot fetch manifest {LOST_HEX}: upstream 404"),
                ),
                none.clone(),
            ),
        ),
        (
            "failed to start, any other code or detail",
            judged(refused_to_start("scratch-failed"), none.clone()),
        ),
        (
            "in a state this server does not know",
            judged(ended(JobState::Unspecified), none),
        ),
        (
            "refused at submit no-capable-host",
            Refused(refusal(RefusalCode::NoCapableHost)),
        ),
        (
            "refused at submit wall-time-not-positive",
            Refused(refusal(RefusalCode::WallTimeNotPositive)),
        ),
        (
            "refused at submit wall-time-above-ceiling",
            Refused(refusal(RefusalCode::WallTimeAboveCeiling)),
        ),
        (
            "refused at submit job-id-conflict",
            Refused(refusal(RefusalCode::JobIdConflict)),
        ),
        (
            "refused at submit as written (`INVALID_ARGUMENT`)",
            Refused(Error::Status {
                code: Code::InvalidArgument,
                message: "bad image".to_owned(),
            }),
        ),
        (
            "refused at submit with a code this server does not know",
            Refused(refusal(RefusalCode::Unspecified)),
        ),
        (
            "refused at submit otherwise",
            Refused(Error::Status {
                code: Code::PermissionDenied,
                message: "no".to_owned(),
            }),
        ),
    ]
}

/// What the code answers for `case`.
fn answer(case: &Case) -> Verdict {
    match case {
        Case::Judged(judged) => judge(&judged.0, &judged.1),
        Case::Forgotten => Verdict::Settled(forgotten()),
        Case::Refused(error) => Verdict::Settled(refused(error).expect("a refusal that decides")),
    }
}

/// The class word of the table for an answer.
fn class_of(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Ingest => "ingest",
        Verdict::Settled(settled) | Verdict::StoreLost { settled, .. } => match settled {
            Settled::Infra(_) => "infra",
            Settled::Timeout => "timeout",
            Settled::OutOfMemory => "out-of-memory",
            Settled::Lost(_) => "lost",
            Settled::Invalid(_) => "invalid",
            Settled::Missing(_) => "missing",
        },
    }
}

/// The rows of the table at the top of `outcome.rs`: the first column and the class.
fn doc_rows() -> Vec<(String, String)> {
    include_str!("outcome.rs")
        .lines()
        .filter(|line| line.starts_with("//! |"))
        .skip(2)
        .map(|line| {
            let cells: Vec<&str> = line
                .trim_start_matches("//! ")
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            (cells[0].to_owned(), cells[1].trim_matches('`').to_owned())
        })
        .collect()
}

/// Catches an ending of a job being mapped to a class other than the one the doc table at the
/// top of `outcome.rs` states, a row of the table with no case here, or a case with no row:
/// every row is built as a job, a refusal or a forgotten job, the code's answer is named by
/// its class, and each must equal the doc's. Each settled answer also becomes the status the
/// callers are answered with, and is run again exactly when its class says so.
#[test]
fn each_job_end_maps_to_one_failure_class() {
    let rows = doc_rows();
    let cases = cases();
    let documented: BTreeSet<&str> = rows.iter().map(|(what, _)| what.as_str()).collect();
    let covered: BTreeSet<&str> = cases.iter().map(|(what, _)| *what).collect();
    assert_eq!(documented.len(), rows.len(), "a row is documented twice");
    assert_eq!(covered.len(), cases.len(), "a case is written twice");
    assert_eq!(
        documented.difference(&covered).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "rows with no case"
    );
    assert_eq!(
        covered.difference(&documented).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "cases with no row"
    );
    for (what, case) in &cases {
        let verdict = answer(case);
        let class = class_of(&verdict);
        let (_, doc_class) = rows
            .iter()
            .find(|(row, _)| row == what)
            .expect("a row of the case");
        assert_eq!(class, doc_class, "{what}");
        let (Verdict::Settled(settled) | Verdict::StoreLost { settled, .. }) = verdict else {
            continue;
        };
        let (code, retried) = match class {
            "infra" | "lost" => (Code::Unavailable, true),
            "timeout" => (Code::DeadlineExceeded, false),
            "out-of-memory" => (Code::FailedPrecondition, true),
            "invalid" => (Code::InvalidArgument, false),
            other => panic!("{what}: {other}"),
        };
        assert_eq!(settled.status().code, code as i32, "{what}");
        assert_eq!(settled.retried(), retried, "{what}");
    }
}

/// Catches the answers a submit has when nothing is decided being settled: a request that was
/// never sent, and one whose answer was lost, are sent again (`None`), whatever else the
/// service might have done.
#[test]
fn a_submit_whose_answer_is_not_known_decides_nothing() {
    let unsent = Error::Unreachable(Unsent::TooLarge { len: 2, cap: 1 });
    let lost = Error::Transport(Broken::Deadline(std::time::Duration::from_secs(1)));
    let undecodable = Error::Decode(Undecodable::Missing("job"));
    for error in [unsent, lost, undecodable] {
        assert_eq!(refused(&error), None, "{error}");
    }
    let listing = [Error::TooMany(3), Error::Stuck(Stuck::EmptyPages(1))];
    for error in listing {
        let decided = refused(&error).expect("a refusal that decides");
        assert!(matches!(decided, Settled::Infra(_)), "{error}");
    }
}

/// Catches a host's detail text reaching the answer or the log: an agent's detail of a start it
/// refused can quote the arguments of the job (the pack url and the read token). The callers
/// are told the fixed phrase of the class, in full, and the log holds the length of the detail
/// and never its text.
#[test]
fn a_start_failure_detail_never_reaches_the_answer_or_the_log() {
    let quoted = "--pack-url https://store/key?sig=MARKER-SECRET";
    let status = |code: Code, message: &str| (code as i32, message.to_owned());
    let log = Log::default();
    for (code, expected) in [
        (
            "invalid-file",
            status(Code::InvalidArgument, "the host refused a file of the job"),
        ),
        (
            "files-unsupported",
            status(
                Code::Unavailable,
                "the host cannot write the files of a job",
            ),
        ),
        (
            "exec-failed",
            status(Code::Unavailable, "the host refused to start the job"),
        ),
    ] {
        let detail = format!("{code}: {quoted}");
        let verdict = log.capture(|| judge(&refused_to_start(&detail), &JobResult::default()));
        let Verdict::Settled(settled) = verdict else {
            panic!("{code}: not settled");
        };
        let answered = settled.status();
        assert_eq!((answered.code, answered.message), expected, "{code}");
    }
    let text = log.text();
    assert!(!text.contains("MARKER-SECRET"), "{text}");
    assert!(!text.contains("--pack-url"), "{text}");
    assert!(text.contains("detail_bytes"), "{text}");
    assert!(text.contains("k0123abcd-4-9"), "{text}");
}

/// Catches an agent's detail being read as more than its leading code: `quota-unavailable-ish`
/// is not `quota-unavailable` (the host's fault, run elsewhere) and a code that is not at the
/// start, or in other case, is not read at all; the same code followed by text is the code.
#[test]
fn an_agent_detail_is_read_by_its_leading_code_only() {
    let judged = |detail: &str| judge(&refused_to_start(detail), &JobResult::default());
    for same in ["quota-unavailable", "quota-unavailable: scratch said no"] {
        assert_eq!(
            judged(same),
            Verdict::Settled(Settled::Lost("the host's scratch quota is out of order")),
            "{same}"
        );
    }
    for other in [
        "quota-unavailable-ish",
        "Quota-unavailable",
        " quota-unavailable",
        "the quota-unavailable",
        "files-unsupported-ish",
        "host-unsafe-ish",
        "egress-unchecked-ish",
        "quota-unavailable2",
        "",
    ] {
        assert!(
            matches!(judged(other), Verdict::Settled(Settled::Infra(_))),
            "{other:?}"
        );
    }
    assert!(matches!(
        judged("invalid-file-ish"),
        Verdict::Settled(Settled::Infra(_))
    ));
    assert_eq!(
        infra(judged("host-unsafe: tainted")),
        "the job failed to start: host-unsafe"
    );
    assert_eq!(
        infra(judged("egress-unchecked: no nft table")),
        "the host refused to start the job: its agent could not check the limits of the \
         job's network (egress-unchecked)"
    );
}

/// Catches the infra reason leaving out the exit code the job service reported, which is what an
/// operator has to go on in the answer.
#[test]
fn an_infra_reason_carries_the_exit_code() {
    let why = infra(judge(
        &ended(JobState::Failed),
        &result(ResultReason::Exit, Some(13)),
    ));
    assert!(why.contains("Some(13)"), "{why}");
    let why = infra(judge(
        &ended(JobState::Succeeded),
        &succeeded(ResultReason::Exit, Some(3)),
    ));
    assert!(why.contains('3'), "{why}");
}

/// Catches a pack being promoted from anything but a clean result. Every combination of the
/// job's state, the result's state, the result's reason and its exit code is judged, each
/// enum over all its values and one value no release has (99), the exit code absent, 0, 1,
/// -1, `i32::MIN` and `i32::MAX`: exactly one combination is ingested, the job and its result
/// succeeded for the job's exit with exit code 0.
#[test]
fn only_a_clean_result_is_ingested() {
    let states: Vec<i32> = (0..=9).chain([99]).collect();
    let reasons: Vec<i32> = (0..=4).chain([99]).collect();
    let exits = [
        None,
        Some(0),
        Some(1),
        Some(-1),
        Some(i32::MIN),
        Some(i32::MAX),
    ];
    let (succeeded, exit) = (JobState::Succeeded as i32, ResultReason::Exit as i32);
    let mut promoted = Vec::new();
    for &job_state in &states {
        let job = Job {
            state: job_state,
            ..ended(JobState::Unspecified)
        };
        for &result_state in &states {
            for &reason in &reasons {
                for exit_code in exits {
                    let result = JobResult {
                        state: result_state,
                        reason,
                        exit_code,
                        ..JobResult::default()
                    };
                    if judge(&job, &result) == Verdict::Ingest {
                        promoted.push((job_state, result_state, reason, exit_code));
                    }
                }
            }
        }
    }
    assert_eq!(promoted, vec![(succeeded, succeeded, exit, Some(0))]);
}

/// Catches a job that succeeded with a result that is not clean being answered with a message
/// that does not say which part of the result was wrong: each is the whole message, as the
/// caller and the operator read it.
#[test]
fn a_result_that_is_not_clean_is_named_in_full() {
    let judged = |result: JobResult| infra(judge(&ended(JobState::Succeeded), &result));
    assert_eq!(
        judged(succeeded(ResultReason::Exit, Some(-1))),
        "the job succeeded with exit code -1"
    );
    assert_eq!(
        judged(succeeded(ResultReason::Exit, None)),
        "the job succeeded with no exit code"
    );
    for (reason, name) in [
        (ResultReason::Unspecified, "RESULT_REASON_UNSPECIFIED"),
        (ResultReason::WallLimit, "RESULT_REASON_WALL_LIMIT"),
        (ResultReason::Memory, "RESULT_REASON_MEMORY"),
        (ResultReason::Cancelled, "RESULT_REASON_CANCELLED"),
    ] {
        assert_eq!(
            judged(succeeded(reason, Some(0))),
            format!("the job succeeded for a reason other than its exit ({name})")
        );
    }
    for (state, name) in [
        (JobState::Unspecified, "JOB_STATE_UNSPECIFIED"),
        (JobState::Running, "JOB_STATE_RUNNING"),
        (JobState::Failed, "JOB_STATE_FAILED"),
        (JobState::Cancelled, "JOB_STATE_CANCELLED"),
    ] {
        let result = JobResult {
            state: state.into(),
            ..result(ResultReason::Exit, Some(0))
        };
        assert_eq!(
            judged(result),
            format!("the job succeeded but its result says {name}")
        );
    }
}

/// Catches a class answered as another: the status code and message of each way a run is
/// settled, whether it is run again, and, for the memory kill at the last attempt, the
/// `ErrorInfo` detail a client tells it by.
#[test]
fn a_settled_run_becomes_the_status_the_callers_are_answered_with() {
    let infra = Settled::Infra("disk".to_owned());
    assert_eq!(infra.status().code, Code::Unavailable as i32);
    assert_eq!(infra.status().message, "disk");
    assert!(infra.status().details.is_empty());

    let lost = Settled::Lost("the job was lost with its host");
    assert_eq!(lost.status().code, Code::Unavailable as i32);
    assert_eq!(lost.status().message, "the job was lost with its host");

    assert_eq!(
        Settled::Timeout.status().code,
        Code::DeadlineExceeded as i32
    );

    let invalid = Settled::Invalid("no image".to_owned());
    assert_eq!(invalid.status().code, Code::InvalidArgument as i32);
    assert_eq!(invalid.status().message, "no image");

    let oom = Settled::OutOfMemory.status();
    assert_eq!(oom.code, Code::FailedPrecondition as i32);
    let [detail] = oom.details.as_slice() else {
        panic!("one detail: {:?}", oom.details);
    };
    let info = ErrorInfo::decode(detail.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(info.reason, OUT_OF_MEMORY_REASON);
    assert_eq!(info.domain, ERROR_DOMAIN);
}

/// Catches an ending run again that should be answered at once, or the reverse: the farm's
/// endings (infrastructure, lost, killed at the memory limit) are submitted again, the action's
/// own timeout and an action that cannot run here are answered, and an operation has two more
/// attempts than its first.
#[test]
fn only_an_ending_of_the_farms_is_run_again() {
    assert!(Settled::Infra(String::new()).retried());
    assert!(Settled::Lost("gone").retried());
    assert!(Settled::OutOfMemory.retried());
    assert!(!Settled::Timeout.retried());
    assert!(!Settled::Invalid(String::new()).retried());
    assert_eq!(RESUBMITS, 2);
}

/// The SHA-256 of an object the store lost, as tierstore writes it.
const LOST_HEX: &str = "9e1bf90c91fe575d7dacd35d1b21d058c2f1fd7abd0d23ffa41c4a0da2facb7f";

/// [`LOST_HEX`] as bytes.
fn lost_sha() -> [u8; 32] {
    let mut sha = [0_u8; 32];
    for (i, byte) in sha.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&LOST_HEX[2 * i..2 * i + 2], 16).expect("hex");
    }
    sha
}

/// Catches a start that failed for an object the store lost being judged by its code alone:
/// whatever the code, a detail that names the object's SHA-256 before `: upstream 404` is
/// `infra` (run again) with the object named for the frontend, and the callers are told a fixed
/// phrase.
#[test]
fn a_start_that_failed_on_a_lost_object_is_infra_whatever_its_code() {
    let detail = format!("cannot fetch manifest {LOST_HEX}: upstream 404");
    for code in [
        StartFailureCode::NoCapacity,
        StartFailureCode::StartTimeout,
        StartFailureCode::ExclusiveNotReady,
        StartFailureCode::InputUnavailable,
        StartFailureCode::AgentRefused,
        StartFailureCode::Unspecified,
    ] {
        let verdict = judge(
            &job(JobState::FailedToStart, code, &detail),
            &JobResult::default(),
        );
        let Verdict::StoreLost { object, settled } = verdict else {
            panic!("{code:?}: not a lost object: {verdict:?}");
        };
        assert_eq!(object, LostObject::Manifest(lost_sha()), "{code:?}");
        assert!(settled.retried(), "{code:?}");
        let status = settled.status();
        assert_eq!(status.code, Code::Unavailable as i32, "{code:?}");
        assert_eq!(
            status.message,
            "the job failed to start: the store does not have an object of its tree"
        );
    }
}

/// Catches a detail read for more than the object's digest, or for a digest that is not one:
/// only 64 lower-case hex digits right before `: upstream 404` count, alone (not the end of a
/// longer run of hex digits) and before the status 404 itself (not a longer number); a tree list
/// named by its key counts; a first mention that does not qualify is passed over for a later
/// one; and anything else is no lost object.
#[test]
fn only_a_whole_sha256_right_before_upstream_404_names_a_lost_object() {
    use super::{hex, lost_object};
    let found = |detail: &str| lost_object(detail).map(|sha| hex(&sha));
    let named = Some(LOST_HEX.to_owned());
    assert_eq!(
        found(&format!("cannot fetch manifest {LOST_HEX}: upstream 404")),
        named
    );
    assert_eq!(found(&format!("cas/l/{LOST_HEX}: upstream 404")), named);
    assert_eq!(found(&format!("{LOST_HEX}: upstream 404")), named);
    assert_eq!(
        found(&format!(
            "x: upstream 404; manifest {LOST_HEX}: upstream 404 (twice)"
        )),
        named
    );
    let upper = LOST_HEX.to_uppercase();
    let short = &LOST_HEX[1..];
    for detail in [
        String::new(),
        "upstream 404".to_owned(),
        ": upstream 404".to_owned(),
        format!("cannot fetch manifest {upper}: upstream 404"),
        format!("cannot fetch manifest {short}: upstream 404"),
        format!("cannot fetch manifest 9g{}: upstream 404", &LOST_HEX[2..]),
        format!("cannot fetch manifest a{LOST_HEX}: upstream 404"),
        format!("cannot fetch manifest {LOST_HEX}: upstream 503"),
        format!("cannot fetch manifest {LOST_HEX}: upstream 4040"),
        format!("cannot fetch manifest {LOST_HEX} upstream 404"),
        format!("é{}: upstream 404", &LOST_HEX[1..]),
        "quota-unavailable".to_owned(),
    ] {
        assert_eq!(found(&detail), None, "{detail}");
    }
}

/// Catches a missing source of a job's tree (tierstore's `404 source missing: <key>`) judged by
/// the start's code alone: whatever the code, it is `infra` (run again) with the key named for
/// the frontend, the fixed phrase for the callers, and a manifest's SHA-256 in the same detail
/// taking precedence.
#[test]
fn a_start_that_failed_on_a_missing_source_is_infra_naming_its_key() {
    let key = "cas/f/fe1/boot1/00000000000000a3";
    let detail = format!("tree create failed: 404 source missing: {key} (path /bin/cc)");
    for code in [
        StartFailureCode::InputUnavailable,
        StartFailureCode::Unspecified,
    ] {
        let verdict = judge(
            &job(JobState::FailedToStart, code, &detail),
            &JobResult::default(),
        );
        let Verdict::StoreLost { object, settled } = verdict else {
            panic!("{code:?}: not a lost object: {verdict:?}");
        };
        assert_eq!(object, LostObject::Source(key.to_owned()), "{code:?}");
        assert!(settled.retried(), "{code:?}");
        assert_eq!(
            settled.status().message,
            "the job failed to start: the store does not have an object of its tree"
        );
    }
    let both = format!("source missing: {key}; cannot fetch manifest {LOST_HEX}: upstream 404");
    assert_eq!(
        super::lost_of(&both),
        Some(LostObject::Manifest(lost_sha()))
    );
}

/// Catches a detail read for more than a missing source's key, or for a key that is none of
/// kbf's: only the run of key characters right after `source missing: ` counts, when it starts
/// `cas/`, names something under it and is at most 1024 bytes; it ends at the first other
/// character; a first mention that does not qualify is passed over for a later one; and anything
/// else is no source.
#[test]
fn only_a_cas_key_right_after_source_missing_names_a_missing_source() {
    use super::source_missing;
    let key = "cas/b/build-1/n0nce_x.y";
    assert_eq!(
        source_missing(&format!("source missing: {key}")),
        Some(key.to_owned())
    );
    assert_eq!(
        source_missing(&format!("404 source missing: {key} at /bin/cc")),
        Some(key.to_owned())
    );
    assert_eq!(
        source_missing(&format!("source missing: {key}?sig=MARKER")),
        Some(key.to_owned())
    );
    assert_eq!(
        source_missing(&format!("source missing: /tmp/x; source missing: {key}")),
        Some(key.to_owned())
    );
    let longest = format!("cas/{}", "a".repeat(1020));
    assert_eq!(
        source_missing(&format!("source missing: {longest}")),
        Some(longest.clone())
    );
    for detail in [
        String::new(),
        "source missing".to_owned(),
        "source missing: ".to_owned(),
        "source missing: cas/".to_owned(),
        "source missing: cas".to_owned(),
        "source missing: other/b/x".to_owned(),
        "source missing:cas/b/x".to_owned(),
        "Source missing: cas/b/x".to_owned(),
        format!("source missing: {longest}a"),
        "source missing: é/cas/b/x".to_owned(),
        "upstream 404".to_owned(),
    ] {
        assert_eq!(source_missing(&detail), None, "{detail}");
    }
}

/// Catches a host's detail text reaching the log through a missing source: the log holds the
/// key and the detail's length, never the rest of the detail.
#[test]
fn a_missing_source_reaches_the_log_as_its_key_only() {
    let detail = "--pack-url https://store/key?sig=MARKER-SECRET source missing: cas/b/k1/n1";
    let log = Log::default();
    let verdict = log.capture(|| judge(&refused_to_start(detail), &JobResult::default()));
    assert!(matches!(verdict, Verdict::StoreLost { .. }), "{verdict:?}");
    let text = log.text();
    assert!(text.contains("cas/b/k1/n1"), "{text}");
    assert!(!text.contains("MARKER-SECRET"), "{text}");
}

/// Catches a host's detail text reaching the log or the answer through a lost object: the log
/// holds the object's digest and the detail's length, never the rest of the detail, and the
/// answer is the fixed phrase.
#[test]
fn a_lost_object_reaches_the_log_as_its_digest_only() {
    let quoted = "--pack-url https://store/key?sig=MARKER-SECRET";
    let detail = format!("{quoted} cannot fetch manifest {LOST_HEX}: upstream 404");
    let log = Log::default();
    let verdict = log.capture(|| judge(&refused_to_start(&detail), &JobResult::default()));
    let Verdict::StoreLost { settled, .. } = verdict else {
        panic!("not a lost object: {verdict:?}");
    };
    let text = log.text();
    assert!(text.contains(LOST_HEX), "{text}");
    assert!(
        text.contains(&format!("detail_bytes={}", detail.len())),
        "{text}"
    );
    assert!(!text.contains("MARKER-SECRET"), "{text}");
    assert!(!settled.status().message.contains("MARKER-SECRET"));
}

/// Catches inputs the store lost answered as anything but REAPI's missing inputs: the callers get
/// FAILED_PRECONDITION with one `MISSING` violation per blob, which is what makes a client upload
/// them again, and the run is not run again (the client's upload comes first).
#[test]
fn inputs_the_store_lost_are_answered_missing_and_not_run_again() {
    use kbf_proto::google::rpc::PreconditionFailure;
    let one = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 5);
    let two = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [2; 32], 7);
    let settled = Settled::Missing(vec![one, two]);

    let status = settled.status();

    assert!(!settled.retried());
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert_eq!(status.details.len(), 1);
    let failure = PreconditionFailure::decode(status.details[0].value.as_slice()).expect("decodes");
    let subjects: Vec<&str> = failure
        .violations
        .iter()
        .map(|v| v.subject.as_str())
        .collect();
    assert_eq!(
        subjects,
        [
            format!("blobs/{}/5", one.hash_hex()),
            format!("blobs/{}/7", two.hash_hex())
        ]
    );
    assert!(failure.violations.iter().all(|v| v.r#type == "MISSING"));
}
