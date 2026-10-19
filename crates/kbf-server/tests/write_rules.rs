//! The table of `kbf_server::write_rules`, checked against keys the code writes: each
//! subsystem is run on an in-memory store and every key it leaves behind must be named by
//! a family of the table, with the rule the design gives it (section 8 of
//! `docs/design/frontend-proxy.md`). The scheduler's and the history index's keys are
//! checked by the swap test of `tests/frontend/managed/writes.rs`.

use std::sync::Arc;
use std::time::Duration;

use kbf_enroll_registry::{TokenSpec, bump_ca_generation, create_token};
use kbf_history::IndexRoot;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::update::{Policy, Release, Rows};
use kbf_server::write_rules::{Rule, classify};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{FarmTime, Role, RoleSet, TokenId, TokenSecret};

fn store() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

/// Every key in `store`, and that each is of a family the table names, fenced.
fn assert_all_fenced(store: &dyn Store, what: &str) -> usize {
    let keys: Vec<String> = store
        .scan_prefix("")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert!(!keys.is_empty(), "{what} wrote nothing");
    for key in &keys {
        let got = classify(key, &IndexRoot::default());
        assert_eq!(
            got.map(|c| c.rule),
            Some(Rule::Fenced),
            "{what} wrote {key}, which the table does not name as fenced"
        );
    }
    keys.len()
}

// Catches: a family a subsystem writes that the table lacks (the swap test would not see
// it, since it runs no registration), or one the table calls free. Membership writes the
// map, the member rows, the id and boot counters, the instance index, the formed mark and
// the bootstrap window; a node that joins after the window its `unfilled/` mark; a deny
// entry its own row. The clock is driven, so both the window and the mark are written on
// every run (the window alone used to be written only when the registrations happened to
// fall inside it).
#[test]
fn what_membership_writes_is_fenced() {
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let members = Members::new(
        Arc::clone(&store),
        MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 2,
        },
    );
    members
        .register(&Registrant {
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("register");
    let window = store.get("meta/members_forming").expect("read");
    assert!(
        window.is_some(),
        "the first member opened no bootstrap window"
    );
    assert_all_fenced(store.as_ref(), "membership inside the window");
    clock.advance(Duration::from_secs(61));
    members
        .register(&Registrant {
            instance_id: "node-2".to_owned(),
            addr: "192.0.2.2:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("register a joiner");
    assert!(
        !store.scan_prefix("unfilled/").expect("scan").is_empty(),
        "a node joining after the window left no mark"
    );
    members
        .deny(&"node node-9".parse().expect("an entry"), "test")
        .expect("deny");
    assert!(assert_all_fenced(store.as_ref(), "membership") >= 5);
}

// Catches: the same for the rolling-update rows: the desired release, the generation
// counter, the policy, the slot and a host's row.
#[test]
fn what_the_update_rows_write_is_fenced() {
    let store = store();
    let rows = Rows::new(Arc::clone(&store));
    rows.offer(&Release::new("v2"), FarmTime::from_millis(1000))
        .expect("an offer");
    rows.set_policy(&Policy::default()).expect("a policy");
    assert!(assert_all_fenced(store.as_ref(), "the update rows") >= 2);
}

// Catches: the same for enrollment: a token row and the CA generation. The registry's other
// rows (nodes, serials, keys, advertised names, requests, renewals) are written by an
// enrollment, under the same `enroll/` prefix by the key builders of its `rows.rs`.
#[test]
fn what_enrollment_writes_is_fenced() {
    let store = store();
    let spec = TokenSpec {
        roles: RoleSet::new([Role::Daemon]).expect("roles"),
        max_uses: 1,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "lab".to_owned(),
        created_by: "tester".to_owned(),
    };
    create_token(
        store.as_ref(),
        &TokenId::from_bits(1),
        &TokenSecret::from_bytes([1; 32]),
        &spec,
    )
    .expect("a token");
    bump_ca_generation(store.as_ref()).expect("a CA generation");
    assert!(assert_all_fenced(store.as_ref(), "enrollment") >= 2);
}
