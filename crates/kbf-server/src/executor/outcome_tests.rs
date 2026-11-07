use std::collections::BTreeSet;

use js_proto::v1::{
    Job, JobResult, JobState, Refusal, RefusalCode, ResultReason, StartFailureCode,
};
use kbf_job_client::{Broken, Error, Stuck, Undecodable, Unsent};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker::{Lost, MemoryKill};
use kbf_types::LeaseId;
use tonic::Code;

use super::{Settled, Verdict, forgotten, judge, ran, refused, result_of};
use crate::executor::test_support::Log;

fn job(state: JobState, failure: StartFailureCode, detail: &str) -> Job {
    Job {
        id: "k0123abcd-4-9-0".to_owned(),
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
            "succeeded (exit 0)",
            judged(
                ended(JobState::Succeeded),
                result(ResultReason::Exit, Some(0)),
            ),
        ),
        (
            "succeeded with a nonzero exit code",
            judged(
                ended(JobState::Succeeded),
                result(ResultReason::Exit, Some(3)),
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
            "failed to start host-unsafe",
            judged(refused_to_start("host-unsafe"), none.clone()),
        ),
        (
            "failed to start egress-unchecked",
            judged(refused_to_start("egress-unchecked"), none.clone()),
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

/// What the code answers for `case`, and whether it flags a twin.
fn answer(case: &Case) -> (Verdict, bool) {
    match case {
        Case::Judged(judged) => (judge(&judged.0, &judged.1), false),
        Case::Forgotten => (Verdict::Settled(forgotten()), false),
        Case::Refused(error) => {
            let decided = refused(error).expect("a refusal that decides");
            (Verdict::Settled(decided.settled), decided.twin)
        }
    }
}

/// The class word of the table for an answer.
fn class_of(verdict: &Verdict, twin: bool) -> &'static str {
    match (verdict, twin) {
        (_, true) => "twin",
        (Verdict::Ingest, _) => "ingest",
        (Verdict::Settled(Settled::Infra(_)), _) => "infra",
        (Verdict::Settled(Settled::Timeout), _) => "timeout",
        (Verdict::Settled(Settled::OutOfMemory), _) => "out-of-memory",
        (Verdict::Settled(Settled::Lost(Lost::Fenced)), _) => "fenced",
        (Verdict::Settled(Settled::Lost(Lost::DaemonRestarted)), _) => "restarted",
        (Verdict::Settled(Settled::Lost(other)), _) => panic!("a loss the table lacks: {other:?}"),
        (Verdict::Settled(Settled::Invalid(_)), _) => "invalid",
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

fn code_of(result: &kbf_proto::worker::Result) -> i32 {
    result.status.as_ref().expect("a status").code
}

/// Catches an ending of a job being mapped to a class other than the one the doc table at the
/// top of `outcome.rs` states, a row of the table with no case here, or a case with no row:
/// every row is built as a job, a refusal or a forgotten job, the code's answer is named by
/// its class, and each must equal the doc's. Each settled answer also becomes the status the
/// farm classifies by (and the `lost` and `memory_kill` fields it reads in place of one).
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
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    for (what, case) in &cases {
        let (verdict, twin) = answer(case);
        let class = class_of(&verdict, twin);
        let (_, doc_class) = rows
            .iter()
            .find(|(row, _)| row == what)
            .expect("a row of the case");
        assert_eq!(class, doc_class, "{what}");
        let Verdict::Settled(settled) = verdict else {
            continue;
        };
        let result = result_of(lease, &action, &settled);
        let (code, lost, memory_kill) = match class {
            "infra" | "twin" => (Code::Unavailable, 0, 0),
            "fenced" => (Code::Unavailable, Lost::Fenced as i32, 0),
            "restarted" => (Code::Unavailable, Lost::DaemonRestarted as i32, 0),
            "timeout" => (Code::DeadlineExceeded, 0, 0),
            "out-of-memory" => (Code::ResourceExhausted, 0, MemoryKill::OwnLimit as i32),
            "invalid" => (Code::InvalidArgument, 0, 0),
            other => panic!("{what}: {other}"),
        };
        assert_eq!(code_of(&result), code as i32, "{what}");
        assert_eq!(
            (result.lost, result.memory_kill),
            (lost, memory_kill),
            "{what}"
        );
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
        assert!(matches!(decided.settled, Settled::Infra(_)), "{error}");
        assert!(!decided.twin, "{error}");
    }
}

/// Catches a host's detail text reaching the farm or the log: an agent's detail of a start it
/// refused can quote the arguments of the job (the pack url and the read token). The farm is
/// told the fixed phrase of the class, in full, and the log holds the length of the detail
/// and never its text.
#[test]
fn a_start_failure_detail_never_reaches_the_result_or_the_log() {
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    let quoted = "--pack-url https://store/key?sig=MARKER-SECRET";
    let status = |code: Code, message: &str| {
        Some(rpc::Status {
            code: code as i32,
            message: message.to_owned(),
            details: Vec::new(),
        })
    };
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
        let result = result_of(lease, &action, &settled);
        assert_eq!(result.status, expected, "{code}");
    }
    let text = log.text();
    assert!(!text.contains("MARKER-SECRET"), "{text}");
    assert!(!text.contains("--pack-url"), "{text}");
    assert!(text.contains("detail_bytes"), "{text}");
    assert!(text.contains("k0123abcd-4-9-0"), "{text}");
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
            Verdict::Settled(Settled::Lost(Lost::Fenced)),
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
        &result(ResultReason::Exit, Some(3)),
    ));
    assert!(why.contains('3'), "{why}");
}

/// Catches a class that reaches the farm as another: the status code, and the two fields
/// the farm reads in place of it (`memory_kill`, `lost`), for each way a job is settled.
#[test]
fn a_settled_run_becomes_the_result_the_farm_classifies() {
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    let of = |settled| result_of(lease, &action, &settled);
    let infra = of(Settled::Infra("disk".to_owned()));
    assert_eq!(code_of(&infra), Code::Unavailable as i32);
    assert_eq!((infra.memory_kill, infra.lost), (0, 0));
    assert_eq!(infra.status.as_ref().expect("a status").message, "disk");
    assert_eq!(infra.lease_id.map(|l| (l.term, l.seq)), Some((4, 9)));
    assert_eq!(infra.action_digest.as_ref(), Some(&action));
    assert!(infra.action_result.is_none());

    let timeout = of(Settled::Timeout);
    assert_eq!(code_of(&timeout), Code::DeadlineExceeded as i32);

    let oom = of(Settled::OutOfMemory);
    assert_eq!(code_of(&oom), Code::ResourceExhausted as i32);
    assert_eq!(oom.memory_kill, MemoryKill::OwnLimit as i32);

    let lost = of(Settled::Lost(Lost::Fenced));
    assert_eq!(code_of(&lost), Code::Unavailable as i32);
    assert_eq!(lost.lost, Lost::Fenced as i32);
    assert_eq!(
        of(Settled::Lost(Lost::DaemonRestarted)).lost,
        Lost::DaemonRestarted as i32
    );

    let invalid = of(Settled::Invalid("no image".to_owned()));
    assert_eq!(code_of(&invalid), Code::InvalidArgument as i32);
}

/// Catches a result that ran with a status (it would not be OK) or without the action digest
/// the farm checks it against.
#[test]
fn a_result_that_ran_is_ok_and_names_its_action() {
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    let result = ran(
        lease,
        &action,
        ActionResult {
            exit_code: 2,
            ..ActionResult::default()
        },
    );
    assert!(result.status.is_none());
    assert_eq!(result.action_result.map(|r| r.exit_code), Some(2));
    assert_eq!(result.action_digest.as_ref(), Some(&action));
}
