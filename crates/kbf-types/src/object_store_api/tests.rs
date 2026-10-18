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
        r#"{"connection":"c1","bucket":"b","prefix":"p/","expect_generation":3}"#
    );

    let entry = AuditEntry {
        seq: 1,
        at: 9,
        action: AuditAction::PutConnection,
        subject: "c1".to_owned(),
        outcome: AuditOutcome::Unchanged,
        generation: 4,
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
