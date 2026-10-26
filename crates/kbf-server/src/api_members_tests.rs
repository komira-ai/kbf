//! `GET /v1/members` and why a joiner is not promoted (`blocked_on`).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use kbf_front::{Cache, closing};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};

use super::*;
use crate::StateStore;
use crate::farm::{EpochPolicy, Farm};
use crate::health::Readiness;
use crate::members::{Fill, Handoff, MembershipConfig, PROTOCOL, Progress, Registrant};
use crate::storage_health::Reported;
use kbf_front::Backend;

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// The state of an API over `members`.
fn api_over(members: Arc<Members>, store: Arc<dyn Store>) -> Arc<ApiState> {
    let farm = Farm::new(
        Backend::from(Arc::new(Cache::memory())),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig::default(),
        history: None,
    }))
    .expect("a farm");
    let (closer, _closing) = closing();
    let (drain, _goaway) = Drain::new(closer, Arc::new(Readiness::default()));
    Arc::new(ApiState {
        farm: Arc::new(farm),
        token: None,
        drain: Arc::new(drain),
        retire: Arc::default(),
        members: Some(members),
        store: None,
    })
}

async fn view(api: &Arc<ApiState>) -> serde_json::Value {
    let answer = members_view(State(Arc::clone(api))).await;
    assert_eq!(answer.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    serde_json::from_slice(&bytes).expect("JSON")
}

/// Catches: an operator left to guess why a joiner stays `joining`: the view names, for each
/// joiner, what holds it back (`blocked_on`, in words), and says nothing of a member that is
/// not one, and nothing on a frontend that does not promote joiners (the key is absent, so
/// the view of a farm without `--auto-promote` is what it was). In order: a joiner whose
/// farm has not flushed is blocked on the flush of each member; when only the settle time
/// is left it says so; once promoted the key goes.
#[tokio::test]
async fn the_view_says_why_a_joiner_is_not_promoted() {
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock.clone(), Flaw::None));
    let config = MembershipConfig {
        ttl: Duration::from_secs(3600),
        dwell: Duration::from_secs(7200),
        replication: 2,
    };
    let members = Arc::new(
        Members::new(Arc::clone(&store), config).with_handoff(Handoff {
            settle: Duration::from_secs(30),
            allow_degraded: false,
        }),
    );
    let plain = Arc::new(Members::new(Arc::clone(&store), config));
    let a = members.register(&registrant(1)).expect("a");
    let b = members.register(&registrant(2)).expect("b");
    clock.advance(Duration::from_secs(120));
    let c = members.register(&registrant(3)).expect("c");
    let api = api_over(Arc::clone(&members), Arc::clone(&store));

    let said = view(&api).await;
    let joiner = &said["members"][2];
    assert_eq!(joiner["state"], "joining");
    let blocked: Vec<&str> = joiner["blocked_on"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|r| r.as_str().expect("words"))
        .collect();
    assert_eq!(blocked.len(), 3, "every member has to flush: {blocked:?}");
    assert!(blocked[0].contains("member 1"), "{blocked:?}");
    assert!(said["members"][0].get("blocked_on").is_none());

    // Everything flushed, the fence open, the joiner's pass done: only the settle is left.
    let generation = said["generation"].as_u64().expect("generation");
    for who in [&a, &b, &c] {
        members
            .heartbeat_with(
                who.id,
                who.boot,
                Progress::CaughtUp,
                Reported::default(),
                Fill {
                    flush_clear_gen: Some(generation),
                    ..Fill::default()
                },
                None,
            )
            .expect("beat");
    }
    members.refresh().expect("refresh");
    let map = members.current_map().expect("read").expect("a map");
    let done = Fill {
        flush_clear_gen: Some(generation),
        tail_epoch_done: members.tail_epoch().expect("epoch"),
        fill_fp: crate::members::fill_fp(&map, c.id).expect("a member"),
    };
    members
        .heartbeat_with(
            c.id,
            c.boot,
            Progress::CaughtUp,
            Reported::default(),
            done,
            None,
        )
        .expect("beat");
    let said = view(&api).await;
    let blocked = said["members"][2]["blocked_on"].as_array().expect("a list");
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].as_str().expect("words").contains("settling"));

    // A frontend that does not promote has nothing to say about it.
    let silent = api_over(plain, Arc::clone(&store));
    assert!(
        view(&silent).await["members"][2]
            .get("blocked_on")
            .is_none()
    );

    clock.advance(Duration::from_secs(31));
    members
        .heartbeat_with(
            c.id,
            c.boot,
            Progress::CaughtUp,
            Reported::default(),
            done,
            None,
        )
        .expect("beat");
    let said = view(&api).await;
    assert_eq!(said["members"][2]["state"], "active");
    assert!(said["members"][2].get("blocked_on").is_none());
}
