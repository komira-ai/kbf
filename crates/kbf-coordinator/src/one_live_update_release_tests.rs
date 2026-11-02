//! One live update at a time (`decide.rs`, rule 7): when a slot one service holds is released,
//! as another service sees it. Each holder keeps the one slot until its own service's rule
//! releases it (its desired version, its `bake_beats`, the order's age, a deferral, a healthy
//! beat), and holders of several services and hosts release the fleet only when the last of
//! them does. Every time is explicit; the fleets are built beat by beat.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{DeferredReport, Health, Heartbeat, Outcome, Token};

use crate::decide::{Decision, Hold, decide_why};
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::services::Services;
use crate::testutil::{beat, id, last, secs, unhealthy, updating, version};

/// `a` bakes for two beats and wants 2.0; `b` bakes for three and wants 5.0; `c` does not
/// bake and wants 2.0; `third` has no desired state; `paused` holds its daemons
/// (`max_updating = 0`). Each runs on daemons and storage hosts, and none keeps a host up.
const SERVICES: &str = "[[service]]\nname = \"a\"\nbake_beats = 2\n\
     [service.roles.daemon]\nmin_up = 0\n[service.roles.storage]\nmin_up = 0\n\
     [[service]]\nname = \"b\"\nbake_beats = 3\n\
     [service.roles.daemon]\nmin_up = 0\n[service.roles.storage]\nmin_up = 0\n\
     [[service]]\nname = \"c\"\n\
     [service.roles.daemon]\nmin_up = 0\n[service.roles.storage]\nmin_up = 0\n\
     [[service]]\nname = \"third\"\n[service.roles.daemon]\nmin_up = 0\n\
     [[service]]\nname = \"paused\"\n[service.roles.daemon]\nmax_updating = 0\nmin_up = 0\n";

fn want(target: &str) -> Desired {
    let text = format!(r#"{{"version":"{target}","min_up":{{"storage":0,"frontend":0}}}}"#);
    Desired::parse(text.as_bytes()).expect("parsed")
}

/// The desired states: every service of [`SERVICES`] but `third`.
fn all() -> BTreeMap<String, Desired> {
    [("a", "2.0"), ("b", "5.0"), ("c", "2.0"), ("paused", "2.0")]
        .into_iter()
        .map(|(s, v)| (s.to_owned(), want(v)))
        .collect()
}

struct Farm {
    services: Services,
    all: BTreeMap<String, Desired>,
    fleet: Fleet,
}

impl Farm {
    fn new() -> Self {
        Self {
            services: Services::parse(SERVICES).expect("services"),
            all: all(),
            fleet: Fleet::new(Duration::ZERO),
        }
    }

    fn daemon(&mut self, service: &str, host: &str, b: Heartbeat, at: u64) {
        self.fleet
            .record_in(service, id(host), HostRole::DAEMON, b, secs(at));
    }

    fn storage(&mut self, service: &str, host: &str, b: Heartbeat, at: u64) {
        self.fleet
            .record_in(service, id(host), HostRole::STORAGE, b, secs(at));
    }

    fn why(&self, service: &str, host: &str, at: u64) -> Decision {
        decide_why(
            &self.services,
            &self.all,
            service,
            &self.fleet,
            &id(host),
            secs(at),
        )
    }

    /// The decision, noted as an order when it is one, as the coordinator does.
    fn ask(&mut self, service: &str, host: &str, at: u64) -> Decision {
        let decision = self.why(service, host, at);
        if let Decision::Update { version } = &decision {
            self.fleet
                .order(service, id(host), version.clone(), secs(at));
        }
        decision
    }
}

fn go(target: &str) -> Decision {
    Decision::Update {
        version: version(target),
    }
}

fn held(why: Hold) -> Decision {
    Decision::Hold(why)
}

fn pending(running: &str) -> Heartbeat {
    let mut b = beat(running);
    b.promotion_pending = true;
    b
}

fn failed_unhealthy(target: &str) -> Heartbeat {
    unhealthy(last(beat("1.0"), target, Outcome::Failed))
}

/// Behaviour: a host whose promotion of `b` is pending holds the one slot against `c` on
/// itself and `a` on a storage host, and still does after 60 s of silence. Once its beats show
/// the promotion done, it bakes `b` (three beats, the silence restarting the count): both stay
/// held with `baking`, and both may be ordered after the third healthy beat. Catches a pending
/// promotion counted only for its own service, or only on its own host.
#[test]
fn a_pending_promotion_holds_other_services_on_its_host_and_on_another_role() {
    let mut f = Farm::new();
    f.daemon("b", "d1", pending("5.0"), 100);
    f.daemon("c", "d1", beat("1.0"), 100);
    f.storage("a", "s1", beat("1.0"), 100);
    for at in [100, 160] {
        assert_eq!(f.why("c", "d1", at), held(Hold::NoSlot), "c on d1 at {at}");
        assert_eq!(f.why("a", "s1", at), held(Hold::NoSlot), "a on s1 at {at}");
    }
    for (beat_no, at) in [(1, 160), (2, 175)] {
        f.daemon("b", "d1", beat("5.0"), at);
        assert_eq!(f.why("c", "d1", at), held(Hold::Baking), "beat {beat_no}");
        assert_eq!(f.why("a", "s1", at), held(Hold::Baking), "beat {beat_no}");
    }
    f.daemon("b", "d1", beat("5.0"), 190);
    assert_eq!(f.why("c", "d1", 190), go("2.0"));
    assert_eq!(f.why("a", "s1", 190), go("2.0"));
}

/// Behaviour: `d1` updated `b` (5.0, three beats to bake) and `d2` waits for `a` (2.0, two
/// beats). The holder's version and count decide: `d2` waits through two healthy beats and is
/// free after the third; an unhealthy beat in between starts the count again. While `d1` is
/// updating, a new desired version of `b` does not free the slot; once `d1` runs 5.0 and `b`
/// wants 6.0, `d1` is no longer baking and frees it. Catches the asking service's version or
/// `bake_beats` used for another service's bake, and a bake that survives an unhealthy beat.
#[test]
fn a_bake_is_counted_with_its_own_services_version_and_beats() {
    let mut f = Farm::new();
    f.daemon("b", "d1", updating(beat("1.0"), "5.0"), 100);
    f.daemon("a", "d2", beat("1.0"), 100);
    f.all.insert("b".to_owned(), want("6.0"));
    assert_eq!(f.why("a", "d2", 100), held(Hold::NoSlot), "updating to 5.0");
    f.all.insert("b".to_owned(), want("5.0"));

    f.daemon("b", "d1", beat("5.0"), 115);
    f.daemon("b", "d1", beat("5.0"), 130);
    assert_eq!(f.why("a", "d2", 130), held(Hold::Baking), "two of three");
    f.daemon("b", "d1", unhealthy(beat("5.0")), 145);
    assert_eq!(f.why("a", "d2", 145), held(Hold::Baking), "unhealthy");
    for at in [160, 175] {
        f.daemon("b", "d1", beat("5.0"), at);
        assert_eq!(f.why("a", "d2", at), held(Hold::Baking), "again at {at}");
    }
    f.daemon("b", "d1", beat("5.0"), 190);
    assert_eq!(f.why("a", "d2", 190), go("2.0"), "baked");

    let mut moved = Farm::new();
    moved.daemon("b", "d1", beat("5.0"), 100);
    moved.daemon("a", "d2", beat("1.0"), 100);
    assert_eq!(moved.why("a", "d2", 100), held(Hold::Baking));
    moved.all.insert("b".to_owned(), want("6.0"));
    assert_eq!(moved.why("a", "d2", 100), go("2.0"), "b wants 6.0 now");
}

/// Behaviour: an order of `c` to `d1` at 100 s holds `a` on `d2` at 219 s and frees it at
/// 220 s; `d1` asking again at 160 s is told again without its order being held longer.
/// Catches an order of one service not holding another, and a repeated order that extends the
/// hold.
#[test]
fn an_order_of_one_service_holds_another_until_it_ages_out() {
    let mut f = Farm::new();
    f.daemon("c", "d1", beat("1.0"), 100);
    f.daemon("a", "d2", beat("1.0"), 100);
    assert_eq!(f.ask("c", "d1", 100), go("2.0"));
    f.daemon("c", "d1", beat("1.0"), 160);
    assert_eq!(f.ask("c", "d1", 160), go("2.0"), "told again");
    assert_eq!(f.why("a", "d2", 219), held(Hold::NoSlot));
    assert_eq!(f.why("a", "d2", 220), go("2.0"));
}

/// Behaviour: `d1` declines the order of `c` (a deferral of its target), which frees the slot
/// at once for `a` on `d2`; a deferral of another version does not. Catches a deferral that
/// keeps holding another service for the order's whole age.
#[test]
fn a_deferral_of_the_ordered_version_frees_the_slot_for_another_service() {
    let deferred = |target: &str| {
        let mut b = beat("1.0");
        b.deferred = Some(DeferredReport {
            target: version(target),
            reason: Token::parse("drain_bound").expect("a token"),
            retry_after_secs: 300,
        });
        b
    };
    let mut f = Farm::new();
    f.daemon("c", "d1", beat("1.0"), 100);
    f.daemon("a", "d2", beat("1.0"), 100);
    assert_eq!(f.ask("c", "d1", 100), go("2.0"));
    f.daemon("c", "d1", deferred("1.5"), 110);
    assert_eq!(f.why("a", "d2", 110), held(Hold::NoSlot), "another version");
    f.daemon("c", "d1", deferred("2.0"), 115);
    assert_eq!(f.why("a", "d2", 115), go("2.0"));
}

/// Behaviour: a host silent past the down limit still holds the slot while its last word was
/// updating or failed and unhealthy, and stops holding it while its last word was a bake: a
/// bake needs live beats. The silent baker holds at 144 s and frees at 145 s (45 s after its
/// last beat). Catches liveness applied to an update or a failure, and a silent bake that
/// holds the fleet for ever.
#[test]
fn a_silent_holder_keeps_the_slot_unless_it_was_only_baking() {
    for (holder, b) in [
        ("updating", updating(beat("1.0"), "2.0")),
        ("failed and unhealthy", failed_unhealthy("2.0")),
    ] {
        let mut f = Farm::new();
        f.daemon("c", "d1", b, 100);
        f.daemon("a", "d2", beat("1.0"), 100);
        assert_eq!(f.why("a", "d2", 1_000), held(Hold::NoSlot), "{holder}");
    }
    let mut f = Farm::new();
    f.daemon("b", "d1", beat("5.0"), 100);
    f.daemon("a", "d2", beat("1.0"), 100);
    assert_eq!(f.why("a", "d2", 144), held(Hold::Baking));
    assert_eq!(f.why("a", "d2", 145), go("2.0"));
}

/// Behaviour: holders of services the coordinator does not order keep the slot all the same:
/// a host updating `third`, which has no desired state, and a host whose update of `paused`
/// (its daemons held) failed and is unhealthy, each hold `c` on another host until they are
/// done or healthy. Catches the count taken only over services that can be ordered.
#[test]
fn a_service_with_no_desired_state_or_a_held_role_still_holds_its_slot() {
    let mut f = Farm::new();
    f.daemon("third", "d1", updating(beat("1.0"), "2.0"), 100);
    f.daemon("c", "d2", beat("1.0"), 100);
    assert_eq!(f.why("c", "d2", 100), held(Hold::NoSlot), "third updating");
    f.daemon("third", "d1", beat("2.0"), 115);
    assert_eq!(f.why("c", "d2", 115), go("2.0"), "third done");

    let mut f = Farm::new();
    f.daemon("paused", "d1", failed_unhealthy("2.0"), 100);
    f.daemon("c", "d2", beat("1.0"), 100);
    assert_eq!(f.why("c", "d2", 100), held(Hold::NoSlot), "paused failed");
    f.daemon("paused", "d3", beat("1.0"), 100);
    assert_eq!(
        f.why("paused", "d3", 100),
        held(Hold::NoSlot),
        "a held role"
    );
    let mut healthy = failed_unhealthy("2.0");
    healthy.health = Health::Healthy;
    f.daemon("paused", "d1", healthy, 115);
    assert_eq!(f.why("c", "d2", 115), go("2.0"), "paused healthy");
}

/// Behaviour: a fleet that a coordinator allowing several updates left with several holders,
/// one of them (`d3`) ordered `a` and baking `b` at once. `c` on `d4` is held with `no_slot`
/// while any slot is held, with `baking` once only bakes remain, and is ordered only after the
/// last bake ends. Catches a hold that lifts when the first holder or the first host is done,
/// and a host's bake of one service hidden by its slot for another.
#[test]
fn several_holders_release_the_fleet_only_when_the_last_one_does() {
    let mut f = Farm::new();
    f.daemon("a", "d1", updating(beat("1.0"), "2.0"), 100);
    f.storage("b", "s1", failed_unhealthy("5.0"), 100);
    f.daemon("c", "d2", pending("2.0"), 100);
    f.daemon("a", "d3", beat("1.0"), 100);
    f.fleet.order("a", id("d3"), version("2.0"), secs(100));
    f.daemon("b", "d3", beat("5.0"), 100);
    f.daemon("c", "d4", beat("1.0"), 100);
    assert_eq!(f.why("c", "d4", 100), held(Hold::NoSlot), "all four");

    f.daemon("a", "d1", beat("2.0"), 110);
    assert_eq!(f.why("c", "d4", 110), held(Hold::NoSlot), "d1 done");
    let mut healthy = failed_unhealthy("5.0");
    healthy.health = Health::Healthy;
    f.storage("b", "s1", healthy, 110);
    assert_eq!(f.why("c", "d4", 110), held(Hold::NoSlot), "s1 healthy");
    f.daemon("c", "d2", beat("2.0"), 110);
    assert_eq!(f.why("c", "d4", 110), held(Hold::NoSlot), "d3's order");
    f.daemon("b", "d3", beat("5.0"), 110);
    f.daemon("a", "d3", updating(beat("1.0"), "2.0"), 110);
    assert_eq!(f.why("c", "d4", 110), held(Hold::NoSlot), "d3 updating");
    f.daemon("a", "d3", beat("2.0"), 120);
    assert_eq!(f.why("c", "d4", 120), held(Hold::Baking), "only bakes left");

    f.daemon("a", "d1", beat("2.0"), 125);
    f.daemon("a", "d3", beat("2.0"), 125);
    assert_eq!(f.why("c", "d4", 125), held(Hold::Baking), "d3 bakes b");
    f.daemon("b", "d3", beat("5.0"), 130);
    assert_eq!(f.why("c", "d4", 130), go("2.0"), "the last bake ended");
}
