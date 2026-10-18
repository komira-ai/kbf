//! Tokens: what create stores, what list shows, what revoke changes.
//! Every test names the defect it catches.

mod common;

use std::time::Duration;

use common::{DAY, Mode, Wrap, fake, make_token, spec, tid};
use kbf_enroll_registry::{DEFAULT_TTL, RegistryError, create_token, list_tokens, revoke_token};
use kbf_state::{Cond, Txn};
use kbf_types::{Role, TokenSecret};

// Catches a row that keeps the secret itself, or anything but its hash: the listing never
// shows a secret and a read of the store must not either.
#[test]
fn a_token_row_holds_the_hash_of_the_secret_and_never_the_secret() {
    let env = fake();
    make_token(&env, 7, &spec(&[Role::Storage], 3));
    let (_, row) = env
        .store
        .scan_prefix("enroll/token/")
        .expect("scan")
        .remove(0);
    let secret = [7u8; 32];
    assert!(!row.value.windows(32).any(|w| w == secret));
    assert!(row.value.windows(32).any(|w| w == sha(&secret)));
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

// Catches a create or a list that drops or garbles a field: the policy a token carries
// (roles, uses, node id, prefix, names, replace, label, creator) must come back as typed.
#[test]
fn create_returns_what_list_shows_with_the_default_lifetime() {
    let env = fake();
    let mut s = spec(&[Role::Daemon, Role::Storage], 5);
    s.node_id = Some(common::node("m1"));
    s.node_prefix = Some("m".to_owned());
    s.names = vec![".example".to_owned()];
    s.replace = true;
    let made =
        create_token(&*env.store, &tid(1), &TokenSecret::from_bytes([1; 32]), &s).expect("create");
    assert_eq!(made.expires_at, env.now() + DEFAULT_TTL.as_secs());
    assert_eq!(made.created_at, env.now());
    assert_eq!((made.uses, made.max_uses, made.revoked_at), (0, 5, None));
    assert_eq!(made.node_id.as_deref(), Some("m1"));
    assert_eq!(made.node_prefix.as_deref(), Some("m"));
    assert_eq!(made.names, vec![".example".to_owned()]);
    assert!(made.replace);
    assert_eq!(
        (made.label.as_str(), made.created_by.as_str()),
        ("lab", "tester")
    );
    assert_eq!(list_tokens(&*env.store).expect("list"), vec![made]);
}

// Catches a limit that is off by one at either end, or a lifetime past 30 days: each
// bound is tried on both sides.
#[test]
fn create_refuses_what_is_out_of_range_and_accepts_the_edges() {
    let env = fake();
    let secret = TokenSecret::from_bytes([1; 32]);
    let invalid = |max_uses: u32, ttl: Option<Duration>| {
        let mut s = spec(&[Role::Storage], max_uses);
        s.ttl = ttl;
        create_token(&*env.store, &tid(9), &secret, &s)
    };
    assert!(matches!(invalid(0, None), Err(RegistryError::Invalid(_))));
    assert!(matches!(
        invalid(1001, None),
        Err(RegistryError::Invalid(_))
    ));
    assert!(matches!(
        invalid(1, Some(Duration::from_millis(999))),
        Err(RegistryError::Invalid(_))
    ));
    assert!(matches!(
        invalid(1, Some(Duration::from_secs(30 * DAY + 1))),
        Err(RegistryError::Invalid(_))
    ));
    assert!(list_tokens(&*env.store).expect("list").is_empty());
    let longest = invalid(1000, Some(Duration::from_secs(30 * DAY))).expect("the edges");
    assert_eq!(longest.expires_at, env.now() + 30 * DAY);
    let shortest = create_token(&*env.store, &tid(10), &secret, &{
        let mut s = spec(&[Role::Storage], 1);
        s.ttl = Some(Duration::from_secs(1));
        s
    })
    .expect("one second");
    assert_eq!(shortest.expires_at, env.now() + 1);
}

// Catches a create that overwrites a token of the same id: its secret would change under
// whoever holds the first.
#[test]
fn create_does_not_overwrite_an_existing_id() {
    let env = fake();
    make_token(&env, 1, &spec(&[Role::Storage], 1));
    let again = create_token(
        &*env.store,
        &tid(1),
        &TokenSecret::from_bytes([2; 32]),
        &spec(&[Role::Daemon], 2),
    );
    assert_eq!(
        again,
        Err(RegistryError::Invalid("a token with this id exists"))
    );
    let listed = list_tokens(&*env.store).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].max_uses, 1);
}

// Catches a revoke that is not recorded, one that moves the recorded time when repeated
// (audit), and one that invents a token.
#[test]
fn revoke_sets_the_time_once_and_an_unknown_id_is_not_found() {
    let env = fake();
    make_token(&env, 1, &spec(&[Role::Storage], 1));
    revoke_token(&*env.store, &tid(1)).expect("revoke");
    let first = list_tokens(&*env.store).expect("list")[0].revoked_at;
    assert_eq!(first, Some(env.now()));
    env.advance(60);
    revoke_token(&*env.store, &tid(1)).expect("revoke again");
    assert_eq!(list_tokens(&*env.store).expect("list")[0].revoked_at, first);
    assert_eq!(
        revoke_token(&*env.store, &tid(2)),
        Err(RegistryError::NotFound)
    );
}

// Catches a revoke or a list that trusts what it reads: rows that do not decode are named,
// not skipped (a skipped token would look revoked or absent to the operator).
#[test]
fn unreadable_rows_are_reported() {
    let env = fake();
    let put = |key: &str, value: &[u8]| {
        env.store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("put");
    };
    put("enroll/token/aaaaaaaaaaaa", &[0xff, 0xff]);
    assert_eq!(
        revoke_token(
            &*env.store,
            &kbf_types::TokenId::parse("aaaaaaaaaaaa").expect("id")
        ),
        Err(RegistryError::Corrupt("token"))
    );
    assert_eq!(
        list_tokens(&*env.store),
        Err(RegistryError::Corrupt("token"))
    );
    env.store
        .txn(Txn::new().delete("enroll/token/aaaaaaaaaaaa", Cond::Any))
        .expect("store")
        .expect("delete");
    put("enroll/token/not-an-id", &[]);
    assert_eq!(
        list_tokens(&*env.store),
        Err(RegistryError::Corrupt("token key"))
    );
    let env = fake();
    env.store
        .txn(Txn::new().put(
            "enroll/token/aaaaaaaaaaaa",
            // field 2 (roles) = "frontend": a role no token can grant.
            [&[0x12, 0x08][..], b"frontend"].concat(),
            Cond::Any,
        ))
        .expect("store")
        .expect("put");
    assert_eq!(
        list_tokens(&*env.store),
        Err(RegistryError::Corrupt("token"))
    );
}

// Catches a store failure turned into success, and a retry loop that never ends under a
// writer that always wins.
#[test]
fn store_failures_and_endless_conflicts_are_errors() {
    let env = fake();
    make_token(&env, 1, &spec(&[Role::Storage], 1));
    let down = Wrap::new(env.store.clone(), Mode::FailTxn);
    assert!(matches!(
        revoke_token(&down, &tid(1)),
        Err(RegistryError::Store(_))
    ));
    let busy = Wrap::new(env.store.clone(), Mode::Conflict);
    assert_eq!(revoke_token(&busy, &tid(1)), Err(RegistryError::Contended));
    assert_eq!(busy.recorded().len(), kbf_enroll_registry::ATTEMPTS);
    assert!(matches!(
        create_token(
            &down,
            &tid(5),
            &TokenSecret::from_bytes([5; 32]),
            &spec(&[Role::Daemon], 1)
        ),
        Err(RegistryError::Store(_))
    ));
}

// Catches names and a node prefix stored unchecked: a name with `@`, a trailing dot, an
// over-long label, an address as a suffix or a prefix no node id can start with would
// never match or would match too much. Mixed case is accepted (matching folds it).
#[test]
fn create_refuses_names_and_prefixes_that_are_no_hosts_or_ids() {
    let env = fake();
    let secret = TokenSecret::from_bytes([1; 32]);
    let make = |names: &[&str], prefix: Option<&str>| {
        let mut s = spec(&[Role::Storage], 1);
        s.names = names.iter().map(|n| (*n).to_owned()).collect();
        s.node_prefix = prefix.map(str::to_owned);
        create_token(&*env.store, &tid(7), &secret, &s)
    };
    let long = "a".repeat(64);
    for bad in [
        "x@trusted.example.com",
        "trusted.example.com.",
        ".trusted.example.com.",
        "..example",
        ".",
        "",
        "a_b.example",
        "-a.example",
        "a b",
        ".203.0.113.1",
        format!("{long}.example").as_str(),
        format!(".{long}.example").as_str(),
    ] {
        assert!(
            matches!(make(&[bad], None), Err(RegistryError::Invalid(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        make(&["ok.example", "x@y"], None),
        Err(RegistryError::Invalid(_))
    ));
    for bad in ["", "-lab", "Lab", "lab_", "a.b", "a".repeat(64).as_str()] {
        assert!(
            matches!(make(&[], Some(bad)), Err(RegistryError::Invalid(_))),
            "{bad:?}"
        );
    }
    assert!(list_tokens(&*env.store).expect("list").is_empty());
    make(
        &["Fixed.example", ".farm.test", "203.0.113.1", "localhost"],
        Some("lab-"),
    )
    .expect("good names and prefix");
}
