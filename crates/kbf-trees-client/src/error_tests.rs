use super::*;

/// The module's table, row for row: each of the 17 codes and its class.
const TABLE: [(Code, Class); 17] = [
    (Code::Ok, Class::MayHaveApplied),
    (Code::Cancelled, Class::MayHaveApplied),
    (Code::Unknown, Class::MayHaveApplied),
    (Code::InvalidArgument, Class::Definite),
    (Code::DeadlineExceeded, Class::MayHaveApplied),
    (Code::NotFound, Class::Definite),
    (Code::AlreadyExists, Class::Definite),
    (Code::PermissionDenied, Class::Definite),
    (Code::ResourceExhausted, Class::Definite),
    (Code::FailedPrecondition, Class::Definite),
    (Code::Aborted, Class::MayHaveApplied),
    (Code::OutOfRange, Class::MayHaveApplied),
    (Code::Unimplemented, Class::Definite),
    (Code::Internal, Class::MayHaveApplied),
    (Code::Unavailable, Class::MayHaveApplied),
    (Code::DataLoss, Class::MayHaveApplied),
    (Code::Unauthenticated, Class::Definite),
];

/// Asserts every gRPC code is classed as the module's table says, and that a status of each
/// code that names no refusal becomes `Transport` or `Status` by that class, keeping its code
/// and message. Catches a code classed the wrong way, which would make a caller repeat a
/// call the node refused or give up on one the node may have applied.
#[test]
fn each_code_is_classed_as_the_table_says() {
    for (code, class) in TABLE {
        assert_eq!(class_of(code), class, "{code:?}");
        let error = from_status(Status::new(code, "a broken pipe"));
        match class {
            Class::MayHaveApplied => {
                assert!(
                    matches!(&error, Error::Transport(Broken::Status(s)) if s.code() == code),
                    "{error:?}"
                );
                assert!(error.may_have_applied());
            }
            Class::Definite => {
                assert!(
                    matches!(&error, Error::Status { code: c, message } if *c == code && message == "a broken pipe"),
                    "{error:?}"
                );
                assert!(!error.may_have_applied());
            }
        }
        assert!(error.refusal().is_none());
    }
}

/// Asserts each of the node's refusals is read from its status, alone and with what it is
/// about, whatever the code's class: the reason, the subject and the code; and that it is not
/// one the caller is told may have applied. Catches a name that differs from tierstore's
/// `refusal.rs`, or a refusal of class "may have applied" (`busy`) taken for a lost answer.
#[test]
fn every_refusal_of_the_node_is_read_by_name() {
    for reason in Reason::ALL {
        let alone = from_status(Status::new(reason.code(), reason.name()));
        let refusal = alone.refusal().expect("a refusal");
        assert_eq!(
            (refusal.reason(), refusal.about(), refusal.code()),
            (reason, None, reason.code())
        );
        assert!(alone.is(reason));
        assert!(!alone.may_have_applied());
        let message = format!("{}: jobs/k1-2-3-0", reason.name());
        let about = from_status(Status::new(reason.code(), message.clone()));
        let refusal = about.refusal().expect("a refusal");
        assert_eq!(refusal.about(), Some("jobs/k1-2-3-0"));
        assert_eq!(refusal.to_string(), message);
        assert_eq!(
            about.to_string(),
            format!("the trees interface refused the call: {message}")
        );
    }
    assert_eq!(Reason::ALL.len(), 34);
}

/// Asserts a status is a refusal only with the code the node answers it with, and a name
/// that is the whole message or ends at `: `. A transport's `UNAVAILABLE` that begins like a
/// name, a name under another code, a name followed by other text, and a longer name that
/// begins with a shorter one (`tree too large for the cache`, `tree digest mismatch`) are
/// told apart. Catches a prefix match that reads `busyness` as `busy`, or a broken
/// connection's message as a refusal.
#[test]
fn a_status_is_a_refusal_only_by_its_exact_name_and_code() {
    let read = |code, message: &str| Refusal::of(&Status::new(code, message));
    assert_eq!(read(Code::Unavailable, "busyness"), None);
    assert_eq!(read(Code::Unavailable, "busy:no space"), None);
    assert_eq!(read(Code::NotFound, "busy"), None);
    assert_eq!(read(Code::Unavailable, "connection reset"), None);
    let cache = read(Code::ResourceExhausted, "tree too large for the cache").expect("cache");
    assert_eq!(cache.reason(), Reason::TooLargeForCache);
    let tree = read(Code::InvalidArgument, "tree too large: 40 GiB").expect("tree");
    assert_eq!(
        (tree.reason(), tree.about()),
        (Reason::TreeTooLarge, Some("40 GiB"))
    );
    let digest = read(Code::InvalidArgument, "tree digest mismatch").expect("digest");
    assert_eq!(digest.reason(), Reason::DigestMismatch);
    let lie = read(Code::FailedPrecondition, "mismatch: bin/cc").expect("mismatch");
    assert_eq!(lie.reason(), Reason::Mismatch);
}

/// Asserts `request too large` is a refusal under `INVALID_ARGUMENT` (the node's check) and
/// under `OUT_OF_RANGE` (the gRPC layer's, before the request is read whole), and that no
/// other reason takes `OUT_OF_RANGE`. Catches a request the node would never take being
/// repeated as a lost answer.
#[test]
fn request_too_large_is_read_under_either_code() {
    let message = "request too large: the limit is 268435456 bytes";
    for code in [Code::InvalidArgument, Code::OutOfRange] {
        let error = from_status(Status::new(code, message));
        assert!(error.is(Reason::RequestTooLarge), "{error:?}");
        assert!(!error.may_have_applied());
    }
    let others = Reason::ALL
        .into_iter()
        .filter(|reason| reason.answered_by(Code::OutOfRange));
    assert_eq!(others.collect::<Vec<_>>(), [Reason::RequestTooLarge]);
}

/// Asserts each error says what happened, with its cause.
#[test]
fn each_error_says_what_happened() {
    let unsent = Error::Unreachable(Unsent::TooLarge { len: 9, cap: 8 });
    assert_eq!(
        unsent.to_string(),
        "nothing was sent to the trees interface: the request is 9 bytes, over the cap of 8 bytes"
    );
    let late = Error::Transport(Broken::Deadline(Duration::from_secs(45)));
    assert_eq!(
        late.to_string(),
        "the call to the trees interface failed after the request may have been sent: no whole answer within 45s"
    );
    let digest = Error::Decode(Undecodable::Digest(3));
    assert!(digest.may_have_applied());
    assert_eq!(
        digest.to_string(),
        "the trees interface answered OK with a body this client cannot decode: the answer's tree digest is 3 bytes, not 32"
    );
    let nothing = Error::Decode(Undecodable::NoPlacement);
    assert!(
        nothing
            .to_string()
            .ends_with("the answer names no placement")
    );
    assert!(!nothing.is(Reason::Busy));
    let status = Error::Status {
        code: Code::Unimplemented,
        message: String::new(),
    };
    assert_eq!(
        status.to_string(),
        "the trees interface answered Unimplemented: "
    );
}
