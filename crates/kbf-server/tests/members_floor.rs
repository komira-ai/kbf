//! The floor of the member map (`Map::min_protocol`) and what a registration is judged
//! against (`docs/design/frontend-proxy.md`, section 9). Every protocol constant is 1 today,
//! so each test stands in for a later build with `Members::with_oldest_protocol`, a build
//! that accepts storage nodes from a protocol above 1: the floor can then differ from the
//! constants and from the node's own protocol, and a judgement against the wrong one shows.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hooked::Hooked;
use kbf_server::members::{Members, MembershipConfig, MembershipError, Progress, Registrant};
use kbf_state::Store;

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

/// A frontend of a build that accepts storage nodes from protocol `oldest` on, over `store`.
fn build(store: &Arc<Hooked>, oldest: u32) -> Members {
    Members::with_oldest_protocol(Arc::clone(store) as Arc<dyn Store>, config(), oldest)
}

/// Storage node `i`, whose newest protocol is `protocol`.
fn node(i: u32, protocol: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol,
        progress: Progress::CaughtUp,
    }
}

/// The floor of the map the store holds.
fn stored_floor(members: &Members) -> u32 {
    members
        .current_map()
        .expect("read the map")
        .expect("a map")
        .min_protocol()
}

/// Catches: a registration judged against the build's own constant instead of the floor the
/// store holds. A map made by a build that accepts from 3 is taken over by a build that
/// accepts from 2 (a frontend rolled back to an older release): the node that speaks 2 is
/// refused, naming the stored floor, because the stored map says what the cluster accepts.
/// Catches too: a floor that falls to the older build's (the map the older build makes
/// still carries 3: the floor only ratchets up, so a rollback is a flag day, design 9).
#[test]
fn a_registration_is_judged_against_the_floor_in_the_stored_map() {
    let s = Hooked::new();
    build(&s, 3)
        .register(&node(1, 3))
        .expect("the first node of a build that accepts from 3");

    let older = build(&s, 2);
    let refused = older
        .register(&node(2, 2))
        .expect_err("below the stored floor");
    assert!(
        matches!(refused, MembershipError::Protocol { got: 2, min: 3 }),
        "{refused}"
    );
    let kept = older.register(&node(2, 3)).expect("at the stored floor");
    assert_eq!(
        kept.map.min_protocol(),
        3,
        "the older build's own oldest must not lower the floor"
    );
    assert_eq!(stored_floor(&older), 3);
}

/// Catches: a floor that stays where an older build left it when this build accepts only
/// newer protocols (the `max` of the stored floor and this build's oldest is dropped), and a
/// registration judged against the stored floor alone: a map made by a build that accepts
/// from 2, then a build that accepts from 3 over it. The node that speaks 2 is refused with
/// 3, the floor of the map this registration would make, not the 2 in the store; a refusal
/// changes nothing; the first map the newer build makes carries 3.
#[test]
fn a_build_that_accepts_from_later_lifts_the_floor_with_its_next_map() {
    let s = Hooked::new();
    let first = build(&s, 2).register(&node(1, 2)).expect("a");
    assert_eq!(first.map.min_protocol(), 2);

    let newer = build(&s, 3);
    let refused = newer
        .register(&node(2, 2))
        .expect_err("below the new floor");
    assert!(
        matches!(refused, MembershipError::Protocol { got: 2, min: 3 }),
        "{refused}"
    );
    let unchanged = newer.current_map().expect("read").expect("map");
    assert_eq!(
        (unchanged.generation(), unchanged.min_protocol()),
        (first.map.generation(), 2),
        "a refused registration writes no map"
    );

    let lifted = newer.register(&node(2, 3)).expect("at the new floor");
    assert_eq!(lifted.map.min_protocol(), 3);
    assert_eq!(stored_floor(&newer), 3);
}

/// Catches: a registration judged before the map is read. The floor is in the store, so a
/// store that fails is a `Store` error and not a verdict on the node; the same call with the
/// store back is the `Protocol` refusal; and a refused node was given no identity (no
/// `instance/` row, so it holds no member id or boot counter).
#[test]
fn a_store_that_fails_is_a_store_error_before_it_is_a_protocol_error() {
    let s = Hooked::new();
    let m = build(&s, 2);

    s.fail_gets.store(1, Ordering::SeqCst);
    let down = m.register(&node(1, 1)).expect_err("the store fails");
    assert!(matches!(down, MembershipError::Store(_)), "{down}");

    let up = m.register(&node(1, 1)).expect_err("below the floor");
    assert!(
        matches!(up, MembershipError::Protocol { got: 1, min: 2 }),
        "{up}"
    );
    assert_eq!(s.fake.get("instance/node-1").expect("read"), None);
}
