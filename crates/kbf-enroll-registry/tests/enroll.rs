//! The enroll transaction: the order of decisions, the races, and what each refusal leaves
//! behind (nothing). Every test names the defect it catches.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    DAY, Env, Mode, Req, Wrap, fake, issue, make_token, node, serial_of, spec, sqlite, tid,
};
use kbf_enroll_registry::{
    NodeState, TokenInfo, deny_rows, list_nodes, list_tokens, request_hash, revoke_node,
};
use kbf_state::{Cond, Fenced, Txn};
use kbf_types::{RefusalReason, Role};

use RefusalReason as R;
use Role::{Daemon, Storage};

fn token(env: &Env, n: u8) -> TokenInfo {
    list_tokens(&*env.store)
        .expect("list")
        .into_iter()
        .find(|t| t.id == tid(u64::from(n)))
        .expect("token")
}

fn keys(env: &Env, prefix: &str) -> Vec<String> {
    env.store
        .scan_prefix(prefix)
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

/// Nothing but the token row exists: no node, serial, key, idempotency or deny row.
fn nothing_landed(env: &Env) {
    for prefix in [
        "enroll/node/",
        "enroll/serial/",
        "enroll/key/",
        "enroll/req/",
        "enroll/adv/",
        "deny/",
    ] {
        assert!(keys(env, prefix).is_empty(), "{prefix} has rows");
    }
}

fn never_signs() -> impl FnMut() -> Result<Vec<kbf_enroll_registry::IssuedRole>, RefusalReason> {
    || panic!("the signer ran for a request that should have been refused")
}

// Catches an enroll that writes any of the rows B1 section 8.2 names with a wrong value,
// a missing row (no reverse lookup, no idempotency row) or an idempotency row that outlives
// the token by more than a day.
#[test]
fn a_good_enrollment_writes_the_node_its_lookups_and_one_use() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 2));
    let done = Req::new(1, "n1", &[Daemon, Storage])
        .enroll(&env, 0x0a)
        .expect("enroll");
    assert!(!done.replayed);
    let got: Vec<(Role, String)> = done
        .certs
        .iter()
        .map(|c| (c.role, c.serial.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Storage, serial_of(0x0a, Storage)),
            (Daemon, serial_of(0x0a, Daemon))
        ]
    );
    assert_eq!(token(&env, 1).uses, 1);
    let nodes = list_nodes(&*env.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    let n = &nodes[0];
    assert_eq!(n.id, node("n1"));
    assert_eq!(n.state, NodeState::Active);
    assert_eq!(n.advertise, "n1.example:7000");
    assert_eq!((n.zone.as_str(), n.token_label.as_str()), ("z1", "lab"));
    assert_eq!(
        (n.created_at, n.not_after, n.last_renewed_at),
        (env.now(), env.now() + 7 * DAY, None)
    );
    assert_eq!(keys(&env, "enroll/serial/").len(), 2);
    assert_eq!(keys(&env, "enroll/key/").len(), 2);
    assert_eq!(
        keys(&env, "enroll/adv/"),
        vec!["enroll/adv/n1.example:7000".to_owned()]
    );
    let req = env.store.scan_prefix("enroll/req/").expect("scan");
    assert_eq!(req.len(), 1);
    let expires = req[0].1.expires_at.expect("a ttl").as_duration().as_secs();
    assert_eq!(expires, token(&env, 1).expires_at + DAY);
    assert!(keys(&env, "deny/").is_empty());
}

// Catches a rejection that tells a guesser anything: an unknown id, a wrong secret and a
// secret of the wrong length are one answer, reached without calling the signer or writing.
#[test]
fn a_wrong_secret_and_an_unknown_id_are_one_answer_and_change_nothing() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 2));
    let mut wrong = Req::new(1, "n1", &[Storage]);
    wrong.secret[31] ^= 1;
    let mut short = Req::new(1, "n1", &[Storage]);
    short.secret.pop();
    let mut long = Req::new(1, "n1", &[Storage]);
    long.secret.push(1);
    let mut unknown = Req::new(1, "n1", &[Storage]);
    unknown.token = tid(99);
    // The dummy digest is not a secret anyone can present: 0x5a bytes under an unknown id.
    let mut dummy = Req::new(1, "n1", &[Storage]);
    dummy.token = tid(98);
    dummy.secret = vec![0x5a; 32];
    for req in [wrong, short, long, unknown, dummy] {
        assert_eq!(
            req.run(&*env.store, &mut never_signs()),
            Err(R::TokenRefused)
        );
    }
    assert_eq!(token(&env, 1).uses, 0);
    nothing_landed(&env);
}

// Catches a token reason told to a caller who has not proved the secret, and the order of
// the reasons (revoked, then expired, then used up).
#[test]
fn token_reasons_are_told_only_after_the_secret_and_in_order() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 1));
    let mut guess = Req::new(1, "n1", &[Storage]);
    guess.secret = vec![9; 32];
    kbf_enroll_registry::revoke_token(&*env.store, &tid(1)).expect("revoke");
    assert_eq!(
        guess.run(&*env.store, &mut never_signs()),
        Err(R::TokenRefused)
    );
    let good = Req::new(1, "n1", &[Storage]);
    assert_eq!(
        good.run(&*env.store, &mut never_signs()),
        Err(R::TokenRevoked)
    );
    nothing_landed(&env);

    // Expired is judged at the store's clock, and at exactly the expiry already.
    make_token(&env, 2, &spec(&[Storage], 1));
    let req = Req::new(2, "n2", &[Storage]);
    env.advance(DAY - 1);
    assert_eq!(req.run(&*env.store, &mut || Err(R::BadCsr)), Err(R::BadCsr));
    env.advance(1);
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::TokenExpired)
    );

    // A token both expired and used up says expired; one revoked and expired says revoked.
    make_token(&env, 3, &spec(&[Storage], 1));
    Req::new(3, "n3", &[Storage])
        .enroll(&env, 3)
        .expect("enroll");
    let other = Req::new(3, "n4", &[Storage]);
    assert_eq!(
        other.run(&*env.store, &mut never_signs()),
        Err(R::TokenUsedUp)
    );
    env.advance(DAY);
    assert_eq!(
        other.run(&*env.store, &mut never_signs()),
        Err(R::TokenExpired)
    );
    kbf_enroll_registry::revoke_token(&*env.store, &tid(3)).expect("revoke");
    assert_eq!(
        other.run(&*env.store, &mut never_signs()),
        Err(R::TokenRevoked)
    );
}

// Catches an enroll that reads the use count outside the transaction's compare: more
// concurrent callers than uses must still make exactly `max_uses` nodes. (Mutant: drop
// Cond::Version on the token row.)
fn racing_enrollments_make_exactly_max_uses_nodes(env: Env) {
    const MAX: u32 = 3;
    const CALLERS: usize = MAX as usize + 4;
    make_token(&env, 1, &spec(&[Storage], MAX));
    let signed = Arc::new(AtomicUsize::new(0));
    let results: Vec<Result<bool, RefusalReason>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..CALLERS)
            .map(|i| {
                let store = Arc::clone(&env.store);
                let signed = Arc::clone(&signed);
                let now = env.now();
                scope.spawn(move || {
                    let req = Req::new(1, &format!("n{i}"), &[Storage]);
                    let seed = u8::try_from(i + 1).expect("seed");
                    req.run(&*store, &mut || {
                        signed.fetch_add(1, Ordering::SeqCst);
                        Ok(issue(&[Storage], seed, now))
                    })
                    .map(|e| e.replayed)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    let won = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(won, MAX as usize, "{results:?}");
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| *e == R::TokenUsedUp)
    );
    assert_eq!(token(&env, 1).uses, MAX);
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), MAX as usize);
    assert_eq!(keys(&env, "enroll/serial/").len(), MAX as usize);
    assert!(signed.load(Ordering::SeqCst) >= MAX as usize);
}

#[test]
fn racing_enrollments_make_exactly_max_uses_nodes_in_memory() {
    racing_enrollments_make_exactly_max_uses_nodes(fake());
}

#[test]
fn racing_enrollments_make_exactly_max_uses_nodes_on_sqlite() {
    racing_enrollments_make_exactly_max_uses_nodes(sqlite("race"));
}

// Catches an idempotency lookup placed after the uses check: the retry of the last use
// (a lost answer) must succeed with no second use and no signature, while a different
// request on the same used-up token is still refused.
#[test]
fn a_retry_of_the_last_use_succeeds_without_spending_another() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 1));
    let req = Req::new(1, "n1", &[Storage]);
    let first = req.enroll(&env, 1).expect("enroll");
    let again = req.run(&*env.store, &mut never_signs()).expect("replay");
    assert!(again.replayed);
    assert_eq!(again.certs, first.certs);
    assert_eq!(token(&env, 1).uses, 1);
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 1);
    let other = Req::new(1, "n2", &[Storage]);
    assert_eq!(
        other.run(&*env.store, &mut never_signs()),
        Err(R::TokenUsedUp)
    );
    // The replay does not recheck the token: revoked, or expired, it still answers.
    kbf_enroll_registry::revoke_token(&*env.store, &tid(1)).expect("revoke");
    assert!(
        req.run(&*env.store, &mut never_signs())
            .expect("replay")
            .replayed
    );
    env.advance(DAY + 1);
    assert!(
        req.run(&*env.store, &mut never_signs())
            .expect("replay")
            .replayed
    );
}

// Catches an idempotency row keyed by the CSRs alone: the same CSRs asked for under another
// node id would return the first node's certificates.
#[test]
fn the_same_csrs_under_another_node_id_are_malformed() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 3));
    let first = Req::new(1, "n1", &[Storage]);
    first.enroll(&env, 1).expect("enroll");
    let mut other = first.clone();
    other.node = node("n2");
    other.advertise = "n2.example:7000".to_owned();
    assert_eq!(
        other.run(&*env.store, &mut never_signs()),
        Err(R::MalformedRequest)
    );
    assert_eq!(token(&env, 1).uses, 1);
}

// Catches a replay that returns certificates of a node that is revoked or whose
// certificates have expired: those fall through to the policy, which refuses them.
#[test]
fn a_replay_is_answered_only_for_an_active_node_with_live_certificates() {
    let env = fake();
    let mut s = spec(&[Storage], 5);
    s.ttl = Some(Duration::from_secs(30 * DAY));
    make_token(&env, 1, &s);
    let req = Req::new(1, "n1", &[Storage]);
    req.enroll(&env, 1).expect("enroll");
    env.advance(8 * DAY);
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::NodeIdInUse)
    );
    make_token(&env, 2, &s);
    let req = Req::new(2, "n2", &[Storage]);
    req.enroll(&env, 2).expect("enroll");
    revoke_node(&*env.store, &node("n2"), "op").expect("revoke");
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::NodeRevoked)
    );
    // An idempotency row whose node is gone (a purged registry) is not an answer.
    let req3 = Req::new(2, "n3", &[Storage]);
    req3.enroll(&env, 3).expect("enroll");
    env.store
        .txn(Txn::new().delete("enroll/node/n3", Cond::Any))
        .expect("store")
        .expect("delete");
    assert!(!req3.enroll(&env, 4).expect("a new enrollment").replayed);
}

// Catches roles, advertise and shape accepted without a state read: each of these is
// refused before the signer runs and leaves nothing.
#[test]
fn a_request_of_the_wrong_shape_is_refused_before_anything_else() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 9));
    let mut none = Req::new(1, "n1", &[Daemon]);
    none.csrs.clear();
    assert_eq!(none.run(&*env.store, &mut never_signs()), Err(R::BadRoles));
    let mut twice = Req::new(1, "n1", &[Daemon]);
    twice.csrs.push((Daemon, b"another".to_vec()));
    assert_eq!(twice.run(&*env.store, &mut never_signs()), Err(R::BadRoles));
    for bad in ["", "host", ":7000", "host:0", "host:x", "host:70000"] {
        let mut r = Req::new(1, "n1", &[Storage]);
        r.advertise = bad.to_owned();
        assert_eq!(
            r.run(&*env.store, &mut never_signs()),
            Err(R::MalformedRequest),
            "{bad:?}"
        );
    }
    let mut daemon = Req::new(1, "n1", &[Daemon]);
    daemon.advertise = "host:7000".to_owned();
    assert_eq!(
        daemon.run(&*env.store, &mut never_signs()),
        Err(R::MalformedRequest)
    );
    nothing_landed(&env);
}

// Catches a token that grants more than its roles, an exact node id or prefix that is not
// enforced (either alone, either failing), and a names policy that matches too much or too
// little: exact host, `.suffix`, neither.
#[test]
fn the_tokens_policy_decides_roles_node_ids_and_hosts() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 9));
    assert_eq!(
        Req::new(1, "n1", &[Storage]).run(&*env.store, &mut never_signs()),
        Err(R::RoleNotGranted)
    );
    assert_eq!(
        Req::new(1, "n1", &[Storage, Daemon]).run(&*env.store, &mut never_signs()),
        Err(R::RoleNotGranted)
    );

    let mut exact = spec(&[Daemon], 9);
    exact.node_id = Some(node("mac1"));
    make_token(&env, 2, &exact);
    assert_eq!(
        Req::new(2, "mac2", &[Daemon]).run(&*env.store, &mut never_signs()),
        Err(R::NodeIdNotAllowed)
    );
    let mut prefix = spec(&[Daemon], 9);
    prefix.node_prefix = Some("lab-".to_owned());
    make_token(&env, 3, &prefix);
    assert_eq!(
        Req::new(3, "prod-1", &[Daemon]).run(&*env.store, &mut never_signs()),
        Err(R::NodeIdNotAllowed)
    );
    Req::new(3, "lab-1", &[Daemon])
        .enroll(&env, 1)
        .expect("the prefix is carried");
    Req::new(2, "mac1", &[Daemon])
        .enroll(&env, 2)
        .expect("the exact id");

    let mut names = spec(&[Storage], 9);
    names.names = vec!["Fixed.example".to_owned(), ".farm.test".to_owned()];
    make_token(&env, 4, &names);
    let host = |h: &str| {
        let mut r = Req::new(4, "s1", &[Storage]);
        r.advertise = format!("{h}:7000");
        r
    };
    assert_eq!(
        host("other.example").run(&*env.store, &mut never_signs()),
        Err(R::AdvertiseNotAllowed)
    );
    assert_eq!(
        host("xfarm.test").run(&*env.store, &mut never_signs()),
        Err(R::AdvertiseNotAllowed)
    );
    assert_eq!(
        host("fixed.example.evil").run(&*env.store, &mut never_signs()),
        Err(R::AdvertiseNotAllowed)
    );
    host("FIXED.example")
        .enroll(&env, 5)
        .expect("an exact host, in any case");
    let mut sub = host("a.farm.test");
    sub.node = node("s2");
    sub.csrs[0].1 = b"another key".to_vec();
    sub.enroll(&env, 6).expect("a suffix");
}

// Catches an advertised address two nodes can hold (a man in the middle for storage
// clients), including by a different spelling of the host.
#[test]
fn an_advertised_address_belongs_to_one_node() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage], 9));
    let first = Req::new(1, "n1", &[Storage]);
    first.enroll(&env, 1).expect("enroll");
    let mut taker = Req::new(1, "n2", &[Storage]);
    taker.advertise = "N1.example:7000".to_owned();
    assert_eq!(
        taker.run(&*env.store, &mut never_signs()),
        Err(R::AdvertiseInUse)
    );
    assert_eq!(token(&env, 1).uses, 1);
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 1);
}

// Catches a registry that lets a node id be enrolled twice, or a revoked id come back by
// any path (revoked ids are burned, replace token or not).
#[test]
fn an_enrolled_node_id_needs_replace_and_a_revoked_one_is_burned() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 9));
    Req::new(1, "n1", &[Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    let mut again = Req::new(1, "n1", &[Daemon]);
    again.csrs[0].1 = b"another key".to_vec();
    assert_eq!(
        again.run(&*env.store, &mut never_signs()),
        Err(R::NodeIdInUse)
    );
    let mut s = spec(&[Daemon], 9);
    s.replace = true;
    make_token(&env, 2, &s);
    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    let mut burned = Req::new(2, "n1", &[Daemon]);
    burned.csrs[0].1 = b"a third key".to_vec();
    assert_eq!(
        burned.run(&*env.store, &mut never_signs()),
        Err(R::NodeRevoked)
    );
    assert_eq!(token(&env, 2).uses, 0);
}

// Catches a replace that leaves the old keys and serials valid: they are denied in the
// same transaction as the new rows, and the old advertise row is released or reused.
#[test]
fn a_replacement_denies_the_old_certificates_in_the_same_transaction() {
    let env = fake();
    let mut s = spec(&[Storage, Daemon], 9);
    s.replace = true;
    make_token(&env, 1, &s);
    Req::new(1, "n1", &[Storage, Daemon])
        .enroll(&env, 1)
        .expect("enroll");

    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let mut again = Req::new(1, "n1", &[Storage, Daemon]);
    again.csrs[0].1 = b"new storage key".to_vec();
    let now = env.now();
    again
        .run(&wrap, &mut || Ok(issue(&[Storage, Daemon], 2, now)))
        .expect("replace");
    assert_eq!(wrap.recorded().len(), 1, "one transaction");
    let denied: Vec<String> = deny_rows(&*env.store)
        .expect("deny rows")
        .into_iter()
        .map(|d| d.entry.to_string())
        .collect();
    assert!(denied.contains(&format!("serial {}", serial_of(1, Storage))));
    assert!(denied.contains(&format!("serial {}", serial_of(1, Daemon))));
    assert_eq!(
        denied
            .iter()
            .filter(|d| d.starts_with("spki-sha256 "))
            .count(),
        2
    );
    assert!(
        !denied.iter().any(|d| d.starts_with("node ")),
        "the id stays usable"
    );
    assert_eq!(denied.len(), 4);
    assert!(
        deny_rows(&*env.store)
            .expect("rows")
            .iter()
            .all(|d| d.reason == "replaced" && d.by == "lab")
    );
    let nodes = list_nodes(&*env.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(keys(&env, "enroll/adv/").len(), 1);

    // To another host: the old address is released. To a daemon-only node: released too.
    let mut moved = Req::new(1, "n1", &[Storage]);
    moved.advertise = "elsewhere.example:7001".to_owned();
    moved.csrs[0].1 = b"moved".to_vec();
    moved.enroll(&env, 3).expect("replace with a new host");
    assert_eq!(
        keys(&env, "enroll/adv/"),
        vec!["enroll/adv/elsewhere.example:7001".to_owned()]
    );
    let mut daemon_only = Req::new(1, "n1", &[Daemon]);
    daemon_only.csrs[0].1 = b"daemon".to_vec();
    daemon_only.enroll(&env, 4).expect("replace with a daemon");
    assert!(keys(&env, "enroll/adv/").is_empty());
    // Replacing a node that held no address leaves no address row to delete.
    let mut again = Req::new(1, "n1", &[Daemon]);
    again.csrs[0].1 = b"daemon 2".to_vec();
    again.enroll(&env, 5).expect("replace a daemon");
    assert_eq!(token(&env, 1).uses, 5);
}

// Catches a key shared by two node ids (Cond::Absent on enroll/key/ dropped or the check
// skipped), a signer that returns the wrong roles, repeats a key or a serial, or refuses:
// each leaves the use unspent and nothing written.
#[test]
fn a_key_belongs_to_one_node_and_a_bad_signature_set_spends_nothing() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 9));
    Req::new(1, "n1", &[Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    let uses = token(&env, 1).uses;
    let before = keys(&env, "enroll/").len();
    let now = env.now();

    // The same key as n1's daemon key, for another node.
    let shared =
        Req::new(1, "n2", &[Daemon]).run(&*env.store, &mut || Ok(issue(&[Daemon], 1, now)));
    assert_eq!(shared, Err(R::BadCsr));
    // Wrong roles, a repeated role, a repeated key, a repeated serial, a refusal.
    let both = Req::new(1, "n3", &[Storage, Daemon]);
    let wrong = both.run(&*env.store, &mut || Ok(issue(&[Daemon], 5, now)));
    assert_eq!(wrong, Err(R::BadRoles));
    let twice = both.run(&*env.store, &mut || {
        Ok([issue(&[Storage], 5, now), issue(&[Storage], 6, now)].concat())
    });
    assert_eq!(twice, Err(R::BadRoles));
    let same_key = both.run(&*env.store, &mut || {
        let mut certs = issue(&[Storage, Daemon], 5, now);
        certs[1].spki_sha256 = certs[0].spki_sha256;
        Ok(certs)
    });
    assert_eq!(same_key, Err(R::BadCsr));
    let same_serial = both.run(&*env.store, &mut || {
        let mut certs = issue(&[Storage, Daemon], 5, now);
        certs[1].serial = certs[0].serial.clone();
        Ok(certs)
    });
    assert_eq!(same_serial, Err(R::BadCsr));
    // A serial that differs only by leading zeros is the same serial.
    let zeros = both.run(&*env.store, &mut || {
        let mut certs = issue(&[Storage, Daemon], 5, now);
        certs[1].serial = [0, 0]
            .iter()
            .chain(certs[0].serial.iter())
            .copied()
            .collect();
        Ok(certs)
    });
    assert_eq!(zeros, Err(R::BadCsr));
    let refused = both.run(&*env.store, &mut || Err(R::BadCsr));
    assert_eq!(refused, Err(R::BadCsr));
    assert_eq!(token(&env, 1).uses, uses);
    assert_eq!(keys(&env, "enroll/").len(), before);
}

// Catches a serial spelt with its leading zero byte, or in capitals, anywhere it is
// stored or denied: it must be exactly kbf_map::normalize_serial's spelling, or the deny
// list's exact-string match never fires.
#[test]
fn serials_are_stored_and_denied_as_the_deny_list_spells_them() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 9));
    let now = env.now();
    let req = Req::new(1, "n1", &[Daemon]);
    req.run(&*env.store, &mut || {
        let mut certs = issue(&[Daemon], 1, now);
        certs[0].serial = vec![0x00, 0x0a, 0xBC];
        Ok(certs)
    })
    .expect("enroll");
    let want = kbf_map::normalize_serial("000aBC");
    assert_eq!(want, "abc");
    assert_eq!(
        keys(&env, "enroll/serial/"),
        vec![format!("enroll/serial/{want}")]
    );
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 1);
    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    assert_eq!(
        keys(&env, "deny/serial/"),
        vec![format!("deny/serial/{want}")]
    );
    assert!(
        deny_rows(&*env.store)
            .expect("rows")
            .iter()
            .any(|d| d.entry.to_string() == "serial abc")
    );
}

// Catches a decision made on a stale read: a rival takes a use between this call's reads
// and its commit, so the commit fails; the call must read again, sign again and end with
// both nodes and both uses. (Mutant: a cached token row, or no version on the token row,
// would end with one use recorded for two nodes.)
#[test]
fn a_lost_race_is_decided_again_on_fresh_rows() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 2));
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let rival = Arc::clone(&env.store);
    let now = env.now();
    wrap.before_first_txn(move || {
        let mut sign_rival = || Ok(issue(&[Daemon], 9, now));
        Req::new(1, "rival", &[Daemon])
            .run(&*rival, &mut sign_rival)
            .expect("rival");
    });
    let signed = AtomicUsize::new(0);
    Req::new(1, "n1", &[Daemon])
        .run(&wrap, &mut || {
            signed.fetch_add(1, Ordering::SeqCst);
            Ok(issue(&[Daemon], 1, now))
        })
        .expect("enroll");
    assert_eq!(
        signed.load(Ordering::SeqCst),
        2,
        "signed again after the lost race"
    );
    assert_eq!(token(&env, 1).uses, 2);
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 2);

    // The rival takes the last use: the retry is refused rather than overspending.
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 1));
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let rival = Arc::clone(&env.store);
    wrap.before_first_txn(move || {
        let mut sign_rival = || Ok(issue(&[Daemon], 9, now));
        Req::new(1, "rival", &[Daemon])
            .run(&*rival, &mut sign_rival)
            .expect("rival");
    });
    let lost = Req::new(1, "n1", &[Daemon]).run(&wrap, &mut || Ok(issue(&[Daemon], 1, now)));
    assert_eq!(lost, Err(R::TokenUsedUp));
    assert_eq!(list_nodes(&*env.store).expect("nodes").len(), 1);
}

// Catches a handle that lost the writer role getting anything done, from a cached row or
// a read that still works: its enroll answers NotSigner and the successor sees nothing.
#[test]
fn a_fenced_handle_enrolls_nothing() {
    let env = fake();
    let stale = Fenced::take(env.store.clone(), None).expect("take");
    let fresh = Fenced::take(env.store.clone(), Some(1)).expect("take");
    make_token(&env, 1, &spec(&[Daemon], 2));
    let now = env.now();
    let req = Req::new(1, "n1", &[Daemon]);
    let stale_answer = req.run(&stale, &mut || Ok(issue(&[Daemon], 1, now)));
    assert_eq!(stale_answer, Err(R::NotSigner));
    assert_eq!(token(&env, 1).uses, 0);
    nothing_landed(&env);
    // The current handle is fine.
    req.run(&fresh, &mut || Ok(issue(&[Daemon], 1, now)))
        .expect("enroll");
    assert_eq!(token(&env, 1).uses, 1);
}

// Catches store trouble answered as anything but "try again": a backend failure on a read
// or a write, a store that never lets the commit through, and rows that do not decode are
// all StoreUnavailable (retryable), never a success or a permanent refusal.
#[test]
fn trouble_in_the_store_is_unavailable() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 2));
    let req = Req::new(1, "n1", &[Daemon]);
    let reads = Wrap::new(env.store.clone(), Mode::FailGet);
    assert_eq!(
        req.run(&reads, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    let writes = Wrap::new(env.store.clone(), Mode::FailTxn);
    let now = env.now();
    assert_eq!(
        req.run(&writes, &mut || Ok(issue(&[Daemon], 1, now))),
        Err(R::StoreUnavailable)
    );
    let busy = Wrap::new(env.store.clone(), Mode::Conflict);
    assert_eq!(
        req.run(&busy, &mut || Ok(issue(&[Daemon], 1, now))),
        Err(R::StoreUnavailable)
    );
    assert_eq!(busy.recorded().len(), kbf_enroll_registry::ATTEMPTS);
    assert_eq!(token(&env, 1).uses, 0);

    let put = |key: &str, value: &[u8]| {
        env.store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("put");
    };
    // A token with a role no token can grant; an idempotency row, a node row and an
    // advertise row that do not decode; a node row with a state this build does not know.
    let bad = fake();
    make_token(&bad, 1, &spec(&[Daemon], 2));
    bad.store
        .txn(Txn::new().put(
            "enroll/token/aaaaaaaaaaab",
            [&[0x12, 0x08][..], b"frontend"].concat(),
            Cond::Any,
        ))
        .expect("store")
        .expect("put");
    let digest_ok = |env: &Env| {
        // a token row whose roles field is bad but whose digest matches the secret [3;32]
        use sha2::Digest as _;
        let d: [u8; 32] = sha2::Sha256::digest([3u8; 32]).into();
        let mut row = vec![0x0a, 0x20];
        row.extend_from_slice(&d);
        // max_uses 5, expires_at far ahead, and roles = "frontend".
        row.extend_from_slice(&[0x18, 0x05, 0x28, 0xff, 0xff, 0xff, 0xff, 0x0f]);
        row.extend_from_slice(&[0x12, 0x08]);
        row.extend_from_slice(b"frontend");
        env.store
            .txn(Txn::new().put("enroll/token/aaaaaaaaaaac", row, Cond::Any))
            .expect("store")
            .expect("put");
    };
    digest_ok(&bad);
    let mut roles = Req::new(3, "n1", &[Daemon]);
    roles.token = kbf_types::TokenId::parse("aaaaaaaaaaac").expect("id");
    assert_eq!(
        roles.run(&*bad.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    let mut corrupt_token = Req::new(1, "n1", &[Daemon]);
    corrupt_token.token = kbf_types::TokenId::parse("aaaaaaaaaaad").expect("id");
    bad.store
        .txn(Txn::new().put("enroll/token/aaaaaaaaaaad", [0xffu8, 0xff], Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        corrupt_token.run(&*bad.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );

    let req = Req::new(1, "n1", &[Daemon]);
    let hash = request_hash(&[(Daemon, req.csrs[0].1.as_slice())]);
    let rowkey = format!("enroll/req/{}/{}", tid(1), hex(&hash));
    env.store
        .txn(Txn::new().put(&rowkey, [0xffu8, 0xff], Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    env.store
        .txn(Txn::new().put(&rowkey, vec![0x0a, 0x02, b'n', b'1'], Cond::Any))
        .expect("store")
        .expect("put");
    put("enroll/node/n1", &[0xff, 0xff]);
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    // A node row with an unknown state.
    put("enroll/node/n1", &[0x30, 0x07]);
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    // A node row naming a role nobody can hold.
    put(
        "enroll/node/n1",
        &[0x0a, 0x04, 0x0a, 0x02, b'x', b'x', 0x30, 0x01],
    );
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
    // A node row with a key that is not 32 bytes.
    let mut short_key = vec![0x0a, 0x0c, 0x0a, 0x06];
    short_key.extend_from_slice(b"daemon");
    short_key.extend_from_slice(&[0x12, 0x02, 1, 2]);
    short_key.extend_from_slice(&[0x30, 0x01]);
    put("enroll/node/n1", &short_key);
    assert_eq!(
        req.run(&*env.store, &mut never_signs()),
        Err(R::StoreUnavailable)
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Catches a framing-free hash (the CSRs concatenated): moving bytes from one CSR to the
// next would give the same idempotency key to different requests. Also that the hash does
// not depend on the order the CSRs were listed in.
#[test]
fn the_request_hash_frames_each_csr_and_ignores_listing_order() {
    let a: &[u8] = b"ab";
    let b: &[u8] = b"c";
    let moved: (&[u8], &[u8]) = (b"a", b"bc");
    assert_ne!(
        request_hash(&[(Storage, a), (Daemon, b)]),
        request_hash(&[(Storage, moved.0), (Daemon, moved.1)])
    );
    assert_eq!(
        request_hash(&[(Storage, a), (Daemon, b)]),
        request_hash(&[(Daemon, b), (Storage, a)])
    );
    assert_ne!(request_hash(&[(Storage, a)]), request_hash(&[(Daemon, a)]));
}

// Catches enrollment rows under the membership prefixes: member scans must not see them,
// and every key written lives under enroll/ or deny/.
#[test]
fn nothing_is_written_where_membership_scans() {
    let env = fake();
    make_token(&env, 1, &spec(&[Storage, Daemon], 9));
    Req::new(1, "n1", &[Storage, Daemon])
        .enroll(&env, 1)
        .expect("enroll");
    revoke_node(&*env.store, &node("n1"), "op").expect("revoke");
    for prefix in ["member/", "instance/", "down/", "map", "meta/", "boot/"] {
        assert!(keys(&env, prefix).is_empty(), "{prefix}");
    }
    assert!(
        keys(&env, "")
            .iter()
            .all(|k| k.starts_with("enroll/") || k.starts_with("deny/"))
    );
}

// Catches the key row written without Cond::Absent: another node registers the same key
// between this call's check and its commit, and the commit must fail and be decided again
// (then refused), not overwrite the other node's key row. (Mutant: Cond::Any on the key put.)
#[test]
fn a_key_registered_by_a_rival_during_the_commit_is_not_overwritten() {
    let env = fake();
    make_token(&env, 1, &spec(&[Daemon], 3));
    // The rival uses another token, so the token row's version cannot be what conflicts.
    make_token(&env, 2, &spec(&[Daemon], 3));
    let wrap = Wrap::new(env.store.clone(), Mode::Pass);
    let rival = Arc::clone(&env.store);
    let now = env.now();
    wrap.before_first_txn(move || {
        let mut sign_rival = || Ok(issue(&[Daemon], 1, now));
        Req::new(2, "rival", &[Daemon])
            .run(&*rival, &mut sign_rival)
            .expect("rival");
    });
    let late = Req::new(1, "n1", &[Daemon]).run(&wrap, &mut || {
        // The rival's key under a serial of our own, so only the key row can conflict.
        let mut certs = issue(&[Daemon], 1, now);
        certs[0].serial = vec![0x77];
        Ok(certs)
    });
    assert_eq!(late, Err(R::BadCsr));
    let nodes = list_nodes(&*env.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, node("rival"));
    assert_eq!(token(&env, 1).uses, 0);
}
