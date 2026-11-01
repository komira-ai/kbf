//! The rules the rows keep: bindings, identity, deletion, the fence, unknown fields,
//! unreadable rows.

use std::num::NonZeroUsize;

use kbf_state::Cond;
use kbf_types::object_store_api::{AuditAction, BindingRole};
use prost::Message as _;

use super::*;
use crate::object_store::rows;

fn audit_all(reg: &Registry) -> Vec<kbf_types::object_store_api::AuditEntry> {
    reg.audit(NonZeroUsize::new(100).unwrap()).unwrap()
}

/// The newest audit row is exactly this action, subject, outcome and generation.
fn newest_is(
    reg: &Registry,
    action: AuditAction,
    subject: &str,
    outcome: AuditOutcome,
    generation: u64,
) {
    let last = &audit_all(reg)[0];
    assert_eq!(
        (
            last.action,
            last.subject.as_str(),
            last.outcome,
            last.generation
        ),
        (action, subject, outcome, generation)
    );
}

// Catches a binding to a connection that does not exist, a first bind that is not
// created, a repeat that bumps the generation, and a bind that loses its audit row.
#[test]
fn a_binding_needs_its_connection_and_a_repeat_is_unchanged() {
    let store = as_store(&fake());
    let root = scratch("bind");
    let reg = open(&store, &root.join("s"));
    let err = reg
        .put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::NoSuchConnection), "{err}");
    assert_eq!(err.to_string(), "no such connection");
    reg.put_connection(&name("cold"), &put("s")).unwrap();
    let made = reg
        .put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();
    assert_eq!((made.generation, made.outcome), (2, AuditOutcome::Created));
    let again = reg
        .put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();
    assert_eq!(
        (again.generation, again.outcome),
        (2, AuditOutcome::Unchanged)
    );
    let bindings = reg.bindings().unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].role, BindingRole::Cold);
    assert_eq!(
        (bindings[0].bucket.as_str(), bindings[0].prefix.as_str()),
        ("b", "farm/")
    );
    assert_eq!(bindings[0].updated_generation, 2);
    let actions: Vec<_> = audit_all(&reg)
        .iter()
        .map(|a| (a.action, a.outcome))
        .collect();
    assert_eq!(
        actions,
        [
            (AuditAction::PutBinding, AuditOutcome::Unchanged),
            (AuditAction::PutBinding, AuditOutcome::Created),
            (AuditAction::PutConnection, AuditOutcome::Created),
        ]
    );
    let mut stale = bind("cold", "b");
    stale.expect_generation = Some(1);
    assert!(matches!(
        reg.put_binding(BindingRole::Cold, &stale),
        Err(RegistryError::GenerationMismatch { current: 2 })
    ));
}

// Catches the identity rule failing in any direction: a bound endpoint, bucket or prefix
// may not move (nodes would silently read a different store); credentials may.
#[test]
fn a_bound_stores_identity_cannot_change_but_its_credentials_can() {
    let store = as_store(&fake());
    let root = scratch("identity");
    let reg = open(&store, &root.join("s"));
    reg.put_connection(&name("cold"), &put("s1")).unwrap();
    reg.put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();

    // Credentials, region and flags change freely.
    let mut creds = put("s2");
    creds.access_key_id = "AKID2".to_owned();
    creds.region = Some("eu-1".to_owned());
    creds.conditional_put = Some(true);
    assert_eq!(
        reg.put_connection(&name("cold"), &creds)
            .unwrap()
            .generation,
        3
    );

    // The endpoint of a bound connection does not.
    let mut moved = put("s2");
    moved.endpoint = "http://elsewhere:9000".to_owned();
    let err = reg.put_connection(&name("cold"), &moved).unwrap_err();
    assert!(matches!(err, RegistryError::IdentityChange), "{err}");
    assert!(err.to_string().contains("cannot change"), "{err}");

    // Neither does the bucket or the prefix of the binding.
    let err = reg
        .put_binding(BindingRole::Cold, &bind("cold", "other"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::IdentityChange), "{err}");
    let mut prefixed = bind("cold", "b");
    prefixed.prefix = "else/".to_owned();
    assert!(matches!(
        reg.put_binding(BindingRole::Cold, &prefixed),
        Err(RegistryError::IdentityChange)
    ));

    // A second connection to the same endpoint can take the role over (a new key pair);
    // one at another endpoint cannot.
    reg.put_connection(&name("cold2"), &put("t")).unwrap();
    let mut elsewhere = put("t");
    elsewhere.endpoint = "http://elsewhere:9000".to_owned();
    reg.put_connection(&name("far"), &elsewhere).unwrap();
    let err = reg
        .put_binding(BindingRole::Cold, &bind("far", "b"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::IdentityChange), "{err}");
    reg.put_binding(BindingRole::Cold, &bind("cold2", "b"))
        .unwrap();
    assert_eq!(reg.bindings().unwrap()[0].connection.as_str(), "cold2");
    // Nothing of the refused calls was written.
    assert_eq!(
        reg.connection(&name("cold")).unwrap().unwrap().endpoint,
        "http://s3.example:9000"
    );

    // An unbound connection moves freely.
    reg.put_connection(&name("far"), &put("t")).unwrap();
    let mut back = put("t");
    back.endpoint = "http://third:9000".to_owned();
    reg.put_connection(&name("far"), &back).unwrap();
}

// Catches a delete of a bound connection (nodes would be handed a dangling binding), a
// delete that leaves its file, and a binding delete that leaves the row.
#[test]
fn deletes_refuse_a_bound_connection_and_remove_the_rest() {
    let store = as_store(&fake());
    let root = scratch("delete");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    assert!(matches!(
        reg.delete_connection(&name("cold")),
        Err(RegistryError::NoSuchConnection)
    ));
    assert!(matches!(
        reg.delete_binding(BindingRole::Cold),
        Err(RegistryError::NoSuchBinding)
    ));
    assert_eq!(RegistryError::NoSuchBinding.to_string(), "no such binding");
    let made = reg.put_connection(&name("cold"), &put("s")).unwrap();
    newest_is(
        &reg,
        AuditAction::PutConnection,
        "cold",
        made.outcome,
        made.generation,
    );
    let bound = reg
        .put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();
    newest_is(
        &reg,
        AuditAction::PutBinding,
        "cold",
        bound.outcome,
        bound.generation,
    );
    let err = reg.delete_connection(&name("cold")).unwrap_err();
    assert!(matches!(err, RegistryError::InUse), "{err}");
    assert_eq!(err.to_string(), "a binding uses this connection");
    assert_eq!(listing(&dir).len(), 1);

    let unbound = reg.delete_binding(BindingRole::Cold).unwrap();
    assert_eq!(
        (unbound.generation, unbound.outcome),
        (3, AuditOutcome::Deleted)
    );
    newest_is(
        &reg,
        AuditAction::DeleteBinding,
        "cold",
        AuditOutcome::Deleted,
        3,
    );
    assert!(reg.bindings().unwrap().is_empty());
    let gone = reg.delete_connection(&name("cold")).unwrap();
    assert_eq!((gone.generation, gone.outcome), (4, AuditOutcome::Deleted));
    newest_is(
        &reg,
        AuditAction::DeleteConnection,
        "cold",
        AuditOutcome::Deleted,
        4,
    );
    assert!(reg.connections().unwrap().is_empty());
    assert!(
        listing(&dir).is_empty(),
        "the secret file went with the row"
    );
}

// Catches a registry that assumes it is the only frontend: two children on one store
// and one directory (old and new side by side) each write, each sees the other's
// rows, and the generation advances by exactly one per change (compare-and-set).
#[test]
fn two_frontend_children_share_one_store_and_one_directory() {
    let store = as_store(&fake());
    let root = scratch("children");
    let dir = root.join("s");
    let old = open(&store, &dir);
    let new = open_now(Arc::clone(&store), &dir, Counting::from(100)).unwrap();
    assert_eq!(
        old.put_connection(&name("cold"), &put("one"))
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        new.put_connection(&name("cold"), &put("two"))
            .unwrap()
            .generation,
        2
    );
    assert_eq!(old.secret(&name("cold")).unwrap().expose(), "two");
    assert_eq!(new.generation().unwrap(), 2);
    new.put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();
    assert!(matches!(
        old.delete_connection(&name("cold")),
        Err(RegistryError::InUse)
    ));
    assert_eq!(listing(&dir).len(), 1);
}

// Catches a sweep that removes the file another child has renamed into place and not yet
// named in a row (it would leave that child's row dangling), and a reader that fails
// when the other child rotated between its row read and its file read.
#[test]
fn a_young_unreferenced_file_survives_the_sweep() {
    let store = as_store(&fake());
    let root = scratch("grace");
    let dir = root.join("s");
    let reg = Registry::open(Arc::clone(&store), &dir, Counting::arc()).unwrap();
    let young = file_of(&dir, "cold", 5);
    std::fs::write(&young, "x").unwrap();
    std::fs::write(dir.join(".tmp-9-9"), "x").unwrap();
    assert_eq!(reg.sweep().unwrap(), 0);
    assert_eq!(listing(&dir).len(), 2);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    std::fs::File::options()
        .write(true)
        .open(&young)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(reg.sweep().unwrap(), 1, "only the aged file goes");
    assert_eq!(listing(&dir), [".tmp-9-9"]);
}

// Catches a reader that reports Missing when the other child rotated between the row
// read and the file read (it removes the old version after its commit).
#[test]
fn a_reader_survives_the_other_child_rotating_under_it() {
    let store = as_store(&fake());
    let root = scratch("rotate-under");
    let dir = root.join("s");
    let reader = open(&store, &dir);
    let writer = open_now(Arc::clone(&store), &dir, Counting::from(100)).unwrap();
    writer.put_connection(&name("cold"), &put("one")).unwrap();
    // The writer rotates between the reader's row read and its first file read.
    let writer = Arc::new(writer);
    let other = Arc::clone(&writer);
    let once = std::sync::atomic::AtomicBool::new(true);
    reader.set_failpoint(move |step| {
        if step == crate::object_store::Step::ReadFile
            && once.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            other.put_connection(&name("cold"), &put("two")).unwrap();
        }
    });
    assert_eq!(reader.secret(&name("cold")).unwrap().expose(), "two");
}

/// What a newer binary writes: the same fields and one this binary does not know.
#[derive(Clone, PartialEq, prost::Message)]
struct NewerConn {
    #[prost(string, tag = "1")]
    endpoint: String,
    #[prost(string, tag = "3")]
    access_key_id: String,
    #[prost(uint64, tag = "6")]
    secret_version: u64,
    #[prost(uint64, tag = "7")]
    updated_generation: u64,
    #[prost(string, tag = "99")]
    future: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NewerBind {
    #[prost(string, tag = "1")]
    connection: String,
    #[prost(string, tag = "2")]
    bucket: String,
    #[prost(string, tag = "99")]
    future: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NewerGen {
    #[prost(uint64, tag = "1")]
    generation: u64,
    #[prost(uint64, tag = "2")]
    audit_seq: u64,
    #[prost(string, tag = "99")]
    future: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NewerAudit {
    #[prost(uint64, tag = "1")]
    at: u64,
    #[prost(int32, tag = "2")]
    action: i32,
    #[prost(string, tag = "3")]
    subject: String,
    #[prost(int32, tag = "4")]
    outcome: i32,
    #[prost(uint64, tag = "5")]
    generation: u64,
    #[prost(string, tag = "99")]
    future: String,
}

fn plant(store: &Arc<dyn Store>, key: &str, value: Vec<u8>) {
    let version = store
        .get(key)
        .unwrap()
        .map_or(Cond::Absent, |v| Cond::Version(v.version));
    store
        .txn(Txn::new().put(key, value, version))
        .unwrap()
        .unwrap();
}

// Catches a row reader that refuses a row with a field it does not know: a handover runs
// an old and a new binary against one store, and the old one must keep working.
#[test]
fn rows_with_fields_from_a_newer_binary_are_read_and_rewritten() {
    let store = as_store(&fake());
    let root = scratch("newer");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    // A secret file the planted row can name.
    reg.put_connection(&name("seed"), &put("planted")).unwrap();
    std::fs::rename(file_of(&dir, "seed", 1), file_of(&dir, "cold", 7)).unwrap();
    reg.delete_connection(&name("seed")).unwrap();
    plant(
        &store,
        "objstore/conn/cold",
        NewerConn {
            endpoint: "http://s3.example:9000".into(),
            access_key_id: "AKID".into(),
            secret_version: 7,
            updated_generation: 5,
            future: "later".into(),
        }
        .encode_to_vec(),
    );
    plant(
        &store,
        "objstore/gen",
        NewerGen {
            generation: 5,
            audit_seq: 9,
            future: "x".into(),
        }
        .encode_to_vec(),
    );
    plant(
        &store,
        "objstore/bind/cold",
        NewerBind {
            connection: "cold".into(),
            bucket: "b".into(),
            future: "y".into(),
        }
        .encode_to_vec(),
    );
    plant(
        &store,
        &rows::audit_key(9),
        NewerAudit {
            at: 3,
            action: 3,
            subject: "cold".into(),
            outcome: 1,
            generation: 5,
            future: "z".into(),
        }
        .encode_to_vec(),
    );

    let reg = open(&store, &dir);
    assert_eq!(reg.generation().unwrap(), 5);
    let info = reg.connection(&name("cold")).unwrap().unwrap();
    assert_eq!(
        (info.updated_generation, info.endpoint.as_str()),
        (5, "http://s3.example:9000")
    );
    assert_eq!(reg.bindings().unwrap()[0].bucket, "b");
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), "planted");
    let newest = &audit_all(&reg)[0];
    assert_eq!(
        (newest.seq, newest.at, newest.action),
        (9, 3, AuditAction::PutBinding)
    );
    // And a write on top of them goes through.
    let mut changed = put("planted");
    changed.region = Some("eu-1".to_owned());
    let written = reg.put_connection(&name("cold"), &changed).unwrap();
    assert_eq!(written.generation, 6);
    assert_eq!(audit_all(&reg)[0].seq, 10);
}

// Catches a reader that turns an unreadable row into a default value or a panic: each
// family reports itself, by name.
#[test]
fn unreadable_rows_are_reported_by_family() {
    let garbage = vec![0xff, 0xff, 0xff];
    let cases: [(&str, &str); 4] = [
        ("objstore/gen", "gen"),
        ("objstore/conn/cold", "conn"),
        ("objstore/bind/cold", "bind"),
        ("objstore/audit/00000000000000000001", "audit"),
    ];
    for (key, family) in cases {
        let store = as_store(&fake());
        let root = scratch("corrupt");
        let reg = open(&store, &root.join("s"));
        reg.put_connection(&name("cold"), &put("s")).unwrap();
        reg.put_binding(BindingRole::Cold, &bind("cold", "b"))
            .unwrap();
        plant(&store, key, garbage.clone());
        let results = [
            reg.generation().map(|_| ()),
            reg.connections().map(|_| ()),
            reg.bindings().map(|_| ()),
            reg.audit(NonZeroUsize::new(5).unwrap()).map(|_| ()),
        ];
        let reported: Vec<_> = results
            .iter()
            .filter_map(|r| match r {
                Err(RegistryError::Corrupt(f)) => Some(*f),
                _ => None,
            })
            .collect();
        assert!(reported.contains(&family), "{key}: {reported:?}");
    }
    assert_eq!(
        RegistryError::Corrupt("conn").to_string(),
        "the conn rows are not readable"
    );
}

// Catches keys and codes the registry would read as something else: a connection key that
// is not a name, a role key that is not a role, an audit key that is not a number, and an
// audit row with an action or outcome this binary does not know.
#[test]
fn rows_under_odd_keys_or_codes_are_corrupt_not_skipped() {
    let ok_audit = |action, outcome| {
        rows::AuditRow {
            at: 1,
            action,
            subject: "x".into(),
            outcome,
            generation: 1,
            detail: String::new(),
        }
        .encode_to_vec()
    };
    let ok_conn = rows::ConnRow {
        endpoint: "http://h".into(),
        region: String::new(),
        access_key_id: "k".into(),
        conditional_put: false,
        min_put_rate: 0,
        secret_version: 1,
        updated_generation: 1,
    }
    .encode_to_vec();
    let ok_bind = |connection: &str| {
        rows::BindRow {
            connection: connection.into(),
            bucket: "b".into(),
            prefix: String::new(),
            updated_generation: 1,
        }
        .encode_to_vec()
    };
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("objstore/conn/Bad Name", ok_conn, "conn"),
        ("objstore/bind/history", ok_bind("cold"), "bind"),
        ("objstore/bind/cold", ok_bind("Not A Name"), "bind"),
        ("objstore/audit/not-a-number", ok_audit(1, 1), "audit"),
        (
            "objstore/audit/00000000000000000001",
            ok_audit(-1, 1),
            "audit",
        ),
        (
            "objstore/audit/00000000000000000001",
            ok_audit(1, 9),
            "audit",
        ),
    ];
    for (key, value, family) in cases {
        let store = as_store(&fake());
        let root = scratch("odd");
        let reg = open(&store, &root.join("s"));
        plant(&store, key, value);
        let err = if family == "conn" {
            reg.connections().unwrap_err()
        } else if family == "bind" {
            reg.bindings().unwrap_err()
        } else {
            reg.audit(NonZeroUsize::new(5).unwrap()).unwrap_err()
        };
        assert!(
            matches!(err, RegistryError::Corrupt(f) if f == family),
            "{key}: {err}"
        );
    }
}

// Catches a rebind over a binding row the registry cannot read, or one that names a
// connection that is not a name: each is reported as a corrupt bind row, not skipped or
// taken for "no binding yet".
#[test]
fn a_rebind_over_an_unreadable_binding_row_is_corrupt() {
    for planted in [
        vec![0xff, 0xff, 0xff],
        rows::BindRow {
            connection: "Not A Name".into(),
            bucket: "b".into(),
            prefix: String::new(),
            updated_generation: 1,
        }
        .encode_to_vec(),
    ] {
        let store = as_store(&fake());
        let root = scratch("rebind-corrupt");
        let reg = open(&store, &root.join("s"));
        reg.put_connection(&name("cold"), &put("s")).unwrap();
        plant(&store, "objstore/bind/cold", planted);
        let err = reg
            .put_binding(BindingRole::Cold, &bind("cold", "b"))
            .unwrap_err();
        assert!(matches!(err, RegistryError::Corrupt("bind")), "{err}");
    }
}

// Catches the audit codes drifting: each action and outcome must survive the row.
#[test]
fn every_audit_code_round_trips() {
    for action in [
        AuditAction::PutConnection,
        AuditAction::DeleteConnection,
        AuditAction::PutBinding,
        AuditAction::DeleteBinding,
    ] {
        assert_eq!(rows::action_of(rows::action_code(action)).unwrap(), action);
    }
    for outcome in [
        AuditOutcome::Created,
        AuditOutcome::Changed,
        AuditOutcome::Unchanged,
        AuditOutcome::Deleted,
    ] {
        assert_eq!(
            rows::outcome_of(rows::outcome_code(outcome)).unwrap(),
            outcome
        );
    }
}

// Catches audit keys that sort wrongly (a sequence of 10 before 9) and a limit ignored.
#[test]
fn the_audit_log_is_newest_first_and_bounded() {
    let store = as_store(&fake());
    let root = scratch("audit");
    let reg = open(&store, &root.join("s"));
    for round in 0..12 {
        let mut body = put("s");
        body.region = Some(format!("r{round}"));
        reg.put_connection(&name("cold"), &body).unwrap();
    }
    let all: Vec<u64> = audit_all(&reg).iter().map(|a| a.seq).collect();
    assert_eq!(all, (1..=12).rev().collect::<Vec<_>>());
    let newest: Vec<u64> = reg
        .audit(NonZeroUsize::new(3).unwrap())
        .unwrap()
        .iter()
        .map(|a| a.seq)
        .collect();
    assert_eq!(newest, [12, 11, 10]);
}
