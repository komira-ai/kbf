use std::collections::BTreeMap;

use js_proto::refusal::refusal_status;

use super::*;

/// The rows of the code table in this module's documentation: each code's name as the
/// table writes it (`NOT_FOUND`) and its class.
fn table() -> BTreeMap<String, Class> {
    let source = include_str!("error.rs");
    let mut rows = BTreeMap::new();
    for line in source.lines() {
        let Some(row) = line.strip_prefix("//! | `") else {
            continue;
        };
        let mut cells = row.split('|').map(str::trim);
        let name = cells.next().unwrap().trim_end_matches('`').to_owned();
        let class = match cells.next().unwrap() {
            "may have applied" => Class::MayHaveApplied,
            "definite" => Class::Definite,
            other => panic!("{name}: the class {other:?} is not one of the two"),
        };
        assert!(
            rows.insert(name.clone(), class).is_none(),
            "{name} is twice"
        );
    }
    rows
}

/// `NOT_FOUND` for the variant `NotFound`, as the gRPC specification and the table write it.
fn screaming(code: Code) -> String {
    let mut name = String::new();
    for (at, c) in format!("{code:?}").char_indices() {
        if c.is_uppercase() && at > 0 {
            name.push('_');
        }
        name.push(c.to_ascii_uppercase());
    }
    name
}

/// Asserts that each of the 17 gRPC codes (0 to 16) has exactly one row in the module's
/// table, that the table has no other row, and that `class_of` gives each code the class its
/// row says. Catches a code added to the match and not to the table, the reverse, and a row
/// whose class the code does not follow (a "definite" code taken as may-have-applied makes
/// a caller retry a refusal; the reverse drops a call that may have run).
#[test]
fn every_grpc_code_has_one_class() {
    let rows = table();
    let codes: Vec<Code> = (0..=16).map(Code::from_i32).collect();
    let names: Vec<String> = codes.iter().map(|code| screaming(*code)).collect();
    assert_eq!(names.len(), 17);
    assert_eq!(
        rows.keys().cloned().collect::<Vec<_>>(),
        {
            let mut sorted = names.clone();
            sorted.sort();
            sorted
        },
        "the table's codes are the 17 codes"
    );
    for code in codes {
        assert_eq!(class_of(code), rows[&screaming(code)], "{code:?}");
    }
    assert_eq!(screaming(Code::Ok), "OK");
    assert_eq!(screaming(Code::FailedPrecondition), "FAILED_PRECONDITION");
}

/// Asserts how a status becomes an error: one from a connection never made is
/// `Unreachable` whatever its code; one with a refusal in its details is `Refused` whatever
/// its code; a refusal's text without its details is no refusal; every other status follows
/// the table. Catches a refusal read from the message text, a connect failure taken for a
/// broken call, and a definite code taken for a lost answer.
#[test]
fn a_status_is_classed_by_connection_refusal_then_code() {
    let never = Status::from_error(Box::new(tonic::ConnectError(Box::new(
        std::io::Error::other("connection refused"),
    ))));
    assert_eq!(never.code(), Code::Unavailable);
    let error = from_status(never);
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );
    assert!(!error.may_have_applied());

    let broke = from_status(Status::unavailable("connection closed"));
    assert!(
        matches!(broke, Error::Transport(Broken::Status(_))),
        "{broke:?}"
    );
    assert!(broke.may_have_applied());

    let refused = from_status(refusal_status(RefusalCode::NoCapableHost, "no host"));
    assert!(refused.is(RefusalCode::NoCapableHost));
    assert!(!refused.is(RefusalCode::JobIdConflict));
    assert!(!refused.may_have_applied());
    assert_eq!(
        refused.refusal().map(|r| r.detail.as_str()),
        Some("no host")
    );

    let text_only = from_status(Status::already_exists("job-id-conflict"));
    assert!(text_only.refusal().is_none());
    assert!(!text_only.is(RefusalCode::JobIdConflict));
    assert!(matches!(
        &text_only,
        Error::Status { code: Code::AlreadyExists, message } if message == "job-id-conflict"
    ));
    assert!(!text_only.may_have_applied());
}

/// Asserts the retry class of each kind of error: only `Transport` and `Decode` may have
/// been applied. Catches an `Unreachable` taken for ambiguous (a caller that never retries a
/// call that was never sent) and a `Decode` taken for safe (a caller that thinks nothing ran).
#[test]
fn only_a_lost_or_unreadable_answer_may_have_applied() {
    let decode = Error::Decode(Undecodable::Missing("job"));
    assert!(decode.may_have_applied());
    let deadline = Error::Transport(Broken::Deadline(Duration::from_millis(5)));
    assert!(deadline.may_have_applied());
    let unsent = [
        Error::Unreachable(Unsent::BadId(BadId::Length(0))),
        Error::Unreachable(Unsent::TooLarge { len: 9, cap: 8 }),
        Error::TooMany(10),
        Error::Status {
            code: Code::NotFound,
            message: "no job j".to_owned(),
        },
    ];
    for error in unsent {
        assert!(!error.may_have_applied(), "{error:?}");
        assert!(error.refusal().is_none());
    }
}

/// Asserts the text of every kind of error, whole: what a log line says is what happened,
/// with the refusal's code and detail, and never the request. Catches a variant whose text
/// names the wrong side (sent or not) and a refusal printed without its code.
#[test]
fn each_error_says_what_happened() {
    let cases: Vec<(Error, &str)> = vec![
        (
            Error::Unreachable(Unsent::Connect(Status::unavailable("refused"))),
            "nothing was sent to the job service: cannot connect: refused",
        ),
        (
            Error::Unreachable(Unsent::BadId(BadId::Character(' '))),
            "nothing was sent to the job service: a job id holds letters, digits, '.', '_' \
             and '-' only, and this one holds ' '",
        ),
        (
            Error::Unreachable(Unsent::TooLarge { len: 9, cap: 8 }),
            "nothing was sent to the job service: the request is 9 bytes, over the cap of 8 \
             bytes",
        ),
        (
            Error::Transport(Broken::Status(Status::internal("stream reset"))),
            "the call to the job service failed after the request may have been sent: \
             Internal: stream reset",
        ),
        (
            Error::Transport(Broken::Deadline(Duration::from_millis(250))),
            "the call to the job service failed after the request may have been sent: no \
             whole answer within 250ms",
        ),
        (
            Error::Decode(Undecodable::Missing("result")),
            "the job service answered OK with a body this client cannot decode: the answer \
             has no result",
        ),
        (
            from_status(refusal_status(
                RefusalCode::JobIdConflict,
                "job-1 names another job",
            )),
            "the job service refused the job: job-id-conflict (job-1 names another job)",
        ),
        (
            Error::Status {
                code: Code::NotFound,
                message: "no job j-1".to_owned(),
            },
            "the job service answered NotFound: no job j-1",
        ),
        (
            Error::TooMany(10_000),
            "the listing holds more than 10000 jobs",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
    let bytes = Error::Decode(Undecodable::Bytes(
        <js_proto::v1::Job as prost::Message>::decode(&[0x0f_u8][..]).unwrap_err(),
    ));
    assert!(
        bytes
            .to_string()
            .starts_with("the job service answered OK with a body this client cannot decode: "),
        "{bytes}"
    );
}

/// Asserts the text of every way a config is refused, so an operator reading it knows which
/// part to fix. Catches two refusals that print alike.
#[test]
fn each_config_error_names_its_part() {
    let io = || std::io::Error::other("denied");
    let cases = [
        (
            ConfigError::Origin {
                origin: "ftp://x".to_owned(),
                reason: "the scheme must be http or https",
            },
            "origin \"ftp://x\": the scheme must be http or https",
        ),
        (
            ConfigError::PlainHttp("http://x".to_owned()),
            "origin \"http://x\" is plain http off loopback; the token would cross the network \
             in clear",
        ),
        (
            ConfigError::MissingCa,
            "an https origin needs the pinned CA certificate (PEM)",
        ),
        (
            ConfigError::UnusedCa,
            "a CA certificate was given for an origin that does not use TLS",
        ),
        (
            ConfigError::BadCa("no certificate".to_owned()),
            "the CA PEM is unusable: no certificate",
        ),
        (ConfigError::BadToken("empty"), "the token is empty"),
        (
            ConfigError::Tls(rustls::Error::General("x".to_owned())),
            "TLS setup: unexpected error: x",
        ),
        (
            ConfigError::TokenFile {
                path: "/t".into(),
                source: io(),
            },
            "token file /t: denied",
        ),
        (
            ConfigError::TokenFileMode {
                path: "/t".into(),
                problem: "over 4 KiB",
            },
            "token file /t: over 4 KiB",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}
