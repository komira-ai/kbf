use std::collections::BTreeMap;

use js_proto::v1::{GetLogsResponse, JobResult, Refusal};
use kbf_job_client::{Broken, Error, Stuck, Undecodable, Unsent};
use kbf_proto::google::rpc::{self, ErrorInfo};
use kbf_proto::worker;
use prost::Message;
use std::time::Duration;
use tonic::Code;

use super::{OUTPUT_CAP, OUTPUT_REASON, Output, Tail, ending, shares};

const SERVICE_TEXT: &str = "held by the service";

/// Catches output over the cap being attached whole, its head kept instead of its end, a cut
/// inside a character, and the service's own cut being forgotten.
#[test]
fn output_over_the_cap_keeps_its_end_on_a_character_boundary() {
    // 6001 bytes: the last 4096 would start inside an "é", so 4095 are kept.
    let stdout = format!("{}x", "é".repeat(3000));
    let stderr = "e".repeat(6000);
    let tail = Tail {
        stdout: stdout.clone().into_bytes(),
        stderr: stderr.into_bytes(),
        truncated: false,
    };
    let output = Output::of(&tail);
    assert_eq!(output.stdout, format!("{}x", "é".repeat(2047)));
    assert_eq!(output.stderr, "e".repeat(4096));
    assert!(output.stdout.len() + output.stderr.len() <= OUTPUT_CAP);
    assert!(stdout.ends_with(&output.stdout));
    assert!(output.truncated);

    let small = Tail {
        stdout: b"out".to_vec(),
        stderr: b"err".to_vec(),
        truncated: false,
    };
    let output = Output::of(&small);
    assert_eq!(
        (output.stdout.as_str(), output.stderr.as_str()),
        ("out", "err")
    );
    assert!(!output.truncated);
    let cut = Tail {
        truncated: true,
        ..small
    };
    assert!(Output::of(&cut).truncated);
}

/// Catches bytes that are not UTF-8 making the output unreadable or dropped.
#[test]
fn bytes_that_are_not_utf8_are_replaced() {
    let tail = Tail {
        stdout: b"a\xffb".to_vec(),
        stderr: Vec::new(),
        truncated: false,
    };
    assert_eq!(Output::of(&tail).stdout, "a\u{fffd}b");
}

/// Catches one stream taking more than its half while the other needs it, and a short
/// stream's unused share being wasted.
#[test]
fn the_cap_is_shared_and_what_one_stream_leaves_goes_to_the_other() {
    assert_eq!(shares(10, 10, 100), (10, 10));
    assert_eq!(shares(10, 500, 100), (10, 90));
    assert_eq!(shares(500, 10, 100), (90, 10));
    assert_eq!(shares(500, 500, 100), (50, 50));
    assert_eq!(shares(500, 500, 101), (50, 51));
    assert_eq!(shares(50, 500, 100), (50, 50));
}

#[test]
fn ending_keeps_the_last_bytes_on_a_boundary() {
    assert_eq!(ending("abc", 5), ("abc", false));
    assert_eq!(ending("abc", 2), ("bc", true));
    assert_eq!(ending("aé", 1), ("", true));
    assert_eq!(ending("", 0), ("", false));
}

/// Catches an empty result being taken as the job's output (the fallback must run), and the
/// two answers' tails being read from the wrong fields.
#[test]
fn a_result_without_output_has_no_tail_and_the_logs_answer_one() {
    assert_eq!(Tail::of_result(&JobResult::default()), None);
    let result = JobResult {
        stderr_tail: b"err".to_vec(),
        truncated: true,
        ..JobResult::default()
    };
    let expected = Tail {
        stdout: Vec::new(),
        stderr: b"err".to_vec(),
        truncated: true,
    };
    assert_eq!(Tail::of_result(&result), Some(expected));
    let logs = GetLogsResponse {
        stdout_tail: b"out".to_vec(),
        stderr_tail: b"err".to_vec(),
        truncated: true,
        ..GetLogsResponse::default()
    };
    let expected = Tail {
        stdout: b"out".to_vec(),
        stderr: b"err".to_vec(),
        truncated: true,
    };
    assert_eq!(Tail::of_logs(logs), expected);
    assert_eq!(
        format!("{expected:?}"),
        "Tail { stdout_bytes: 3, stderr_bytes: 3, truncated: true }",
        "a tail never prints its text"
    );
}

fn failed() -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: Code::Unavailable as i32,
            message: "why".to_owned(),
            details: Vec::new(),
        }),
        ..worker::Result::default()
    }
}

fn fields(stdout: &str, stderr: &str, truncated: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("stdout".to_owned(), stdout.to_owned()),
        ("stderr".to_owned(), stderr.to_owned()),
        ("truncated".to_owned(), truncated.to_owned()),
    ])
}

fn metadata(result: &worker::Result) -> BTreeMap<String, String> {
    let status = result.status.as_ref().expect("a status");
    assert_eq!(status.details.len(), 1);
    let detail = &status.details[0];
    assert_eq!(detail.type_url, "type.googleapis.com/google.rpc.ErrorInfo");
    let info = ErrorInfo::decode(detail.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(info.reason, OUTPUT_REASON);
    assert_eq!(info.domain, "kbf");
    info.metadata.into_iter().collect()
}

/// Catches the output replacing the failure's code or message, and the fields being written
/// under other names.
#[test]
fn the_output_is_a_detail_beside_the_reason() {
    let mut result = failed();
    let output = Output {
        stdout: "out".to_owned(),
        stderr: "err".to_owned(),
        truncated: true,
        note: None,
    };
    output.attach(&mut result);
    let status = result.status.as_ref().expect("a status");
    assert_eq!(
        (status.code, status.message.as_str()),
        (Code::Unavailable as i32, "why")
    );
    assert_eq!(metadata(&result), fields("out", "err", "true"));
}

/// Catches an empty output adding a detail, and one attached to a result with no status
/// making one up.
#[test]
fn nothing_is_attached_when_there_is_nothing_to_say() {
    let mut result = failed();
    Output::default().attach(&mut result);
    assert_eq!(result, failed());
    let mut only_stderr = failed();
    Output {
        stderr: "err".to_owned(),
        ..Output::default()
    }
    .attach(&mut only_stderr);
    assert_eq!(metadata(&only_stderr), fields("", "err", "false"));
    let mut ok = worker::Result::default();
    Output {
        stdout: "out".to_owned(),
        ..Output::default()
    }
    .attach(&mut ok);
    assert_eq!(ok, worker::Result::default());
}

/// Catches a failed read replacing the cause, and the service's own message (which this
/// executor does not control) reaching the note: the note names the kind of failure only.
#[test]
fn a_failed_read_is_a_short_note_that_never_quotes_the_service() {
    let status = Error::Status {
        code: Code::Unavailable,
        message: SERVICE_TEXT.to_owned(),
    };
    let cases = [
        (status, "the job service answered Unavailable"),
        (
            Error::Unreachable(Unsent::TooLarge { len: 2, cap: 1 }),
            "the call was not sent",
        ),
        (
            Error::Transport(Broken::Deadline(Duration::from_secs(1))),
            "the call was not answered",
        ),
        (
            Error::Decode(Undecodable::Missing("result")),
            "the answer could not be decoded",
        ),
        (
            Error::Refused(Refusal::default()),
            "the job service gave an answer a read does not have",
        ),
        (
            Error::TooMany(1),
            "the job service gave an answer a read does not have",
        ),
        (
            Error::Stuck(Stuck::EmptyPages(1)),
            "the job service gave an answer a read does not have",
        ),
    ];
    for (error, said) in cases {
        let output = Output::unread(&error);
        let mut result = failed();
        output.attach(&mut result);
        let mut expected = fields("", "", "false");
        let note = format!("the job's output could not be read: {said}");
        expected.insert("note".to_owned(), note);
        assert_eq!(metadata(&result), expected);
        assert_eq!(
            result.status.as_ref().map(|s| s.message.as_str()),
            Some("why")
        );
    }
}

/// Catches the log excerpt cutting a character in two, keeping the head instead of the end,
/// keeping what a stream's white space adds, or showing something where there is nothing:
/// the end of the stream on a character boundary, trimmed; none at a cap of 0 or for white
/// space alone.
#[test]
fn the_stderr_excerpt_is_the_end_trimmed_on_a_character_boundary() {
    let output = |stderr: &str| Output {
        stderr: stderr.to_owned(),
        ..Output::default()
    };
    assert_eq!(output("one\ntwo\nthree\n").stderr_excerpt(6), Some("three"));
    assert_eq!(output("short\n").stderr_excerpt(512), Some("short"));
    // "é" is two bytes: a cap that lands inside it starts after it.
    assert_eq!(output("aéb").stderr_excerpt(2), Some("b"));
    assert_eq!(output("anything").stderr_excerpt(0), None);
    assert_eq!(output(" \n\t").stderr_excerpt(512), None);
    assert_eq!(output("").stderr_excerpt(512), None);
}
