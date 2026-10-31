//! A host's updater in the registry: its own row family, its token's rules, and the host
//! role it carries from the token to its renewals. Every test names the behaviour it holds.

mod common;

use common::{DAY, Req, fake, issue, make_token, node, spec, tid, updater_spec};
use kbf_enroll_registry::{
    IssuedRole, RegistryError, RenewCsr, RenewRequest, TokenSpec, create_token, deny_rows,
    list_nodes, list_tokens, renew_txn, token_host_role, updater_host_role,
};
use kbf_state::{Cond, Txn};
use kbf_types::{HostRole, RefusalReason, Role, RoleSet, TokenSecret};

use RefusalReason as R;
use Role::{Daemon, Storage, Updater};

fn keys(env: &common::Env, prefix: &str) -> Vec<String> {
    env.store
        .scan_prefix(prefix)
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn invalid(spec: &TokenSpec) -> Option<&'static str> {
    let env = fake();
    match create_token(
        &*env.store,
        &tid(1),
        &TokenSecret::from_bytes([1; 32]),
        spec,
    ) {
        Err(RegistryError::Invalid(why)) => Some(why),
        Ok(_) => None,
        Err(other) => panic!("{other}"),
    }
}

/// An updater's token is for one named host, once, and says what the host runs; a token for a
/// node never carries a host role, and the role never mixes with a node's.
#[test]
fn an_updater_token_is_for_one_named_host_once_and_with_its_host_role() {
    let good = updater_spec("host-1", HostRole::DAEMON);
    assert_eq!(invalid(&good), None);

    let mixed = TokenSpec {
        roles: RoleSet::new([Updater, Daemon]).unwrap(),
        ..good.clone()
    };
    assert_eq!(invalid(&mixed), Some("the role updater is granted alone"));
    let no_host_role = TokenSpec {
        host_role: None,
        ..good.clone()
    };
    assert_eq!(
        invalid(&no_host_role),
        Some("an updater token needs a host_role")
    );
    let no_id = TokenSpec {
        node_id: None,
        ..good.clone()
    };
    let prefix = TokenSpec {
        node_prefix: Some("host-".to_owned()),
        ..good.clone()
    };
    for spec in [no_id, prefix] {
        assert_eq!(
            invalid(&spec),
            Some("an updater token names its one host by node_id, not by a prefix")
        );
    }
    let many = TokenSpec {
        max_uses: 2,
        ..good.clone()
    };
    assert_eq!(invalid(&many), Some("an updater token has one use"));
    let node_with_host_role = TokenSpec {
        host_role: Some(HostRole::DAEMON),
        ..spec(&[Daemon], 1)
    };
    assert_eq!(
        invalid(&node_with_host_role),
        Some("host_role goes with the role updater only")
    );
}

/// The token's host role is read back from the token, and from the host's row once it
/// enrolled: what a renewal issues.
#[test]
fn the_host_role_goes_from_the_token_to_the_updaters_row() {
    let env = fake();
    let id = node("host-1");
    make_token(&env, 1, &updater_spec("host-1", HostRole::MAC_DAEMON));
    make_token(&env, 2, &spec(&[Daemon], 1));
    assert_eq!(
        token_host_role(&*env.store, &tid(1)).unwrap(),
        Some(HostRole::MAC_DAEMON)
    );
    assert_eq!(token_host_role(&*env.store, &tid(2)).unwrap(), None);
    assert_eq!(token_host_role(&*env.store, &tid(9)).unwrap(), None);
    assert_eq!(
        list_tokens(&*env.store).unwrap()[0].host_role,
        Some(HostRole::MAC_DAEMON)
    );
    assert_eq!(updater_host_role(&*env.store, &id).unwrap(), None);

    Req::new(1, "host-1", &[Updater])
        .enroll(&env, 1)
        .expect("enrolled");
    assert_eq!(
        updater_host_role(&*env.store, &id).unwrap(),
        Some(HostRole::MAC_DAEMON)
    );
}

/// A host's updater and its node share an id and must not touch each other: two rows, the
/// node listing shows the node alone, and neither enrollment is `NodeIdInUse` for the other.
#[test]
fn an_updater_and_the_node_of_one_id_have_rows_of_their_own() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 1));
    make_token(&env, 2, &updater_spec("host-1", HostRole::DAEMON));
    Req::new(1, "host-1", &[Daemon])
        .enroll(&env, 1)
        .expect("node");
    Req::new(2, "host-1", &[Updater])
        .enroll(&env, 2)
        .expect("updater");
    assert_eq!(keys(&env, "enroll/node/"), ["enroll/node/host-1"]);
    assert_eq!(keys(&env, "enroll/updater/"), ["enroll/updater/host-1"]);
    let nodes = list_nodes(&*env.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].roles, RoleSet::new([Daemon]).unwrap());
    // Both certificates are recorded by serial and key, each under its own role.
    assert_eq!(keys(&env, "enroll/serial/").len(), 2);
    assert_eq!(keys(&env, "enroll/key/").len(), 2);
    assert!(keys(&env, "deny/").is_empty());
}

/// The token's rules hold for an updater: its node id is exact, it is used once, and it
/// grants the updater role and nothing else; the signer is not called for a refusal.
#[test]
fn an_updater_token_is_refused_for_another_node_a_second_use_and_other_roles() {
    let env = fake();
    make_token(&env, 1, &updater_spec("host-1", HostRole::DAEMON));
    let refuse = |req: Req, why: R| {
        assert_eq!(
            req.run(&*env.store, &mut || panic!("the signer ran")),
            Err(why)
        );
    };
    refuse(Req::new(1, "host-2", &[Updater]), R::NodeIdNotAllowed);
    refuse(Req::new(1, "host-1", &[Daemon]), R::RoleNotGranted);
    refuse(Req::new(1, "host-1", &[Storage]), R::RoleNotGranted);
    refuse(Req::new(1, "host-1", &[Updater, Daemon]), R::BadRoles);
    let first = Req::new(1, "host-1", &[Updater]);
    first.enroll(&env, 1).expect("enrolled");
    // The same request again is the lost answer, served again without a use.
    assert!(first.enroll(&env, 1).expect("replayed").replayed);
    // Another key with the spent token: used up.
    let mut other = Req::new(1, "host-1", &[Updater]);
    other.csrs[0].1 = b"another csr".to_vec();
    refuse(other, R::TokenUsedUp);
}

/// A token for a node cannot make an updater: a daemon token, asked for the updater role.
#[test]
fn a_node_token_cannot_enroll_an_updater() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 1));
    assert_eq!(
        Req::new(1, "host-1", &[Updater]).enroll(&env, 1),
        Err(R::RoleNotGranted)
    );
}

/// An expired and a revoked token enroll no updater.
#[test]
fn an_expired_or_revoked_updater_token_is_refused() {
    let env = fake();
    make_token(&env, 1, &updater_spec("host-1", HostRole::DAEMON));
    make_token(&env, 2, &updater_spec("host-2", HostRole::DAEMON));
    kbf_enroll_registry::revoke_token(&*env.store, &tid(2)).expect("revoked");
    env.advance(2 * DAY);
    assert_eq!(
        Req::new(1, "host-1", &[Updater]).enroll(&env, 1),
        Err(R::TokenExpired)
    );
    assert_eq!(
        Req::new(2, "host-2", &[Updater]).enroll(&env, 2),
        Err(R::TokenRevoked)
    );
}

/// An updater renews under its own row with the key it enrolled with, and renewing as the
/// node's roles is not renewing the updater.
#[test]
fn an_updater_renews_its_own_row() {
    let env = fake();
    make_token(&env, 1, &updater_spec("host-1", HostRole::DAEMON));
    Req::new(1, "host-1", &[Updater])
        .enroll(&env, 1)
        .expect("enrolled");
    env.advance(3 * DAY);
    let now = env.now();
    let id = node("host-1");
    let enrolled_key = issue(&[Updater], 1, now)[0].spki_sha256;
    let renew = |role: Role, sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>| {
        let der = b"renew csr".to_vec();
        renew_txn(
            &*env.store,
            &RenewRequest {
                node_id: &id,
                csrs: &[RenewCsr {
                    role,
                    csr_der: &der,
                    spki_sha256: enrolled_key,
                }],
                issued_at: now,
            },
            sign,
        )
    };
    assert_eq!(
        renew(Daemon, &mut || panic!("the signer ran")),
        Err(R::UnknownNode)
    );
    let renewed = renew(Updater, &mut || {
        let mut out = issue(&[Updater], 2, now);
        out[0].spki_sha256 = enrolled_key;
        Ok(out)
    })
    .expect("renewed");
    assert_eq!(renewed.certs.len(), 1);
    assert_eq!(renewed.certs[0].role, Updater);
    assert_eq!(renewed.certs[0].not_after, now + 7 * DAY);
    assert_eq!(
        updater_host_role(&*env.store, &id).unwrap(),
        Some(HostRole::DAEMON),
        "the host role stays with the row"
    );
}

/// Enrolling the host's updater again with a replace token ends the old key's right to renew
/// (its serial and key are denied) and does not deny the node id, which the host's node
/// shares.
#[test]
fn a_replaced_updater_denies_its_old_certificate_but_not_the_node_id() {
    let env = fake();
    make_token(&env, 1, &updater_spec("host-1", HostRole::DAEMON));
    make_token(
        &env,
        2,
        &TokenSpec {
            replace: true,
            ..updater_spec("host-1", HostRole::DAEMON)
        },
    );
    Req::new(1, "host-1", &[Updater])
        .enroll(&env, 1)
        .expect("first");
    Req::new(2, "host-1", &[Updater])
        .enroll(&env, 2)
        .expect("second");
    let denied = deny_rows(&*env.store).expect("deny rows");
    assert_eq!(denied.len(), 2, "a serial and a key: {denied:?}");
    assert!(
        denied
            .iter()
            .all(|row| !matches!(row.entry, kbf_map::DenyEntry::Node(_))),
        "{denied:?}"
    );
}

/// A host role a row holds that is not one is a row that does not read, told as such: the
/// listing of tokens, the token's host role and the updater's.
#[test]
fn a_row_with_a_host_role_that_is_not_one_is_unreadable() {
    let env = fake();
    make_token(&env, 1, &updater_spec("host-1", HostRole::DAEMON));
    Req::new(1, "host-1", &[Updater])
        .enroll(&env, 1)
        .expect("enrolled");
    // A later occurrence of a field wins when a row is read: `host_role` (tag 14 of a token
    // row, 12 of a node row) is set to a name that is no host role.
    let bogus = |tag: u8| {
        let mut bytes = vec![tag, 5];
        bytes.extend_from_slice(b"cache");
        bytes
    };
    for (key, tag) in [("enroll/token/", 0x72), ("enroll/updater/", 0x62)] {
        let (name, stored) = env
            .store
            .scan_prefix(key)
            .expect("scan")
            .into_iter()
            .next()
            .expect("a row");
        let mut value = stored.value.clone();
        value.extend(bogus(tag));
        env.store
            .txn(Txn::new().put(&name, value, Cond::Version(stored.version)))
            .expect("store")
            .expect("put");
    }
    assert_eq!(
        list_tokens(&*env.store).err(),
        Some(RegistryError::Corrupt("token"))
    );
    assert_eq!(
        token_host_role(&*env.store, &tid(1)).err(),
        Some(RegistryError::Corrupt("token"))
    );
    assert_eq!(
        updater_host_role(&*env.store, &node("host-1")).err(),
        Some(RegistryError::Corrupt("node"))
    );
}

/// Revoking a host id also ends its updater: the updater row is revoked, so its renewal is
/// refused, and its serial and key are denied. A host with an updater and no node row is
/// revoked by the same call, and its id is not burned.
#[test]
fn revoking_a_host_ends_its_updater_renewals() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 1));
    make_token(&env, 2, &updater_spec("host-1", HostRole::DAEMON));
    make_token(&env, 3, &updater_spec("host-2", HostRole::DAEMON));
    Req::new(1, "host-1", &[Daemon])
        .enroll(&env, 1)
        .expect("node");
    Req::new(2, "host-1", &[Updater])
        .enroll(&env, 2)
        .expect("updater");
    Req::new(3, "host-2", &[Updater])
        .enroll(&env, 3)
        .expect("updater alone");
    let now = env.now();
    let renew = |name: &str, seed: u8| {
        let der = b"renew csr".to_vec();
        let key = issue(&[Updater], seed, now)[0].spki_sha256;
        renew_txn(
            &*env.store,
            &RenewRequest {
                node_id: &node(name),
                csrs: &[RenewCsr {
                    role: Updater,
                    csr_der: &der,
                    spki_sha256: key,
                }],
                issued_at: now,
            },
            &mut || panic!("the signer ran"),
        )
    };

    let denied =
        kbf_enroll_registry::revoke_node(&*env.store, &node("host-1"), "op").expect("revoked");
    assert!(
        denied.len() > 2,
        "node and updater serials and keys are denied: {denied:?}"
    );
    assert_eq!(renew("host-1", 2), Err(R::NodeRevoked));

    kbf_enroll_registry::revoke_node(&*env.store, &node("host-2"), "op").expect("updater alone");
    assert_eq!(renew("host-2", 3), Err(R::NodeRevoked));
    assert!(
        !deny_rows(&*env.store)
            .unwrap()
            .iter()
            .any(|d| d.entry.to_string() == "node host-2"),
        "an updater-only revoke burns no node id"
    );
    assert!(matches!(
        kbf_enroll_registry::revoke_node(&*env.store, &node("host-9"), "op"),
        Err(RegistryError::NotFound)
    ));
}
