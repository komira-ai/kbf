use super::*;

const SENTINEL: &str = "SENTINEL-secret-key-0123456789";

fn put_body(extra: &str) -> String {
    format!(
        r#"{{"endpoint":"http://s3:9000","access_key_id":"AKIDEXAMPLE","secret_access_key":"{SENTINEL}"{extra}}}"#
    )
}

// Catches a name check that lets a path, a custom-method colon, a NUL, an upper-case or
// a leading-punctuation name through: the name becomes a file name and a URL segment.
#[test]
fn a_connection_name_is_a_plain_lower_case_word() {
    for good in ["a", "0", "cold-main", "prod_1", &"x".repeat(63)] {
        assert_eq!(good.parse::<ConnectionName>().unwrap().as_str(), good);
    }
    for bad in [
        "../x", "a/b", "a.b", "a:verify", "a\0b", "A", "-a", "_a", "a b", "é", ".", "..", "a%2fb",
    ] {
        assert_eq!(
            bad.parse::<ConnectionName>(),
            Err(NameError::Character),
            "{bad:?}"
        );
    }
    assert_eq!("".parse::<ConnectionName>(), Err(NameError::Length));
    assert!(NameError::Length.to_string().contains("1 to 63"));
    assert!(
        NameError::Character
            .to_string()
            .contains("a-z, 0-9, _ and -")
    );
    assert_eq!(
        "x".repeat(64).parse::<ConnectionName>(),
        Err(NameError::Length)
    );
}

// Catches a Deserialize that skips the check (a name in a body would then reach the
// filesystem) and a Serialize that changes the spelling.
#[test]
fn a_connection_name_in_json_is_checked_and_round_trips() {
    let name: ConnectionName = serde_json::from_str(r#""cold-1""#).unwrap();
    assert_eq!(serde_json::to_string(&name).unwrap(), r#""cold-1""#);
    assert_eq!(String::from(name.clone()), "cold-1");
    assert_eq!(name.to_string(), "cold-1");
    assert_eq!(ConnectionName::try_from("cold-1".to_owned()), Ok(name));
    assert!(serde_json::from_str::<ConnectionName>(r#""../etc""#).is_err());
}

// Catches a role parser that accepts history or backup (no consumer exists) or maps them
// to the same error as nonsense: the API answers them differently.
#[test]
fn only_the_cold_role_exists() {
    assert_eq!("cold".parse::<BindingRole>(), Ok(BindingRole::Cold));
    assert_eq!(BindingRole::Cold.to_string(), "cold");
    assert_eq!(
        "history".parse::<BindingRole>(),
        Err(RoleFault::Unsupported)
    );
    assert_eq!("backup".parse::<BindingRole>(), Err(RoleFault::Unsupported));
    assert_eq!("Cold".parse::<BindingRole>(), Err(RoleFault::Unknown));
    assert!(RoleFault::Unsupported.to_string().contains("no consumer"));
    assert!(RoleFault::Unknown.to_string().contains("unknown role"));
    assert_eq!(
        serde_json::to_string(&BindingRole::Cold).unwrap(),
        r#""cold""#
    );
}

// Catches a derived Debug on Secret, and a ConnectionPut whose Debug reaches the text some
// other way: a tracing line or an error that formats the body must not print the key.
#[test]
fn a_secret_prints_as_redacted_everywhere_it_is_formatted() {
    let secret = Secret::new(SENTINEL.to_owned());
    assert_eq!(format!("{secret:?}"), "Secret(..)");
    assert_eq!(format!("{secret:#?}"), "Secret(..)");
    let put: ConnectionPut = serde_json::from_str(&put_body("")).unwrap();
    assert_eq!(put.secret_access_key.expose(), SENTINEL);
    for text in [format!("{put:?}"), format!("{put:#?}")] {
        assert!(!text.contains(SENTINEL), "{text}");
        assert!(text.contains("Secret(..)"), "{text}");
    }
}

// Catches a body type that ignores an unknown field (a typo of `secret_access_key` would
// silently register a connection with no secret) or insists on every optional one.
#[test]
fn a_put_body_takes_its_optional_fields_and_refuses_unknown_ones() {
    let put: ConnectionPut = serde_json::from_str(&put_body("")).unwrap();
    assert_eq!(put.region, None);
    assert_eq!(put.conditional_put, None);
    assert_eq!(put.min_put_rate, None);
    assert_eq!(put.expect_generation, None);
    assert_eq!(put.verify, None);
    let all = put_body(
        r#","region":"eu-1","conditional_put":true,"min_put_rate":4096,"expect_generation":7,"verify":false"#,
    );
    let put: ConnectionPut = serde_json::from_str(&all).unwrap();
    assert_eq!(put.region.as_deref(), Some("eu-1"));
    assert_eq!(put.conditional_put, Some(true));
    assert_eq!(put.min_put_rate.map(NonZeroU64::get), Some(4096));
    assert_eq!(put.expect_generation, Some(7));
    assert_eq!(put.verify, Some(false));
    let err = serde_json::from_str::<ConnectionPut>(&put_body(r#","secret":"x""#)).unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
    // The error of a wrong type does not quote the value.
    let err = serde_json::from_str::<ConnectionPut>(&put_body(r#","min_put_rate":0"#)).unwrap_err();
    assert!(!err.to_string().contains(SENTINEL), "{err}");
}

#[test]
fn a_binding_body_and_the_answers_round_trip() {
    let body = r#"{"connection":"c1","bucket":"b","prefix":"p/","expect_generation":3}"#;
    let bind: BindingPut = serde_json::from_str(body).unwrap();
    assert_eq!(bind.connection.as_str(), "c1");
    assert_eq!(bind.prefix, "p/");
    let bare: BindingPut = serde_json::from_str(r#"{"connection":"c1","bucket":"b"}"#).unwrap();
    assert_eq!((bare.prefix.as_str(), bare.expect_generation), ("", None));
    assert!(
        serde_json::from_str::<BindingPut>(r#"{"connection":"c1","bucket":"b","x":1}"#).is_err()
    );
    assert_eq!(
        serde_json::to_string(&bind).unwrap(),
        r#"{"connection":"c1","bucket":"b","prefix":"p/","expect_generation":3,"verify":null}"#
    );

    let entry = AuditEntry {
        seq: 1,
        at: 9,
        action: AuditAction::PutConnection,
        subject: "c1".to_owned(),
        outcome: AuditOutcome::Unchanged,
        generation: 4,
        detail: None,
    };
    let text = serde_json::to_string(&entry).unwrap();
    assert_eq!(
        text,
        r#"{"seq":1,"at":9,"action":"put_connection","subject":"c1","outcome":"unchanged","generation":4}"#
    );
    assert_eq!(serde_json::from_str::<AuditEntry>(&text).unwrap(), entry);
    for (action, name) in [
        (AuditAction::DeleteConnection, "delete_connection"),
        (AuditAction::PutBinding, "put_binding"),
        (AuditAction::DeleteBinding, "delete_binding"),
    ] {
        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            format!("\"{name}\"")
        );
    }
    for (outcome, name) in [
        (AuditOutcome::Created, "created"),
        (AuditOutcome::Changed, "changed"),
        (AuditOutcome::Deleted, "deleted"),
    ] {
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            format!("\"{name}\"")
        );
    }

    let answer = ConnectionPutAnswer {
        name: "c1".parse().unwrap(),
        generation: 2,
        changed: true,
        access_key_id: "AKID".to_owned(),
        secret_set: true,
        verified: Some(Verified {
            ok: true,
            put: true,
            get: true,
            delete: true,
            conditional_put: false,
        }),
    };
    let text = serde_json::to_string(&answer).unwrap();
    assert_eq!(
        serde_json::from_str::<ConnectionPutAnswer>(&text).unwrap(),
        answer
    );
    let info = ConnectionInfo {
        name: "c1".parse().unwrap(),
        endpoint: "http://s3:9000".to_owned(),
        region: String::new(),
        access_key_id: "AKID".to_owned(),
        conditional_put: false,
        min_put_rate: NonZeroU64::new(1),
        updated_generation: 2,
    };
    let text = serde_json::to_string(&info).unwrap();
    assert_eq!(serde_json::from_str::<ConnectionInfo>(&text).unwrap(), info);
    let info = BindingInfo {
        role: BindingRole::Cold,
        connection: "c1".parse().unwrap(),
        bucket: "b".to_owned(),
        prefix: String::new(),
        updated_generation: 3,
    };
    let text = serde_json::to_string(&info).unwrap();
    assert_eq!(serde_json::from_str::<BindingInfo>(&text).unwrap(), info);
    assert_eq!(
        Verified::default(),
        Verified {
            ok: false,
            put: false,
            get: false,
            delete: false,
            conditional_put: false
        }
    );
}

fn node(
    id: &str,
    state: MemberStanding,
    applied: Option<u64>,
    status: NodeStatus,
) -> NodeConvergence {
    NodeConvergence {
        id: id.to_owned(),
        state,
        applied_generation: applied,
        status,
        last_seen_secs: applied.map(|_| 1),
    }
}

// Catches a convergence that counts a draining or denied node as expected (a node leaving
// would hold `converged` false for ever), one that calls a node done at an older
// generation, one that lets an `unsupported` node count as expected or as applied, and a
// farm with no node to ask called converged (a restarted frontend that has heard from
// nobody must never say true).
#[test]
fn convergence_counts_only_active_nodes_at_the_current_generation() {
    use MemberStanding::{Active, Denied, Draining};
    use NodeStatus::{IdentityMismatch, Ok, Pending, ProbeFailed, Unsupported};

    let done = Convergence::of(
        5,
        vec![
            node("b", Active, Some(5), Ok),
            node("a", Active, Some(5), Ok),
            node("leaving", Draining, Some(2), Pending),
            node("gone", Denied, None, Pending),
        ],
    );
    assert!(done.converged, "{done:?}");
    assert_eq!((done.expected, done.applied, done.unsupported), (2, 2, 0));
    let ids: Vec<&str> = done.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "gone", "leaving"], "sorted by id");

    // `ok` at an older generation reads as pending, and the farm is not converged.
    let behind = Convergence::of(
        5,
        vec![
            node("a", Active, Some(5), Ok),
            node("b", Active, Some(4), Ok),
        ],
    );
    assert!(!behind.converged);
    assert_eq!((behind.expected, behind.applied), (2, 1));
    assert_eq!(behind.nodes[1].status, Pending);
    // A generation that went backwards (a restored state) is not "applied" either.
    let ahead = Convergence::of(3, vec![node("a", Active, Some(5), Ok)]);
    assert!(!ahead.converged);
    assert_eq!(ahead.nodes[0].status, Pending);

    // A failure status is kept as it is, and blocks convergence.
    for status in [ProbeFailed, IdentityMismatch] {
        let bad = Convergence::of(5, vec![node("a", Active, Some(5), status)]);
        assert!(!bad.converged);
        assert_eq!((bad.expected, bad.applied), (1, 0));
        assert_eq!(bad.nodes[0].status, status);
    }

    // An old build is named and counted apart: not expected, not applied, and it blocks.
    let skew = Convergence::of(
        5,
        vec![
            node("a", Active, Some(5), Ok),
            node("old", Active, None, Unsupported),
            node("old-leaving", Draining, None, Unsupported),
        ],
    );
    assert!(!skew.converged);
    assert_eq!((skew.expected, skew.applied, skew.unsupported), (1, 1, 1));

    // No node at all: nothing is known, so nothing is converged.
    let none = Convergence::of(5, Vec::new());
    assert!(!none.converged);
    assert_eq!((none.expected, none.applied, none.unsupported), (0, 0, 0));
}

// Catches a status or standing spelled differently on the wire (clients match on these
// strings), and an error body whose `verified` appears when it is absent.
#[test]
fn the_status_vocabulary_and_error_body_have_their_wire_spellings() {
    for (status, text) in [
        (NodeStatus::Ok, "ok"),
        (NodeStatus::Pending, "pending"),
        (NodeStatus::ProbeFailed, "probe_failed"),
        (NodeStatus::IdentityMismatch, "identity_mismatch"),
        (NodeStatus::Unsupported, "unsupported"),
    ] {
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            format!("\"{text}\"")
        );
    }
    for (standing, text) in [
        (MemberStanding::Active, "active"),
        (MemberStanding::Draining, "draining"),
        (MemberStanding::Denied, "denied"),
    ] {
        assert_eq!(
            serde_json::to_string(&standing).unwrap(),
            format!("\"{text}\"")
        );
    }
    let plain = ErrorBody {
        error: "no".to_owned(),
        code: "x".to_owned(),
        verified: None,
    };
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        r#"{"error":"no","code":"x"}"#
    );
    let with: ErrorBody =
        serde_json::from_str(r#"{"error":"no","code":"probe_failed","verified":{"ok":false,"put":true,"get":false,"delete":false,"conditional_put":false}}"#)
            .unwrap();
    assert!(with.verified.is_some_and(|v| v.put && !v.get));
}

// Catches a verify body that takes a field it ignores (a typo would pass silently), and
// answers that lose a field in a round trip.
#[test]
fn the_verify_body_and_the_small_answers_round_trip() {
    let empty: VerifyRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, VerifyRequest::default());
    let some: VerifyRequest = serde_json::from_str(r#"{"bucket":"b","prefix":"p/"}"#).unwrap();
    assert_eq!(
        (some.bucket.as_deref(), some.prefix.as_deref()),
        (Some("b"), Some("p/"))
    );
    assert!(serde_json::from_str::<VerifyRequest>(r#"{"bukket":"b"}"#).is_err());

    let answers = (
        VerifyAnswer {
            name: "c1".parse().unwrap(),
            verified: Verified::default(),
        },
        BindingPutAnswer {
            role: BindingRole::Cold,
            generation: 3,
            changed: false,
            verified: None,
        },
        Deleted { generation: 9 },
        AuditList {
            entries: Vec::new(),
        },
        ConnectionList {
            connections: Vec::new(),
        },
    );
    assert_eq!(
        serde_json::from_str::<VerifyAnswer>(&serde_json::to_string(&answers.0).unwrap()).unwrap(),
        answers.0
    );
    assert_eq!(
        serde_json::from_str::<BindingPutAnswer>(&serde_json::to_string(&answers.1).unwrap())
            .unwrap(),
        answers.1
    );
    assert_eq!(
        serde_json::from_str::<Deleted>(&serde_json::to_string(&answers.2).unwrap()).unwrap(),
        answers.2
    );
    assert_eq!(
        serde_json::from_str::<AuditList>(&serde_json::to_string(&answers.3).unwrap()).unwrap(),
        answers.3
    );
    assert_eq!(
        serde_json::from_str::<ConnectionList>(&serde_json::to_string(&answers.4).unwrap())
            .unwrap(),
        answers.4
    );
    let status = Status {
        generation: 1,
        connections: Vec::new(),
        bindings: Vec::new(),
        convergence: Convergence::of(
            1,
            vec![node("a", MemberStanding::Active, Some(1), NodeStatus::Ok)],
        ),
    };
    let back: Status = serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
    assert_eq!(back, status);
}

/// Catches an action this build does not know being unrepresentable on the wire: it is the
/// text `unknown (N)` with its numeric code, it reads back as the same action, and text that
/// names neither a known action nor such a code is still refused.
#[test]
fn an_unknown_action_is_text_with_its_code_and_reads_back() {
    let unknown = AuditAction::Unknown(9);
    assert_eq!(serde_json::to_string(&unknown).unwrap(), r#""unknown (9)""#);
    assert_eq!(
        serde_json::from_str::<AuditAction>(r#""unknown (9)""#).unwrap(),
        unknown
    );
    assert_eq!(
        serde_json::to_string(&AuditAction::Repoint).unwrap(),
        r#""repoint""#
    );
    for bad in [
        r#""bogus""#,
        r#""unknown (x)""#,
        r#""unknown 9""#,
        r#""unknown (9""#,
    ] {
        assert!(serde_json::from_str::<AuditAction>(bad).is_err(), "{bad}");
    }
}
