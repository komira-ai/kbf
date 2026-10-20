//! The maps a frontend keeps from before a change (`kbf_server::members`): a blob written
//! before a join or a drain is found where it was for as long as the pins last, across
//! quick bumps and across a frontend restart; and a join or a drain past the cap on pins is
//! refused. Every test runs on the fake store with its injectable clock.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{Map, MemberId, MemberState};
use kbf_route::MapSource as _;
use kbf_server::front_maps::FrontMaps;
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_state::Store;
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

const SECOND: Duration = Duration::from_secs(1);

/// One owner per digest; a pin is held for the two minutes of the dwell.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(600),
        dwell: Duration::from_secs(120),
        replication: 1,
    }
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// A frontend over `store` that has seen nothing yet: what a restart leaves.
fn frontend(store: &Arc<Hooked>) -> Arc<Members> {
    Arc::new(Members::new(Arc::clone(store) as Arc<dyn Store>, config()))
}

fn generations(maps: &[Arc<Map>]) -> Vec<u64> {
    maps.iter().map(|m| m.generation()).collect()
}

fn pins(store: &Hooked) -> usize {
    store.fake.scan_prefix("handoff/pin/").expect("scan").len()
}

/// The maps of three registrations in a row (generations 1, 2 and 3), and a digest that
/// each of them places on a different node: node 1 at generation 1, node 2 at 2 and node
/// 3 at 3. A blob written at generation 1 sits on node 1 alone.
fn three_bumps(m: &Members) -> (Vec<Map>, Digest) {
    let maps: Vec<Map> = (1..=3)
        .map(|i| m.register(&node(i)).expect("register").map)
        .collect();
    let first = |map: &Map, d: &Digest| map.read_order(d)[0];
    let digest = (0_u64..2000)
        .map(|i| {
            let hash: [u8; 32] = Sha256::digest(i.to_be_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, 1000 + i)
        })
        .find(|d| (0..3).all(|i| first(&maps[i], d) == MemberId(i as u64 + 1)))
        .expect("a digest that moves at every bump");
    (maps, digest)
}

fn finds_node_one(map: &Map, history: &[&Map], digest: &Digest) -> bool {
    map.read_order_with_history(history, digest)
        .iter()
        .any(|member| member.id == MemberId(1))
}

/// Catches a frontend that keeps one previous map: after two quick bumps the map a blob
/// was written under is gone, and the blob reads as missing (it is only on node 1, which
/// neither of the two newest maps names for it).
#[test]
fn two_quick_bumps_keep_the_map_the_blob_was_written_under() {
    let s = Hooked::new();
    let m = frontend(&s);
    let (_, digest) = three_bumps(&m);
    let view = FrontMaps::new(Arc::clone(&m)).current().expect("a view");
    assert_eq!(view.map.generation(), 3);
    assert_eq!(view.previous.as_deref().map(Map::generation), Some(2));
    assert_eq!(generations(&view.earlier), [1]);
    assert!(finds_node_one(&view.map, &view.history(), &digest));
    assert!(
        !finds_node_one(&view.map, &view.history()[..1], &digest),
        "one map back is not enough: that is what this test is about"
    );
}

/// Catches a restarted frontend that knows only the newest map: it has seen no bump, so
/// nothing in memory says where a blob was before; the pins in the store do.
#[test]
fn a_restarted_frontend_has_the_maps_before_the_change() {
    let s = Hooked::new();
    let (_, digest) = three_bumps(&frontend(&s));

    let restarted = frontend(&s);
    let maps = FrontMaps::new(Arc::clone(&restarted));
    restarted.map_source().expect("seeded from the store");
    let view = maps.current().expect("a view");
    assert_eq!(view.map.generation(), 3);
    assert_eq!(view.previous.as_deref().map(Map::generation), Some(2));
    assert_eq!(generations(&view.earlier), [1]);
    assert!(finds_node_one(&view.map, &view.history(), &digest));
}

/// Catches a frontend that learns only the newest generation when another frontend bumps
/// twice between two of its reads: the generation in the middle was never in its memory.
#[test]
fn a_generation_another_frontend_skipped_past_is_pinned() {
    let s = Hooked::new();
    let m = frontend(&s);
    m.register(&node(1)).expect("a");
    let elsewhere = Members::new(Arc::clone(&s.fake) as Arc<dyn Store>, config());
    elsewhere.register(&node(2)).expect("b");
    elsewhere.register(&node(3)).expect("c");
    m.current_map().expect("read");
    let (latest, history) = m.view().expect("a view");
    assert_eq!(latest.generation(), 3);
    assert_eq!(generations(&history), [2, 1]);
}

/// Catches pins that never lapse (the maps would be kept for ever) and ones that lapse
/// before the dwell (a handoff under way would lose them).
#[test]
fn a_pin_is_held_for_the_dwell() {
    let s = Hooked::new();
    three_bumps(&frontend(&s));
    s.clock.advance(100 * SECOND);
    let held = frontend(&s);
    held.current_map().expect("read");
    assert_eq!(generations(&held.view().expect("a view").1), [2, 1]);

    s.clock.advance(21 * SECOND);
    let lapsed = frontend(&s);
    lapsed.current_map().expect("read");
    assert!(lapsed.view().expect("a view").1.is_empty());
    assert_eq!(pins(&s), 0);
}

/// Catches a history, or a store of pins, that grows without a bound: seven bumps leave
/// the four newest earlier maps, in memory and in the store.
#[test]
fn the_history_and_the_pins_are_bounded() {
    let s = Hooked::new();
    let m = frontend(&s);
    for i in 1..=7 {
        m.register(&node(i)).expect("register");
    }
    assert_eq!(pins(&s), 4);
    assert_eq!(generations(&m.view().expect("a view").1), [6, 5, 4, 3]);
    let restarted = frontend(&s);
    restarted.current_map().expect("read");
    assert_eq!(
        generations(&restarted.view().expect("a view").1),
        [6, 5, 4, 3]
    );
}

/// Catches a cap that is not enforced, or that refuses what it must not: with four pins
/// open a new joiner gets no identity and no row, a member that is not yet draining is
/// refused a drain, and a restart of a known member, a repeated drain and a pin that has
/// lapsed are all let through.
#[test]
fn a_join_or_a_drain_past_the_cap_is_refused() {
    let s = Hooked::new();
    let m = frontend(&s);
    m.register(&node(1))
        .expect("the first member forms the farm");
    // Past the bootstrap window the farm holds data, and a new instance is a joiner.
    s.clock.advance(61 * SECOND);
    let two = m.register(&node(2)).expect("a joiner");
    let three = m.register(&node(3)).expect("a joiner");
    m.register(&node(4)).expect("a joiner");
    assert_eq!(pins(&s), 3);
    m.set_state(two.id, MemberState::Draining)
        .expect("a drain below the cap");
    assert_eq!(pins(&s), 4);

    let instances = s.fake.scan_prefix("instance/").expect("scan").len();
    let error = m.register(&node(5)).expect_err("a fifth joiner");
    assert!(
        matches!(error, MembershipError::PinsFull { held: 4, cap: 4 }),
        "{error:?}"
    );
    assert_eq!(
        s.fake.scan_prefix("instance/").expect("scan").len(),
        instances
    );
    assert!(
        m.current_map()
            .expect("read")
            .expect("a map")
            .member(MemberId(5))
            .is_none()
    );

    let error = m
        .set_state(three.id, MemberState::Draining)
        .expect_err("a second drain");
    assert!(
        matches!(error, MembershipError::PinsFull { .. }),
        "{error:?}"
    );
    assert_eq!(
        m.current_map()
            .expect("read")
            .expect("a map")
            .member(three.id)
            .map(|x| x.state),
        Some(MemberState::Joining)
    );

    // A member that is known restarts, and a drain that is under way is asked again.
    m.register(&node(1))
        .expect("a known member registers again");
    m.set_state(two.id, MemberState::Draining)
        .expect("the same drain again");

    s.clock.advance(121 * SECOND);
    m.register(&node(5)).expect("the pins lapsed");
    m.set_state(three.id, MemberState::Draining)
        .expect("the pins lapsed");
}

/// Catches a cap that bites a farm that is still forming: inside the bootstrap window the
/// farm holds no data, so a handoff has nothing to move, and five nodes registered in a row
/// (four pins) can still be joined by a sixth and drained.
#[test]
fn a_farm_that_holds_no_data_has_no_cap() {
    let s = Hooked::new();
    let m = frontend(&s);
    let ids: Vec<MemberId> = (1..=5)
        .map(|i| m.register(&node(i)).expect("register").id)
        .collect();
    assert_eq!(pins(&s), 4);
    m.register(&node(6)).expect("a sixth node while forming");
    m.set_state(ids[1], MemberState::Draining)
        .expect("a drain while forming");
}
