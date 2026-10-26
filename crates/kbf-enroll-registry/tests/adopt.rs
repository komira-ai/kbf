//! Adopting a node provisioned before enrollment: its row, its refusals, and what renewal,
//! revocation and the listing do with a row that holds no certificate. Every test names the
//! behaviour it holds.

mod common;

use common::{Env, Mode, Req, Wrap, fake, make_token, node, roles, spec};
use kbf_enroll_registry::{
    ADOPTED, NodeState, RegistryError, RenewCsr, RenewRequest, adopt_node, deny_rows, list_nodes,
    renew_txn, revoke_node,
};
use kbf_state::Op;
use kbf_types::{
    RefusalReason,
    Role::{Daemon, Storage, Updater},
};

fn keys(env: &Env, prefix: &str) -> Vec<String> {
    env.store
        .scan_prefix(prefix)
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

// Catches an adoption that writes anything but the node row and the address row (a serial, a
// key, a request row), that loses the roles (the listing derives them from certificates an
// adopted row does not have), or that shows the row as an enrolled one.
#[test]
fn adopt_writes_the_node_and_address_rows_and_the_list_shows_it_as_adopted() {
    let env = fake();
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let info = adopt_node(
        &wrap,
        &node("s1"),
        roles(&[Storage, Daemon]),
        "S1.Example:7000",
    )
    .expect("adopt");
    let txns = wrap.recorded();
    assert_eq!(txns.len(), 1, "one transaction");
    let mut put: Vec<String> = txns[0]
        .ops()
        .iter()
        .filter_map(|op| match op {
            Op::Put { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    put.sort();
    assert_eq!(
        put,
        vec!["enroll/adv/s1.example:7000", "enroll/node/s1"],
        "no serial, key or request row"
    );
    assert_eq!(list_nodes(&*env.store).expect("list"), vec![info.clone()]);
    assert_eq!(info.id, node("s1"));
    assert_eq!(info.roles, roles(&[Storage, Daemon]));
    assert_eq!(info.state, NodeState::Active);
    assert_eq!(info.advertise, "s1.example:7000");
    assert_eq!(
        (info.token_id.as_str(), info.token_label.as_str()),
        (ADOPTED, ADOPTED)
    );
    assert_eq!(info.created_at, env.now());
    assert_eq!(
        (info.last_renewed_at, info.not_after, info.revoked_at),
        (None, 0, None)
    );
    assert!(keys(&env, "enroll/serial/").is_empty());
    assert!(keys(&env, "enroll/key/").is_empty());
}

// Catches a daemon-only adoption that stores or demands an address, and a storage adoption
// that takes none or a malformed one: enrollment's rule is that storage names `host:port`
// and no other node does.
#[test]
fn adopt_takes_enrollments_rule_for_the_advertise_address() {
    let env = fake();
    let bad = |text: &str, set| adopt_node(&*env.store, &node("x1"), roles(set), text);
    let invalid = |r: Result<_, RegistryError>| matches!(r, Err(RegistryError::Invalid(_)));
    assert!(invalid(bad("", &[Storage])));
    assert!(invalid(bad("host", &[Storage])));
    assert!(invalid(bad("host:0", &[Storage])));
    assert!(invalid(bad("host:70000", &[Storage])));
    assert!(invalid(bad("Bad_Name:7000", &[Storage])));
    assert!(invalid(bad("h.example:7000", &[Daemon])));
    assert!(invalid(bad("", &[Updater])));
    assert!(list_nodes(&*env.store).expect("list").is_empty());
    let d = adopt_node(&*env.store, &node("d1"), roles(&[Daemon]), "").expect("daemon");
    assert_eq!(d.advertise, "");
    assert!(keys(&env, "enroll/adv/").is_empty());
}

// Catches an adoption that overwrites a node the registry holds (enrolled, adopted or
// revoked: a revoked id is burned) and one that takes an address another node holds, and
// an adoption that leaves a row behind when it refuses.
#[test]
fn adopt_refuses_an_existing_id_and_an_address_another_node_holds() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 5));
    Req::new(1, "n1", &[Storage])
        .enroll(&env, 1)
        .expect("enroll");
    adopt_node(
        &*env.store,
        &node("a1"),
        roles(&[Storage]),
        "a1.example:7000",
    )
    .expect("adopt");
    let before = list_nodes(&*env.store).expect("list");
    let exists = |r: Result<_, RegistryError>| matches!(r, Err(RegistryError::AlreadyExists(_)));
    assert!(exists(adopt_node(
        &*env.store,
        &node("n1"),
        roles(&[Daemon]),
        ""
    )));
    assert!(exists(adopt_node(
        &*env.store,
        &node("a1"),
        roles(&[Daemon]),
        ""
    )));
    assert!(exists(adopt_node(
        &*env.store,
        &node("a2"),
        roles(&[Storage]),
        "a1.example:7000"
    )));
    assert!(exists(adopt_node(
        &*env.store,
        &node("a2"),
        roles(&[Storage]),
        "A1.example:7000"
    )));
    assert!(exists(adopt_node(
        &*env.store,
        &node("a2"),
        roles(&[Storage]),
        "n1.example:7000"
    )));
    revoke_node(&*env.store, &node("a1"), "op").expect("revoke");
    assert!(exists(adopt_node(
        &*env.store,
        &node("a1"),
        roles(&[Daemon]),
        ""
    )));
    assert_eq!(list_nodes(&*env.store).expect("list").len(), before.len());
    // The revoked node's address is free again, as for an enrolled node.
    adopt_node(
        &*env.store,
        &node("a2"),
        roles(&[Storage]),
        "a1.example:7000",
    )
    .expect("free");
}

// Catches an adoption whose failures are lost: a store that is down or a busy one must be an
// error, not an unwritten success.
#[test]
fn adopt_reports_store_trouble() {
    let env = fake();
    let go = |mode| {
        adopt_node(
            &Wrap::new(env.store.clone(), mode),
            &node("s1"),
            roles(&[Storage]),
            "s1.example:7000",
        )
    };
    assert!(matches!(go(Mode::FailGet), Err(RegistryError::Store(_))));
    assert!(matches!(go(Mode::FailTxn), Err(RegistryError::Store(_))));
    assert_eq!(go(Mode::Conflict), Err(RegistryError::Contended));
    assert!(list_nodes(&*env.store).expect("list").is_empty());
}

fn renew_adopted(env: &Env, id: &str, role: kbf_types::Role) -> Result<(), RefusalReason> {
    let der = b"csr".to_vec();
    let csrs = [RenewCsr {
        role,
        csr_der: &der,
        spki_sha256: [7; 32],
    }];
    let id = node(id);
    renew_txn(
        &*env.store,
        &RenewRequest {
            node_id: &id,
            csrs: &csrs,
            issued_at: env.now(),
        },
        &mut || panic!("the signer ran for an adopted node"),
    )
    .map(|_| ())
}

// Catches a renewal of a row with no certificate that panics, signs, or answers with a
// reason that says something else (an adopted row is "expired" to a time rule that reads
// no certificates): it is refused as a node that holds nothing to renew, and the row is
// left as it was.
#[test]
fn an_adopted_node_never_renews() {
    let env = fake();
    adopt_node(
        &*env.store,
        &node("s1"),
        roles(&[Storage]),
        "s1.example:7000",
    )
    .expect("adopt");
    let before = list_nodes(&*env.store).expect("list");
    env.advance(100 * 24 * 3600);
    assert_eq!(
        renew_adopted(&env, "s1", Storage),
        Err(RefusalReason::UnknownNode)
    );
    assert_eq!(list_nodes(&*env.store).expect("list"), before);
}

// Catches a revoke that cannot handle a row without certificates: it must burn the id, free
// the address, write the one deny row for the node id and none for serials or keys the row
// never had, and after it a renewal says the node is revoked.
#[test]
fn revoking_an_adopted_node_denies_its_id_alone() {
    let env = fake();
    adopt_node(
        &*env.store,
        &node("s1"),
        roles(&[Storage]),
        "s1.example:7000",
    )
    .expect("adopt");
    env.advance(60);
    let entries = revoke_node(&*env.store, &node("s1"), "op").expect("revoke");
    assert_eq!(
        entries.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["node s1"]
    );
    let rows = deny_rows(&*env.store).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].reason.as_str(), rows[0].at),
        ("revoked", env.now())
    );
    let n = list_nodes(&*env.store).expect("list").remove(0);
    assert_eq!(
        (n.state, n.revoked_at),
        (NodeState::Revoked, Some(env.now()))
    );
    assert_eq!((n.token_id.as_str(), n.roles), (ADOPTED, roles(&[Storage])));
    assert!(keys(&env, "enroll/adv/").is_empty());
    assert_eq!(
        renew_adopted(&env, "s1", Storage),
        Err(RefusalReason::NodeRevoked)
    );
    assert_eq!(
        revoke_node(&*env.store, &node("s1"), "op"),
        Err(RegistryError::Invalid("the node is already revoked"))
    );
}

// Catches a token that replaces an adopted node failing on its missing certificates: a
// token with `replace` enrolls over an adopted row, which becomes an enrolled one.
#[test]
fn a_replacing_token_enrolls_over_an_adopted_node() {
    let env = fake();
    adopt_node(
        &*env.store,
        &node("n1"),
        roles(&[Storage]),
        "n1.example:7000",
    )
    .expect("adopt");
    let mut s = spec(&[Storage], 5);
    s.replace = true;
    make_token(&env, 1, &s);
    Req::new(1, "n1", &[Storage])
        .enroll(&env, 1)
        .expect("enroll");
    let n = list_nodes(&*env.store).expect("list").remove(0);
    assert!(!n.token_id.is_empty());
    assert_ne!(n.token_id, ADOPTED);
    assert_ne!(n.not_after, 0);
}
