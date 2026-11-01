//! `Registry::repoint`: what it writes together, and what it refuses.

use kbf_types::object_store_api::{AuditAction, BindingRole, StoreIdentity};

use super::*;
use crate::object_store::rows::{self, BindRow, ConnRow};

fn identity(endpoint: &str, bucket: &str, prefix: &str) -> StoreIdentity {
    StoreIdentity {
        endpoint: endpoint.to_owned(),
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
    }
}

fn audit_all(reg: &Registry) -> Vec<kbf_types::object_store_api::AuditEntry> {
    reg.audit(std::num::NonZeroUsize::new(100).unwrap())
        .unwrap()
}

fn bound(reg: &Registry) {
    reg.put_connection(&name("cold"), &put("s")).unwrap(); // generation 1
    reg.put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap(); // generation 2
}

// Catches a repoint that writes the connection and the binding in separate steps (a crash
// between them names an endpoint with the wrong bucket), that bumps the generation more than
// once or not at all, that stamps the rows with another generation than the one it made, or
// whose audit row lacks the identity before and after.
#[test]
fn a_repoint_writes_both_rows_the_generation_and_the_audit_together() {
    use prost::Message as _;
    let store = as_store(&fake());
    let root = scratch("repoint");
    let reg = open(&store, &root.join("s"));
    bound(&reg);
    let to = identity("http://127.0.0.1:30900", "av2dev1", "kbf/");
    let (written, from) = reg.repoint(&name("cold"), &to, 2).unwrap();
    assert_eq!(
        (written.generation, written.outcome),
        (3, AuditOutcome::Changed)
    );
    assert_eq!(from, identity("http://s3.example:9000", "b", "farm/"));

    let conn = store.get(&rows::conn_key(&name("cold"))).unwrap().unwrap();
    let conn = ConnRow::decode(conn.value.as_slice()).unwrap();
    assert_eq!(
        (conn.endpoint.as_str(), conn.updated_generation),
        (to.endpoint.as_str(), 3)
    );
    assert_eq!(conn.access_key_id, "AKID");
    let bind = store
        .get(&rows::bind_key(BindingRole::Cold))
        .unwrap()
        .unwrap();
    let bind = BindRow::decode(bind.value.as_slice()).unwrap();
    assert_eq!(
        (
            bind.bucket.as_str(),
            bind.prefix.as_str(),
            bind.updated_generation
        ),
        ("av2dev1", "kbf/", 3)
    );
    assert_eq!(reg.generation().unwrap(), 3);
    let newest = &audit_all(&reg)[0];
    assert_eq!(newest.action, AuditAction::Repoint);
    assert_eq!(newest.generation, 3);
    assert_eq!(
        newest.detail.as_deref(),
        Some("http://s3.example:9000 b/farm/ -> http://127.0.0.1:30900 av2dev1/kbf/")
    );

    let (again, was) = reg.repoint(&name("cold"), &to, 3).unwrap();
    assert_eq!(
        (again.generation, again.outcome),
        (3, AuditOutcome::Unchanged)
    );
    assert_eq!(was, to);
}

// Catches a repoint that moves a connection no role names, one that does not exist, or one
// that answers a stale generation; each writes nothing.
#[test]
fn a_repoint_refuses_a_connection_the_role_does_not_name_and_a_stale_generation() {
    let store = as_store(&fake());
    let root = scratch("repoint-refused");
    let reg = open(&store, &root.join("s"));
    let to = identity("http://127.0.0.1:30900", "av2dev1", "kbf/");
    let err = reg.repoint(&name("cold"), &to, 0).unwrap_err();
    assert!(matches!(err, RegistryError::NoSuchConnection), "{err}");

    reg.put_connection(&name("cold"), &put("s")).unwrap();
    let err = reg.repoint(&name("cold"), &to, 1).unwrap_err();
    assert!(matches!(err, RegistryError::NotBound), "{err}");

    reg.put_connection(&name("spare"), &put("s")).unwrap();
    reg.put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap(); // generation 3
    let err = reg.repoint(&name("spare"), &to, 3).unwrap_err();
    assert!(matches!(err, RegistryError::NotBound), "{err}");
    let err = reg.repoint(&name("cold"), &to, 2).unwrap_err();
    assert!(
        matches!(err, RegistryError::GenerationMismatch { current: 3 }),
        "{err}"
    );
    assert_eq!(reg.generation().unwrap(), 3);
    assert_eq!(audit_all(&reg).len(), 3, "put, put, bind");
}
