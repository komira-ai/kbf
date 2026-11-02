//! A shared slot under competing heartbeats: the coordinator decides and records an order
//! under one lock, so two hosts asking at the same moment for two services of one pool cannot
//! both take its last slot.

use std::sync::Barrier;

use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, ServiceBeat, ServiceReply};
use serde_json::json;

use crate::identity::Identity;
use crate::services::Services;
use crate::testutil::{Rig, beat, id};

/// Services `a` and `b`, each on the daemon role with one slot, both in the pool `p`.
const POOLED: &str = r#"
[[service]]
name = "a"
[service.roles.daemon]
max_updating = 1
min_up = 0
pool = "p"

[[service]]
name = "b"
[service.roles.daemon]
max_updating = 1
min_up = 0
pool = "p"
"#;

fn rig(test: &str) -> Rig {
    let mut rig = Rig::new(test);
    let list = format!(
        "{}services = [\"kbf\", \"a\", \"b\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(POOLED).expect("the services"));
    rig
}

/// The heartbeat of a v2 agent running `services`, in that order, each at 1.0.
fn beat_of(services: &[&str]) -> Heartbeat {
    let mut b = beat("1.0");
    b.agent_features = vec!["heartbeat-v2".to_owned()];
    b.services = Some(
        services
            .iter()
            .map(|name| {
                serde_json::from_value::<ServiceBeat>(json!({
                    "name": name, "version": "1.0", "health": "healthy"
                }))
                .expect("a service beat")
            })
            .collect(),
    );
    b
}

fn who(host: &str) -> Identity {
    Identity {
        node_id: id(host),
        role: HostRole::DAEMON,
    }
}

/// Catches: a decision and its order recorded apart, so that two hosts asking at once each
/// see the pool's one slot free. d1 offers `a` first and d2 offers `b` first, released
/// together by a barrier after both are known: exactly one is ordered, whichever wins, and
/// the other is still told nothing on its next beat. Repeated on fresh coordinators so the
/// threads meet in different orders.
#[tokio::test]
async fn competing_heartbeats_for_two_services_of_one_pool_take_one_slot() {
    for round in 0..16 {
        let rig = rig(&format!("pool-race-{round}"));
        rig.set(r#"{"service":"a","version":"2.0"}"#).await;
        rig.set(r#"{"service":"b","version":"2.0"}"#).await;
        // Both hosts are heard within the quiet start (told nothing), so each is a member of
        // both services before the race, which comes after it.
        rig.at(1_015);
        for (host, services) in [("d1", ["a", "b"]), ("d2", ["b", "a"])] {
            let reply = rig
                .coordinator
                .heartbeat(who(host), beat_of(&services))
                .reply;
            assert_eq!(reply, ServiceReply::Nothing, "quiet");
        }
        rig.at(1_040);
        let barrier = Barrier::new(2);
        let replies: Vec<ServiceReply> = std::thread::scope(|scope| {
            let racers: Vec<_> = [("d1", ["a", "b"]), ("d2", ["b", "a"])]
                .map(|(host, services)| {
                    let (rig, barrier) = (&rig, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        rig.coordinator
                            .heartbeat(who(host), beat_of(&services))
                            .reply
                    })
                })
                .into_iter()
                .collect();
            racers
                .into_iter()
                .map(|r| r.join().expect("a racer"))
                .collect()
        });
        let ordered = replies
            .iter()
            .filter(|r| matches!(r, ServiceReply::Update { .. }))
            .count();
        assert_eq!(ordered, 1, "round {round}: {replies:?}");
        let loser = if matches!(replies[0], ServiceReply::Update { .. }) {
            ("d2", ["b", "a"])
        } else {
            ("d1", ["a", "b"])
        };
        let again = rig
            .coordinator
            .heartbeat(who(loser.0), beat_of(&loser.1))
            .reply;
        assert_eq!(again, ServiceReply::Nothing, "round {round}");
    }
}
