use super::*;

/// Asserts that every code the server documents reads and prints as itself, and that a code
/// this client has not heard of survives as `Other` with its text. It catches a table row
/// whose text and variant disagree (a copy-paste slip that would make a refusal be taken
/// for another one), and an unknown code swallowed or turned into a known one.
#[test]
fn every_code_round_trips_and_an_unknown_one_is_kept() {
    for (text, code) in CODES {
        assert_eq!(&ErrorCode::from_text(text), code, "{text}");
        assert_eq!(code.as_str(), *text);
        assert_eq!(code.to_string(), *text);
    }
    let unknown = ErrorCode::from_text("brand-new");
    assert_eq!(unknown, ErrorCode::Other("brand-new".to_owned()));
    assert_eq!(unknown.as_str(), "brand-new");
    assert_eq!(ErrorCode::Unspecified.as_str(), "-");
    let mut texts: Vec<_> = CODES.iter().map(|(text, _)| *text).collect();
    texts.sort_unstable();
    texts.dedup();
    assert_eq!(texts.len(), CODES.len(), "two rows share a text");
}

/// Asserts the retry classification: only an error after the request left may have been
/// applied, and a refusal answers the caller's question either way. It catches an
/// `Unreachable` taken for ambiguous (a retry loop that never retries) and a `Transport`
/// taken for safe (a queued run submitted twice).
#[test]
fn only_a_failure_after_sending_may_have_applied() {
    let io = || std::io::Error::other("x");
    let decode = serde_json::from_str::<u8>("x").unwrap_err();
    let refusal = ApiError {
        status: 409,
        code: ErrorCode::NoCapacity,
        reason: None,
    };
    assert!(!Error::Unreachable(io().into()).may_have_applied());
    assert!(Error::Transport(io().into()).may_have_applied());
    assert!(
        Error::Decode {
            status: 202,
            source: decode
        }
        .may_have_applied()
    );
    let refused = Error::from(refusal.clone());
    assert!(!refused.may_have_applied());
    assert_eq!(refused.api(), Some(&refusal));
    assert!(refused.is(&ErrorCode::NoCapacity));
    assert!(!refused.is(&ErrorCode::NoFit));
    assert!(!Error::Unreachable(io().into()).is(&ErrorCode::NoCapacity));
    assert_eq!(Error::Transport(io().into()).api(), None);
}

/// Asserts the refusal's text carries status, code and the 400 reason, and nothing else.
#[test]
fn a_refusal_prints_its_status_code_and_reason() {
    let plain = ApiError {
        status: 410,
        code: ErrorCode::RunForgotten,
        reason: None,
    };
    assert_eq!(
        plain.to_string(),
        "the lease server answered 410 run-forgotten"
    );
    let bad = ApiError {
        status: 400,
        code: ErrorCode::BadRequest,
        reason: Some("the body is not valid JSON (line 1, column 2)".to_owned()),
    };
    assert_eq!(
        bad.to_string(),
        "the lease server answered 400 bad-request (the body is not valid JSON (line 1, column 2))"
    );
}

/// Asserts that the code a js-agent refuses a start with when its scratch quota is out of
/// order is typed, so that a caller can tell it from a code it does not know. It catches the
/// code falling into `Other`, where the executor would treat it as an infrastructure failure
/// of the action.
#[test]
fn quota_unavailable_is_a_code_of_its_own() {
    let code = ErrorCode::from_text("quota-unavailable");
    assert_eq!(code, ErrorCode::QuotaUnavailable);
    assert_eq!(code.as_str(), "quota-unavailable");
}

/// Asserts that the two codes a lease server answers a start with files are typed: both
/// directions of the table, so a caller can tell a host that cannot deliver files from a
/// file that is wrong. It catches the codes falling into `Other`, where the executor would
/// treat both as an infrastructure failure of the action.
#[test]
fn error_codes_files_unsupported_and_invalid_file_parse_both_ways() {
    for (text, code) in [
        ("files-unsupported", ErrorCode::FilesUnsupported),
        ("invalid-file", ErrorCode::InvalidFile),
    ] {
        assert_eq!(ErrorCode::from_text(text), code);
        assert_eq!(code.as_str(), text);
        assert_eq!(code.to_string(), text);
        assert!(CODES.contains(&(text, code)), "{text} is in the table");
    }
}
