//! Revocation, the listing, the deny rows and the CA generation.
//! Every test names the defect it catches.

mod common;

use common::{DAY, Mode, Req, Wrap, fake, issue, make_token, node, serial_of, spec};
use kbf_enroll_registry::{
    ATTEMPTS, NodeState, RegistryError, bump_ca_generation, ca_generation, deny_rows, list_nodes,
    revoke_node,
};
use kbf_state::{Cond, Op, Txn};
use kbf_types::Role::{Daemon, Storage};

fn denied(env: &common::Env) -> Vec<String> {
    let mut out: Vec<String> = deny_rows(&*env.store)
        .expect("deny")
        .into_iter()
        .map(|d| d.entry.to_string())
        .collect();
    out.sort();
    out
}

fn spki_text(seed: u8, role: kbf_types::Role) -> String {
    issue(&[role], seed, 1000)[0]
        .spki_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Catches a revocation that misses a deny row (node, each unexpired serial, each key), or
// writes them apart from the registry row: one transaction holds the state change and all
// the rows, and the entries it returns are the ones written.
#[test]
fn a_revoke_writes_the_registry_row_and_every_deny_row_in_one_transaction() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 5));
    Req::new(1, "n1", &[Storage, Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    env.advance(60);
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let entries = revoke_node(&wrap, &node("n1"), "operator").expect("revoke");
    let txns = wrap.recorded();
    assert_eq!(txns.len(), 1);
    let mut put_keys: Vec<String> = txns[0]
        .ops()
        .iter()
        .filter_map(|op| match op {
            Op::Put { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    put_keys.sort();
    let mut want = vec![
        "enroll/node/n1".to_owned(),
        "deny/node/n1".to_owned(),
        format!("deny/serial/{}", serial_of(1, Storage)),
        format!("deny/serial/{}", serial_of(1, Daemon)),
        format!("deny/spki-sha256/{}", spki_text(1, Storage)),
        format!("deny/spki-sha256/{}", spki_text(1, Daemon)),
    ];
    want.sort();
    assert_eq!(put_keys, want);
    assert_eq!(entries.len(), 5);
    let mut shown: Vec<String> = entries.iter().map(ToString::to_string).collect();
    shown.sort();
    assert_eq!(shown, denied(&env));
    let rows = deny_rows(&*env.store).expect("rows");
    assert!(
        rows.iter()
            .all(|d| d.by == "operator" && d.reason == "revoked" && d.at == env.now())
    );
    let n = list_nodes(&*env.store).expect("nodes").remove(0);
    assert_eq!(
        (n.state, n.revoked_at),
        (NodeState::Revoked, Some(env.now()))
    );
}

// Catches revoking twice or revoking nothing succeeding quietly, and a store failure or a
// busy store reported as success.
#[test]
fn revoke_refuses_an_unknown_or_revoked_node_and_reports_store_trouble() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 5));
    Req::new(1, "n1", &[Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    assert_eq!(
        revoke_node(&*env.store, &node("ghost"), "op"),
        Err(RegistryError::NotFound)
    );
    let unreadable = Wrap::new(env.store.clone(), Mode::FailGet);
    assert!(matches!(
        revoke_node(&unreadable, &node("n1"), "op"),
        Err(RegistryError::Store(_))
    ));
    env.store
        .txn(Txn::new().put("enroll/node/n9", [0xffu8, 0xff], Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        revoke_node(&*env.store, &node("n9"), "op"),
        Err(RegistryError::Corrupt("node"))
    );
    let down = Wrap::new(env.store.clone(), Mode::FailTxn);
    assert!(matches!(
        revoke_node(&down, &node("n1"), "op"),
        Err(RegistryError::Store(_))
    ));
    let busy = Wrap::new(env.store.clone(), Mode::Conflict);
    assert_eq!(
        revoke_node(&busy, &node("n1"), "op"),
        Err(RegistryError::Contended)
    );
    assert_eq!(busy.recorded().len(), ATTEMPTS);
    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    let before = denied(&env);
    assert_eq!(
        revoke_node(&*env.store, &node("n1"), "other"),
        Err(RegistryError::Invalid("the node is already revoked"))
    );
    assert_eq!(denied(&env), before);
}

// Catches deny rows for certificates that expired (the list would grow without bound) and
// missing ones for certificates a renewal superseded but that are still valid.
#[test]
fn a_revoke_denies_unexpired_serials_current_and_earlier_and_not_expired_ones() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 5));
    Req::new(1, "n1", &[Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    let renew = |seed: u8, tag: &str| {
        let ders = [format!("renew-{tag}").into_bytes()];
        let csrs = [kbf_enroll_registry::RenewCsr {
            role: Daemon,
            csr_der: &ders[0],
            spki_sha256: issue(&[Daemon], 1, 1000)[0].spki_sha256,
        }];
        let id = node("n1");
        let now = env.now();
        kbf_enroll_registry::renew_txn(
            &*env.store,
            &kbf_enroll_registry::RenewRequest {
                node_id: &id,
                csrs: &csrs,
                issued_at: now,
            },
            &mut || Ok(issue(&[Daemon], seed, now)),
        )
        .expect("renew");
    };
    env.advance(3 * DAY);
    renew(2, "a");
    env.advance(6 * DAY);
    // Now: enrollment cert (expires day 7) is expired; renewal 1 (day 10) is live.
    renew(3, "b");
    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    let serials: Vec<String> = denied(&env)
        .into_iter()
        .filter(|d| d.starts_with("serial "))
        .collect();
    assert_eq!(
        serials,
        vec![
            format!("serial {}", serial_of(2, Daemon)),
            format!("serial {}", serial_of(3, Daemon))
        ]
    );
}

// Catches a revoke whose certificates have all expired still being written as serials, and
// the key hash being dropped with them: the key is denied whatever the clock says.
#[test]
fn an_expired_certificate_has_no_serial_deny_row_but_its_key_is_denied() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 5));
    Req::new(1, "n1", &[Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    env.advance(8 * DAY);
    let entries = revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    let shown: Vec<String> = entries.iter().map(ToString::to_string).collect();
    assert_eq!(shown.len(), 2);
    assert!(shown.contains(&"node n1".to_owned()));
    assert!(shown.contains(&format!("spki-sha256 {}", spki_text(1, Daemon))));
}

// Catches a listing that hides a node or loses a field, and a listing or deny read that
// skips rows it cannot decode instead of saying so.
#[test]
fn listings_show_every_node_and_name_rows_they_cannot_read() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 5));
    Req::new(1, "n1", &[Storage])
        .enroll(&env, 1)
        .expect("enroll");
    Req::new(1, "n2", &[Daemon])
        .enroll(&env, 2)
        .expect("enroll");
    revoke_node(&*env.store, &node("n2"), "op").expect("revoke");
    let nodes = list_nodes(&*env.store).expect("nodes");
    let seen: Vec<_> = nodes
        .iter()
        .map(|n| (n.id.to_string(), n.state, n.token_id.clone()))
        .collect();
    let token = common::tid(1).to_string();
    assert_eq!(
        seen,
        vec![
            ("n1".to_owned(), NodeState::Active, token.clone()),
            ("n2".to_owned(), NodeState::Revoked, token)
        ]
    );
    let put = |key: &str, value: &[u8]| {
        env.store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("put");
    };
    put("deny/bogus/x", &[]);
    assert_eq!(
        deny_rows(&*env.store),
        Err(RegistryError::Corrupt("deny key"))
    );
    put("deny/bogus/x", &[0xff, 0xff]);
    assert_eq!(deny_rows(&*env.store), Err(RegistryError::Corrupt("deny")));
    env.store
        .txn(Txn::new().delete("deny/bogus/x", Cond::Any))
        .expect("store")
        .expect("delete");
    put("deny/spki-sha256/short", &[]);
    assert_eq!(
        deny_rows(&*env.store),
        Err(RegistryError::Corrupt("deny key"))
    );
    put("deny/nokind", &[]);
    env.store
        .txn(Txn::new().delete("deny/spki-sha256/short", Cond::Any))
        .expect("store")
        .expect("delete");
    assert_eq!(
        deny_rows(&*env.store),
        Err(RegistryError::Corrupt("deny key"))
    );
    put("enroll/node/Bad_Id", &[]);
    assert_eq!(
        list_nodes(&*env.store),
        Err(RegistryError::Corrupt("node key"))
    );
    env.store
        .txn(Txn::new().delete("enroll/node/Bad_Id", Cond::Any))
        .expect("store")
        .expect("delete");
    put("enroll/node/n3", &[0xff, 0xff]);
    assert_eq!(list_nodes(&*env.store), Err(RegistryError::Corrupt("node")));
    let mut odd_state = vec![0x0a, 0x08, 0x0a, 0x06];
    odd_state.extend_from_slice(b"daemon");
    odd_state.extend_from_slice(&[0x30, 0x09]);
    put("enroll/node/n3", &odd_state);
    assert_eq!(
        list_nodes(&*env.store),
        Err(RegistryError::Corrupt("node state"))
    );
    put(
        "enroll/node/n3",
        &[0x0a, 0x04, 0x0a, 0x02, b'x', b'x', 0x30, 0x01],
    );
    assert_eq!(list_nodes(&*env.store), Err(RegistryError::Corrupt("node")));
    put("enroll/node/n3", &[0x30, 0x01]);
    assert_eq!(list_nodes(&*env.store), Err(RegistryError::Corrupt("node")));
}

// Catches a CA generation that does not count, that restarts, or that is bumped by two
// writers at once to the same value.
#[test]
fn the_ca_generation_counts_bundle_changes() {
    let env = fake();
    assert_eq!(ca_generation(&*env.store), Ok(0));
    assert_eq!(bump_ca_generation(&*env.store), Ok(1));
    assert_eq!(bump_ca_generation(&*env.store), Ok(2));
    assert_eq!(ca_generation(&*env.store), Ok(2));
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let inner = std::sync::Arc::clone(&env.store);
    wrap.before_first_txn(move || {
        bump_ca_generation(&*inner).expect("rival");
    });
    assert_eq!(bump_ca_generation(&wrap), Ok(4));
    let busy = Wrap::new(env.store.clone(), Mode::Conflict);
    assert_eq!(bump_ca_generation(&busy), Err(RegistryError::Contended));
    let down = Wrap::new(env.store.clone(), Mode::FailGet);
    assert!(matches!(ca_generation(&down), Err(RegistryError::Store(_))));
    env.store
        .txn(Txn::new().put("enroll/ca/generation", [1u8, 2, 3], Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        ca_generation(&*env.store),
        Err(RegistryError::Corrupt("ca generation"))
    );
    assert_eq!(
        bump_ca_generation(&*env.store),
        Err(RegistryError::Corrupt("ca generation"))
    );
}
