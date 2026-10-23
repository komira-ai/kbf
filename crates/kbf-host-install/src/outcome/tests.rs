use serde_json::{Value, json};

use super::{Failure, FailureCode, Outcome, QueryReport, Refusal};

fn json(outcome: &Outcome) -> Value {
    serde_json::from_str(&outcome.json_line()).unwrap()
}

const ALL_REFUSALS: [(Refusal, &str); 10] = [
    (Refusal::BadCommand, "bad_command"),
    (Refusal::ChannelNotAllowed, "channel_not_allowed"),
    (Refusal::ConfigInvalid, "config_invalid"),
    (Refusal::ConfigUnsafe, "config_unsafe"),
    (Refusal::ChannelUnsigned, "channel_unsigned"),
    (Refusal::KeyUntrusted, "key_untrusted"),
    (Refusal::ChannelInsecure, "channel_insecure"),
    (Refusal::UnknownRelease, "unknown_release"),
    (Refusal::AmbiguousRelease, "ambiguous_release"),
    (Refusal::FetchFailed, "fetch_failed"),
];

const ALL_FAILURES: [(FailureCode, &str); 11] = [
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
    (FailureCode::RestartFailed, "restart_failed"),
    (FailureCode::RestartTimeout, "restart_timeout"),
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
        node_id: None,
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

/// The query line is `result` and then the four fields, in this order, with nulls spelled
/// out: every answer of the program has a `result`.
#[test]
fn query_line_shape_and_order() {
    let full = Outcome::Queried(QueryReport {
        installed_version: Some("1.0-1".into()),
        running_commit: Some("abc".into()),
        node_id: Some("node-1".into()),
        in_progress: true,
    });
    assert_eq!(
        full.json_line(),
        r#"{"result":"query","installed_version":"1.0-1","running_commit":"abc","node_id":"node-1","in_progress":true}"#
    );
    let empty = Outcome::Queried(QueryReport {
        installed_version: None,
        running_commit: None,
        node_id: None,
        in_progress: false,
    });
    assert_eq!(
        empty.json_line(),
        r#"{"result":"query","installed_version":null,"running_commit":null,"node_id":null,"in_progress":false}"#
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
        node_id: None,
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

/// Catches a code table that drifted from the enums: `ALL` lists each variant once, in the
/// order of the contract, and a code reads back to the variant that wrote it.
#[test]
fn codes_round_trip() {
    assert_eq!(
        Refusal::ALL.map(Refusal::code),
        ALL_REFUSALS.map(|(_, code)| code)
    );
    assert_eq!(
        FailureCode::ALL.map(FailureCode::code),
        ALL_FAILURES.map(|(_, code)| code)
    );
    for refusal in Refusal::ALL {
        assert_eq!(Refusal::from_code(refusal.code()), Some(refusal));
    }
    for failure in FailureCode::ALL {
        assert_eq!(FailureCode::from_code(failure.code()), Some(failure));
    }
    for text in [
        "",
        "nope",
        "Busy",
        "busy ",
        "bad_command\n",
        "install_failed",
    ] {
        assert_eq!(Refusal::from_code(text), None, "{text:?}");
    }
    for text in ["", "nope", "BUSY", " busy", "bad_command"] {
        assert_eq!(FailureCode::from_code(text), None, "{text:?}");
    }
}

/// Catches an install unit's answer that is lost or changed on the way back: each line the
/// unit can print reads back to the outcome that wrote it (a failure without its detail).
#[test]
fn a_unit_line_reads_back() {
    let mut outcomes = vec![
        Outcome::Installed {
            version: "1.2.3-1".into(),
        },
        Outcome::AlreadyInstalled {
            version: "1.2.3-01".into(),
        },
    ];
    outcomes.extend(Refusal::ALL.map(Outcome::Refused));
    outcomes.extend(FailureCode::ALL.map(|code| {
        Outcome::Failed(Failure {
            code,
            detail: String::new(),
        })
    }));
    for outcome in outcomes {
        assert_eq!(
            Outcome::from_unit_line(&outcome.json_line()),
            Some(outcome.clone()),
            "{outcome:?}"
        );
    }
    let detailed = Outcome::Failed(Failure {
        code: FailureCode::InstallFailed,
        detail: "E: x".into(),
    });
    assert_eq!(
        Outcome::from_unit_line(&detailed.json_line()),
        Some(Outcome::Failed(Failure {
            code: FailureCode::InstallFailed,
            detail: String::new()
        }))
    );
}

/// Catches a unit line that is believed when it is not one of ours: anything but the exact
/// closed lines is none.
#[test]
fn other_lines_are_not_a_units_answer() {
    for line in [
        "",
        "installed",
        "[]",
        "null",
        "{}",
        r#"{"result":"installed"}"#,
        r#"{"result":"installed","installed_version":"1"}"#,
        r#"{"result":"installed","installed_version":"1","unit_state":"not_run"}"#,
        r#"{"result":"already_installed","installed_version":"1","unit_state":"finished"}"#,
        r#"{"result":"installed","installed_version":1,"unit_state":"finished"}"#,
        r#"{"result":"installed","installed_version":"1","unit_state":"finished","x":1}"#,
        r#"{"result":"refused"}"#,
        r#"{"result":"refused","reason":"bad_command","installed_version":"1"}"#,
        r#"{"result":"failed","reason":"busy","installed_version":"1"}"#,
        r#"{"result":"refused","reason":"bad_command","unit_state":"finished"}"#,
        r#"{"result":"installed","unit_state":"finished"}"#,
        r#"{"result":"already_installed","unit_state":"not_run"}"#,
        r#"{"result":"installed","reason":"busy","installed_version":"1","unit_state":"finished"}"#,
        r#"{"result":"refused","reason":"nope"}"#,
        r#"{"result":"refused","reason":"busy"}"#,
        r#"{"result":"failed","reason":"bad_command"}"#,
        r#"{"result":"failed"}"#,
        r#"{"result":"failed","reason":"busy","extra":true}"#,
        r#"{"result":"query","installed_version":null,"running_commit":null,"in_progress":false}"#,
        r#"{"result":"other","reason":"busy"}"#,
        r#"{"reason":"busy"}"#,
    ] {
        assert_eq!(Outcome::from_unit_line(line), None, "{line:?}");
    }
}

/// Asserts that a restart answers with the `restarted` line naming the unit, exit status 0,
/// the audit outcome `restarted` and no stderr detail, and that the line is not the answer
/// of an install unit.
#[test]
fn the_restarted_answer() {
    let restarted = Outcome::Restarted {
        unit: "kbf-frontend@a1.service".into(),
    };
    assert_eq!(
        restarted.json_line(),
        r#"{"result":"restarted","unit":"kbf-frontend@a1.service"}"#
    );
    assert_eq!(restarted.exit_code(), 0);
    assert_eq!(restarted.audit_fields(), ("restarted", None));
    assert_eq!(restarted.stderr_detail(), None);
    assert_eq!(Outcome::from_unit_line(&restarted.json_line()), None);
}

/// A Mac's two answers have their own lines, which the Linux ones never are: the apply
/// started (exit 0), and the query with the standing of the last apply after the four fields
/// of a query; neither is an install unit's answer.
#[test]
fn the_mac_answers() {
    let started = Outcome::ApplyStarted {
        version: "0.1.7+gbbbbbbbbbbbb-3".into(),
        commit: "b".repeat(40),
    };
    assert_eq!(
        started.json_line(),
        format!(
            r#"{{"result":"apply_started","version":"0.1.7+gbbbbbbbbbbbb-3","commit":"{}","apply":"running"}}"#,
            "b".repeat(40)
        )
    );
    assert_eq!(started.exit_code(), 0);
    assert_eq!(started.audit_fields(), ("apply_started", None));
    assert_eq!(started.stderr_detail(), None);
    let report = || QueryReport {
        installed_version: Some("1".into()),
        running_commit: Some("abc".into()),
        node_id: Some("mac-a".into()),
        in_progress: false,
    };
    let done = Outcome::MacQueried {
        report: report(),
        apply: Some("failed:not_welcomed:rolled_back".into()),
    };
    assert_eq!(
        done.json_line(),
        r#"{"result":"query","installed_version":"1","running_commit":"abc","node_id":"mac-a","in_progress":false,"apply":"failed:not_welcomed:rolled_back"}"#
    );
    let none = Outcome::MacQueried {
        report: report(),
        apply: None,
    };
    assert!(
        none.json_line()
            .ends_with(r#""in_progress":false,"apply":null}"#)
    );
    assert_eq!(
        (done.exit_code(), none.audit_fields()),
        (0, ("query", None))
    );
    for outcome in [&started, &done] {
        assert_eq!(Outcome::from_unit_line(&outcome.json_line()), None);
    }
    // The Linux query line is unchanged: it has no `apply`.
    assert!(!Outcome::Queried(report()).json_line().contains("apply"));
}
