use serde_json::{Value, json};

use super::{Failure, FailureCode, Outcome, QueryReport, Refusal};

fn json(outcome: &Outcome) -> Value {
    serde_json::from_str(&outcome.json_line()).unwrap()
}

const ALL_REFUSALS: [(Refusal, &str); 7] = [
    (Refusal::BadCommand, "bad_command"),
    (Refusal::ChannelNotAllowed, "channel_not_allowed"),
    (Refusal::ConfigInvalid, "config_invalid"),
    (Refusal::ConfigUnsafe, "config_unsafe"),
    (Refusal::ChannelUnsigned, "channel_unsigned"),
    (Refusal::KeyUntrusted, "key_untrusted"),
    (Refusal::OlderThanInstalled, "older_than_installed"),
];

const ALL_FAILURES: [(FailureCode, &str); 9] = [
    (FailureCode::AuditUnavailable, "audit_unavailable"),
    (FailureCode::QueryFailed, "query_failed"),
    (
        FailureCode::InstalledVersionInvalid,
        "installed_version_invalid",
    ),
    (FailureCode::PackageStateUnclean, "package_state_unclean"),
    (FailureCode::UpdateFailed, "update_failed"),
    (FailureCode::InstallFailed, "install_failed"),
    (FailureCode::VerifyFailed, "verify_failed"),
    (FailureCode::Busy, "busy"),
    (FailureCode::LockUnavailable, "lock_unavailable"),
];

/// Catches a renamed code: the codes are the contract with the updater.
#[test]
fn the_codes_are_the_closed_set() {
    for (refusal, code) in ALL_REFUSALS {
        assert_eq!(refusal.code(), code);
    }
    for (failure, code) in ALL_FAILURES {
        assert_eq!(failure.code(), code);
    }
}

/// Catches an exit status that moved: 2 is "nothing ran", 3 is "something failed".
#[test]
fn exit_codes() {
    let installed = Outcome::Installed {
        version: "1.0".into(),
    };
    let already = Outcome::AlreadyInstalled {
        version: "1.0".into(),
    };
    let query = Outcome::Queried(QueryReport {
        installed_version: None,
        running_commit: None,
        in_progress: false,
    });
    assert_eq!(
        [&installed, &already, &query].map(Outcome::exit_code),
        [0, 0, 0]
    );
    for (refusal, _) in ALL_REFUSALS {
        assert_eq!(Outcome::Refused(refusal).exit_code(), 2);
    }
    for (code, _) in ALL_FAILURES {
        let failed = Outcome::Failed(Failure {
            code,
            detail: String::new(),
        });
        assert_eq!(failed.exit_code(), 3);
    }
}

/// The shapes of the lines the updater parses.
#[test]
fn json_shapes() {
    assert_eq!(
        json(&Outcome::Installed {
            version: "1.0-1".into()
        }),
        json!({"result": "installed", "installed_version": "1.0-1", "unit_state": "finished"})
    );
    assert_eq!(
        json(&Outcome::AlreadyInstalled {
            version: "1.0-1".into()
        }),
        json!({"result": "already_installed", "installed_version": "1.0-1", "unit_state": "not_run"})
    );
    for (refusal, code) in ALL_REFUSALS {
        assert_eq!(
            json(&Outcome::Refused(refusal)),
            json!({"result": "refused", "reason": code})
        );
    }
    for (failure, code) in ALL_FAILURES {
        let outcome = Outcome::Failed(Failure {
            code: failure,
            detail: "x".into(),
        });
        assert_eq!(json(&outcome), json!({"result": "failed", "reason": code}));
    }
}

/// The query line is exactly the three fields, in this order, with nulls spelled out.
#[test]
fn query_line_shape_and_order() {
    let full = Outcome::Queried(QueryReport {
        installed_version: Some("1.0-1".into()),
        running_commit: Some("abc".into()),
        in_progress: true,
    });
    assert_eq!(
        full.json_line(),
        r#"{"installed_version":"1.0-1","running_commit":"abc","in_progress":true}"#
    );
    let empty = Outcome::Queried(QueryReport {
        installed_version: None,
        running_commit: None,
        in_progress: false,
    });
    assert_eq!(
        empty.json_line(),
        r#"{"installed_version":null,"running_commit":null,"in_progress":false}"#
    );
}

/// A line is one line: a version with a quote or a newline in it (it cannot come from the
/// grammar, but dpkg's text is untrusted too) is escaped, never echoed raw.
#[test]
fn a_value_is_escaped_not_echoed() {
    let line = Outcome::Installed {
        version: "1\"\n,\"result\":\"x".into(),
    }
    .json_line();
    assert!(!line.contains('\n'));
    assert_eq!(
        json(&Outcome::Installed {
            version: "1\"\n,\"result\":\"x".into()
        })["result"],
        "installed"
    );
}

/// The audit fields: outcome and reason, one per case.
#[test]
fn audit_fields() {
    assert_eq!(
        Outcome::Installed {
            version: "1".into()
        }
        .audit_fields(),
        ("installed", None)
    );
    assert_eq!(
        Outcome::AlreadyInstalled {
            version: "1".into()
        }
        .audit_fields(),
        ("already_installed", None)
    );
    let query = Outcome::Queried(QueryReport {
        installed_version: None,
        running_commit: None,
        in_progress: false,
    });
    assert_eq!(query.audit_fields(), ("query", None));
    assert_eq!(
        Outcome::Refused(Refusal::KeyUntrusted).audit_fields(),
        ("refused", Some("key_untrusted"))
    );
    let failed = Outcome::Failed(Failure {
        code: FailureCode::UpdateFailed,
        detail: String::new(),
    });
    assert_eq!(failed.audit_fields(), ("failed", Some("update_failed")));
}

/// The stderr detail is the failure's, with control characters made visible; a success or
/// a refusal has none. Long detail is cut.
#[test]
fn stderr_detail_is_cleaned_and_bounded() {
    assert_eq!(Outcome::Refused(Refusal::BadCommand).stderr_detail(), None);
    assert_eq!(
        Outcome::Installed {
            version: "1".into()
        }
        .stderr_detail(),
        None
    );
    let failed = |detail: &str| {
        Outcome::Failed(Failure {
            code: FailureCode::InstallFailed,
            detail: detail.into(),
        })
    };
    assert_eq!(
        failed("E: held\n").stderr_detail().as_deref(),
        Some("E: held\n")
    );
    assert_eq!(
        failed("a\x1b[31mb\0c\r").stderr_detail().as_deref(),
        Some("a?[31mb?c?"),
        "escape, NUL and CR become '?'; a tab and a newline stay"
    );
    assert_eq!(failed("a\tb").stderr_detail().as_deref(), Some("a\tb"));
    let long = failed(&"x".repeat(10_000)).stderr_detail().unwrap();
    assert_eq!(long.len(), 2048);
    let multibyte = failed(&"\u{e9}".repeat(2000)).stderr_detail().unwrap();
    assert!(multibyte.len() <= 2048 && multibyte.chars().all(|c| c == '\u{e9}'));
}
