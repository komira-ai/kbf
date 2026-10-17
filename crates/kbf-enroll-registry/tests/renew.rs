//! The renew transaction. Every test names the defect it catches.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{DAY, Env, Mode, Req, Wrap, fake, issue, make_token, node, serial_of, spec};
use kbf_enroll_registry::{
    Enrolled, ISSUED_AT_WINDOW, IssuedRole, MAX_EXPIRED_AGE, MIN_INTERVAL, NodeState, REPLAY_TTL,
    RenewCsr, RenewRequest, deny_rows, list_nodes, renew_txn, revoke_node,
};
use kbf_state::{Cond, Fenced, Store, Txn};
use kbf_types::{RefusalReason, Role};

use RefusalReason as R;
use Role::{Daemon, Storage};

fn spki(seed: u8, role: Role) -> [u8; 32] {
    issue(&[role], seed, 1000)[0].spki_sha256
}

/// Renews node `name` (enrolled under `old_seed`) as `roles`, signing with `new_seed`.
fn renew_at(
    env: &Env,
    name: &str,
    old_seed: u8,
    roles: &[Role],
    new_seed: u8,
    issued_at: u64,
    tag: &str,
) -> Result<Enrolled, RefusalReason> {
    renew_with(
        &*env.store,
        env,
        name,
        old_seed,
        roles,
        issued_at,
        tag,
        &mut || Ok(issue(roles, new_seed, env.now())),
    )
}

#[allow(clippy::too_many_arguments)]
fn renew_with(
    store: &dyn Store,
    env: &Env,
    name: &str,
    old_seed: u8,
    roles: &[Role],
    issued_at: u64,
    tag: &str,
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Enrolled, RefusalReason> {
    let _ = env;
    let ders: Vec<Vec<u8>> = roles
        .iter()
        .map(|r| format!("renew-{tag}-{r}").into_bytes())
        .collect();
    let csrs: Vec<RenewCsr<'_>> = roles
        .iter()
        .zip(&ders)
        .map(|(role, der)| RenewCsr {
            role: *role,
            csr_der: der,
            spki_sha256: spki(old_seed, *role),
        })
        .collect();
    let id = node(name);
    renew_txn(
        store,
        &RenewRequest {
            node_id: &id,
            csrs: &csrs,
            issued_at,
        },
        sign,
    )
}

fn enrolled(env: &Env, name: &str, seed: u8, roles: &[Role]) {
    make_token(env, seed, &spec(roles, 5));
    Req::new(seed, name, roles)
        .enroll(env, seed)
        .expect("enroll");
}

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out
}

fn never() -> impl FnMut() -> Result<Vec<IssuedRole>, RefusalReason> {
    || panic!("the signer ran for a request that should have been refused")
}

// Catches a renewal that does not move the node on: new serials and times in the row, the
// old serials marked superseded (with the time), the replay marker with its ten minutes,
// and a node that keeps the roles and the enrollment facts it had.
#[test]
fn a_renewal_records_new_serials_and_supersedes_the_old() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Storage, Daemon]);
    let before = list_nodes(&*env.store).expect("nodes").remove(0);
    env.advance(3 * DAY);
    let now = env.now();
    let out = renew_at(&env, "n1", 1, &[Storage, Daemon], 2, now, "a").expect("renew");
    assert!(!out.replayed);
    assert_eq!(out.certs.len(), 2);
    assert_eq!(out.certs[0].serial, serial_of(2, Storage));
    assert_eq!(out.certs[0].not_after, now + 7 * DAY);
    let after = list_nodes(&*env.store).expect("nodes").remove(0);
    assert_eq!(after.last_renewed_at, Some(now));
    assert_eq!(after.not_after, now + 7 * DAY);
    assert_eq!(
        (
            after.roles,
            after.advertise,
            after.token_label,
            after.created_at,
            after.state
        ),
        (
            before.roles,
            before.advertise,
            before.token_label,
            before.created_at,
            NodeState::Active
        )
    );
    for role in [Storage, Daemon] {
        let old = env
            .store
            .get(&format!("enroll/serial/{}", serial_of(1, role)))
            .expect("get")
            .expect("row");
        let mut stamp = vec![0x20];
        stamp.extend(varint(now));
        assert!(
            old.value.ends_with(&stamp),
            "the old serial carries superseded_at"
        );
        let new = env
            .store
            .get(&format!("enroll/serial/{}", serial_of(2, role)))
            .expect("get")
            .expect("row");
        assert!(
            !new.value.ends_with(&stamp),
            "the new serial is not superseded"
        );
    }
    let markers = env.store.scan_prefix("enroll/renew/n1/").expect("scan");
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0]
            .1
            .expires_at
            .expect("a ttl")
            .as_duration()
            .as_secs(),
        now + REPLAY_TTL.as_secs()
    );
}

// Catches a renewal that adopts the key the signer returns instead of the one the registry
// holds (key continuity): the node's key hashes must not change.
#[test]
fn the_registry_keeps_its_key_whatever_the_signer_returns() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Daemon]);
    env.advance(DAY);
    let out = renew_at(&env, "n1", 1, &[Daemon], 2, env.now(), "a").expect("renew");
    assert_eq!(out.certs[0].spki_sha256, spki(1, Daemon));
    assert_eq!(out.certs[0].serial, serial_of(2, Daemon));
    // A renewal for a role the signer did not return a certificate for keeps nothing odd.
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 1);
}

// Catches each renewal check being missing or reversed: unknown node, revoked node, roles
// that are not exactly the node's, a key the registry does not hold, an issued_at outside
// the window on either side (the edges are in), a signer refusal and a signer that returns
// other roles. None of them signs where it should not, and none writes.
#[test]
fn a_renewal_that_fails_a_check_is_refused_and_writes_nothing() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Storage, Daemon]);
    let now = env.now();
    let rows_before = env.store.scan_prefix("").expect("scan").len();
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "ghost",
            1,
            &[Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::UnknownNode)
    );
    for roles in [&[Daemon][..], &[Storage][..]] {
        assert_eq!(
            renew_with(&*env.store, &env, "n1", 1, roles, now, "a", &mut never()),
            Err(R::BadRoles)
        );
    }
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Storage, Daemon, Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::BadRoles)
    );
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            2,
            &[Storage, Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::WrongKey)
    );
    let both = [Storage, Daemon];
    for stale in [now - ISSUED_AT_WINDOW - 1, now + ISSUED_AT_WINDOW + 1, 0] {
        assert_eq!(
            renew_with(&*env.store, &env, "n1", 1, &both, stale, "a", &mut never()),
            Err(R::StaleIssuedAt),
            "{stale}"
        );
    }
    assert_eq!(
        renew_with(&*env.store, &env, "n1", 1, &both, now, "a", &mut || Err(
            R::BadCsr
        )),
        Err(R::BadCsr)
    );
    assert_eq!(
        renew_with(&*env.store, &env, "n1", 1, &both, now, "a", &mut || Ok(
            issue(&[Daemon], 2, now)
        )),
        Err(R::BadRoles)
    );
    assert_eq!(env.store.scan_prefix("").expect("scan").len(), rows_before);

    // The edges of the window are inside it.
    renew_at(&env, "n1", 1, &both, 2, now - ISSUED_AT_WINDOW, "edge-low").expect("low edge");
    env.advance(MIN_INTERVAL);
    let now = env.now();
    renew_at(&env, "n1", 1, &both, 3, now + ISSUED_AT_WINDOW, "edge-high").expect("high edge");

    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    assert_eq!(
        renew_with(&*env.store, &env, "n1", 1, &both, now, "z", &mut never()),
        Err(R::NodeRevoked)
    );
}

// Catches a replay set held in memory or never expiring, and the minimum interval being
// off by one: the same CSR is refused for ten minutes and accepted after; a different one
// is refused for a minute and accepted at exactly a minute.
#[test]
fn renewals_are_spaced_and_a_csr_is_remembered_in_the_store_for_ten_minutes() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Daemon]);
    env.advance(DAY);
    renew_at(&env, "n1", 1, &[Daemon], 2, env.now(), "a").expect("renew");
    // Immediately: a different CSR is too soon, and so is the same.
    for tag in ["b", "a"] {
        assert_eq!(
            renew_with(
                &*env.store,
                &env,
                "n1",
                1,
                &[Daemon],
                env.now(),
                tag,
                &mut never()
            ),
            Err(R::RenewTooSoon)
        );
    }
    env.advance(MIN_INTERVAL - 1);
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Daemon],
            env.now(),
            "b",
            &mut never()
        ),
        Err(R::RenewTooSoon)
    );
    env.advance(1);
    // A minute on, a different CSR goes through; the first CSR is still remembered.
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Daemon],
            env.now(),
            "a",
            &mut never()
        ),
        Err(R::RenewTooSoon)
    );
    renew_at(&env, "n1", 1, &[Daemon], 3, env.now(), "b").expect("renew");
    env.advance(REPLAY_TTL.as_secs() - MIN_INTERVAL);
    // Ten minutes after the first renewal its CSR is forgotten (a row's expiry, not memory).
    renew_at(&env, "n1", 1, &[Daemon], 4, env.now(), "a").expect("the marker expired");
}

// Catches the thirty-day bound being off by one or missing: a node whose newest certificate
// expired exactly thirty days ago may still renew, one second later it must enroll again.
#[test]
fn a_node_expired_for_over_thirty_days_must_enroll_again() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Daemon]);
    enrolled(&env, "n2", 2, &[Daemon]);
    env.advance(7 * DAY + MAX_EXPIRED_AGE);
    renew_at(&env, "n1", 1, &[Daemon], 3, env.now(), "a").expect("exactly thirty days");
    env.advance(1);
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n2",
            2,
            &[Daemon],
            env.now(),
            "a",
            &mut never()
        ),
        Err(R::RenewalExpired)
    );
}

// Catches a revocation that loses to a renewal: the renewal read the node active, a revoke
// lands, and the renewal's commit must fail and be decided again (and refused). No cert is
// recorded after the revoke, the node stays revoked. (Mutant: renew without Cond::Version
// on the node row writes the node back as active.)
#[test]
fn a_revoke_racing_a_renewal_wins() {
    let env = fake();
    enrolled(&env, "n1", 1, &[Daemon]);
    env.advance(DAY);
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let inner = Arc::clone(&env.store);
    wrap.before_first_txn(move || {
        revoke_node(&*inner, &node("n1"), "op").expect("revoke");
    });
    let signed = AtomicUsize::new(0);
    let now = env.now();
    let out = renew_with(&wrap, &env, "n1", 1, &[Daemon], now, "a", &mut || {
        signed.fetch_add(1, Ordering::SeqCst);
        Ok(issue(&[Daemon], 2, now))
    });
    assert_eq!(out, Err(R::NodeRevoked));
    assert_eq!(
        signed.load(Ordering::SeqCst),
        1,
        "signed once, before the revoke landed"
    );
    let n = list_nodes(&*env.store).expect("nodes").remove(0);
    assert_eq!(n.state, NodeState::Revoked);
    assert_eq!(n.last_renewed_at, None);
    assert!(
        env.store
            .get(&format!("enroll/serial/{}", serial_of(2, Daemon)))
            .expect("get")
            .is_none()
    );
    assert!(
        env.store
            .scan_prefix("enroll/renew/")
            .expect("scan")
            .is_empty()
    );
    assert!(
        deny_rows(&*env.store)
            .expect("deny")
            .iter()
            .all(|d| d.entry.to_string() != format!("serial {}", serial_of(2, Daemon)))
    );
}

// Catches trouble answered as anything but "try again" or "not the signer".
#[test]
fn trouble_in_the_store_is_unavailable_and_a_fenced_handle_is_not_the_signer() {
    let env = fake();
    let stale = Fenced::take(env.store.clone(), None).expect("take");
    let _fresh = Fenced::take(env.store.clone(), Some(1)).expect("take");
    enrolled(&env, "n1", 1, &[Daemon]);
    env.advance(DAY);
    let now = env.now();
    let mut sign = || Ok(issue(&[Daemon], 2, now));
    assert_eq!(
        renew_with(&stale, &env, "n1", 1, &[Daemon], now, "a", &mut sign),
        Err(R::NotSigner)
    );
    let reads = Wrap::new(env.store.clone(), Mode::FailGet);
    assert_eq!(
        renew_with(&reads, &env, "n1", 1, &[Daemon], now, "a", &mut sign),
        Err(R::StoreUnavailable)
    );
    let writes = Wrap::new(env.store.clone(), Mode::FailTxn);
    assert_eq!(
        renew_with(&writes, &env, "n1", 1, &[Daemon], now, "a", &mut sign),
        Err(R::StoreUnavailable)
    );
    let busy = Wrap::new(env.store.clone(), Mode::Conflict);
    assert_eq!(
        renew_with(&busy, &env, "n1", 1, &[Daemon], now, "a", &mut sign),
        Err(R::StoreUnavailable)
    );
    assert_eq!(busy.recorded().len(), kbf_enroll_registry::ATTEMPTS);
    let n = list_nodes(&*env.store).expect("nodes").remove(0);
    assert_eq!(n.last_renewed_at, None);
}

// Catches a row that does not decode being renewed over: a node row with no certificates,
// an unknown state or bytes that are not a row is refused as unavailable, not repaired.
#[test]
fn an_unreadable_node_row_is_not_renewed() {
    let env = fake();
    let now = env.now();
    let put = |value: &[u8]| {
        env.store
            .txn(Txn::new().put("enroll/node/n1", value, Cond::Any))
            .expect("store")
            .expect("put");
    };
    put(&[0x30, 0x01]);
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::StoreUnavailable)
    );
    put(&[0x30, 0x09]);
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::StoreUnavailable)
    );
    put(&[0xff, 0xff]);
    assert_eq!(
        renew_with(
            &*env.store,
            &env,
            "n1",
            1,
            &[Daemon],
            now,
            "a",
            &mut never()
        ),
        Err(R::StoreUnavailable)
    );
}
