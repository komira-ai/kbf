use std::collections::BTreeMap;

use js_proto::v1::{GetLogsResponse, JobFile, JobResult, JobSpec, Refusal};
use kbf_job_client::{Broken, Error, Stuck, Undecodable, Unsent};
use kbf_proto::google::rpc::{self, ErrorInfo};
use kbf_proto::worker;
use prost::Message;
use std::time::Duration;
use tonic::Code;

use super::{
    Grants, MIN_CUT_SECRET, OUTPUT_CAP, OUTPUT_REASON, Output, REDACTED, Tail, ending, shares,
};

const URL: &str = "https://store.test/cas/b/1-1/aa-0?X-Signature=0123456789abcdef0123456789";
const SIGNATURE: &str = "X-Signature=0123456789abcdef0123456789";
const TOKEN: &str = "read-token-fedcba9876543210fedcba98";

fn grants() -> Grants {
    Grants::of(&JobSpec::default(), [URL, TOKEN])
}

/// No run of more than `REDACTED.len()` bytes of `secret` is in `text`: neither the secret,
/// nor a prefix or suffix of it longer than the marker.
fn assert_hidden(text: &str, secret: &str) {
    let window = REDACTED.len() + 1;
    for at in 0..=secret.len() - window {
        let piece = &secret[at..at + window];
        assert!(!text.contains(piece), "{piece:?} of a grant is in {text:?}");
    }
}

/// Catches a grant reaching the attached output: each occurrence of the url and the token is
/// one marker, wherever it is, and so is the url's signature alone.
#[test]
fn every_occurrence_of_a_grant_is_replaced_by_one_marker() {
    let text = format!(
        "{URL} start {TOKEN} middle {TOKEN}{TOKEN} sig {SIGNATURE} value {} end {URL}",
        &SIGNATURE["X-Signature=".len()..]
    );
    let redacted = grants().redact(text.as_bytes(), false);
    assert_eq!(
        String::from_utf8(redacted).expect("utf-8"),
        "[redacted] start [redacted] middle [redacted] sig [redacted] value [redacted] end \
         [redacted]"
    );
    // A short query value is not a grant of its own: it would redact ordinary text.
    let short = Grants::of(&JobSpec::default(), ["https://s.test/k?a=1&sig=0123456789"]);
    assert_eq!(
        short.redact(b"a=1 sig 0123456789", false),
        b"a=1 sig [redacted]"
    );
}

/// Catches two grants that overlap leaving the end of the second: the bytes of both are one
/// run, and one marker.
#[test]
fn overlapping_grants_are_one_marker() {
    let grants = Grants::of(&JobSpec::default(), ["abcdefgh", "efghijkl"]);
    let redacted = grants.redact(b"<abcdefghijkl>", false);
    assert_eq!(redacted, b"<[redacted]>");
}

/// Catches the end of a grant at the head of a stream the service cut being left, and short
/// heads being redacted from text that only happens to end the same way.
#[test]
fn the_head_of_a_cut_stream_is_redacted_when_it_ends_a_grant() {
    let grants = grants();
    let head = &TOKEN[TOKEN.len() - MIN_CUT_SECRET..];
    let text = format!("{head} then text");
    assert_eq!(
        grants.redact(text.as_bytes(), true),
        b"[redacted] then text"
    );
    assert_eq!(
        grants.redact(text.as_bytes(), false),
        text.as_bytes(),
        "a stream that was not cut is left as it is"
    );
    let short = format!("{} then text", &TOKEN[TOKEN.len() - (MIN_CUT_SECRET - 1)..]);
    assert_eq!(grants.redact(short.as_bytes(), true), short.as_bytes());
}

/// Catches the grants of the job's files and environment being missed, empty values being
/// taken as grants (an empty one matches everywhere), and a grant printing.
#[test]
fn the_grants_are_the_secrets_the_files_and_the_environment() {
    let spec = JobSpec {
        files: vec![JobFile {
            name: "f".to_owned(),
            contents: "file-secret".to_owned(),
        }],
        env: [
            ("A".to_owned(), "env-secret".to_owned()),
            ("B".to_owned(), String::new()),
        ]
        .into(),
        ..JobSpec::default()
    };
    let grants = Grants::of(&spec, ["", "named-secret", "named-secret"]);
    let redacted = grants.redact(b"file-secret env-secret named-secret plain", false);
    assert_eq!(redacted, b"[redacted] [redacted] [redacted] plain");
    assert_eq!(format!("{grants:?}"), "Grants(3 redacted)");
    assert_eq!(grants.redact(b"", true), b"");
}

/// Catches a grant cut in two by the cap leaving a piece of it: redaction comes first, so the
/// cap can cut only the marker.
#[test]
fn a_grant_on_the_cap_boundary_leaves_no_piece_of_it() {
    // Both streams are over half the cap, so each keeps its last half. Redacted, stderr is
    // 16 bytes over it: the cut falls five bytes into the marker of its url.
    let half = OUTPUT_CAP / 2;
    let stdout = format!("{TOKEN}{}{TOKEN}", "o".repeat(half));
    let filler = "e".repeat(half - 30);
    let stderr = format!("{TOKEN}-{URL}{filler}{TOKEN}-mid-{TOKEN}");
    let tail = Tail {
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
        truncated: false,
    };
    let output = Output::of(&tail, &grants());
    for text in [&output.stdout, &output.stderr] {
        assert_hidden(text, URL);
        assert_hidden(text, TOKEN);
    }
    assert!(output.truncated);
    assert!(
        output.stderr.starts_with("cted]e"),
        "{}",
        &output.stderr[..20]
    );
    assert!(output.stderr.ends_with("e[redacted]-mid-[redacted]"));
}

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
    let output = Output::of(&tail, &Grants::default());
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
    let output = Output::of(&small, &Grants::default());
    assert_eq!(
        (output.stdout.as_str(), output.stderr.as_str()),
        ("out", "err")
    );
    assert!(!output.truncated);
    let cut = Tail {
        truncated: true,
        ..small
    };
    assert!(Output::of(&cut, &Grants::default()).truncated);
}

/// Catches bytes that are not UTF-8 making the output unreadable or dropped.
#[test]
fn bytes_that_are_not_utf8_are_replaced() {
    let tail = Tail {
        stdout: b"a\xffb".to_vec(),
        stderr: Vec::new(),
        truncated: false,
    };
    assert_eq!(Output::of(&tail, &Grants::default()).stdout, "a\u{fffd}b");
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
        message: format!("held {TOKEN}"),
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
