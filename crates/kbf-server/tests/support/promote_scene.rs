//! The scene of the automatic promotion tests (`members_auto_promote.rs`,
//! `members_promote_guards.rs`): a farm of two members that formed it and a third that
//! joined it, on the fake store with its injectable clock, and the beats each node would
//! make.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use super::hooked::Hooked;
use kbf_map::{Map, MemberId, MemberState};
use kbf_server::members::{
    Block, Fill, Handoff, Members, MembershipConfig, PROTOCOL, Progress, Registrant, Registration,
    fill_fp,
};
use kbf_server::storage_health::Reported;
use kbf_state::Store;
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

pub const SECOND: Duration = Duration::from_secs(1);
/// The settle time of every test.
pub const SETTLE: Duration = Duration::from_secs(30);
/// Longer than the freshness of a report (two beats of a 3600 s lease).
pub const STALE: Duration = Duration::from_secs(2500);
pub const DWELL: Duration = Duration::from_secs(7200);

pub fn config(replication: u8) -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(3600),
        dwell: DWELL,
        replication,
    }
}

pub fn handoff() -> Handoff {
    Handoff {
        settle: SETTLE,
        allow_degraded: false,
    }
}

pub fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

pub fn digests() -> Vec<Digest> {
    (0_u64..300)
        .map(|i| {
            let hash: [u8; 32] = Sha256::digest(i.to_be_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, 1000 + i)
        })
        .collect()
}

pub fn owned(map: &Map, id: MemberId) -> usize {
    digests()
        .iter()
        .filter(|d| map.write_owners(d).contains(&id))
        .count()
}

/// A farm of two members that formed it, long enough ago that it holds data, and a third
/// that joined it: `Joining`, with its mark, as the farm's frontend sees it.
pub struct Scene {
    pub store: Arc<Hooked>,
    /// The frontend that promotes joiners.
    pub m: Arc<Members>,
    pub a: Registration,
    pub b: Registration,
    pub c: Registration,
    /// The blobs the farm held before the join and the members that owned each.
    pub before: Vec<(Digest, Vec<MemberId>)>,
}

pub fn members(store: &Arc<Hooked>, replication: u8, handoff: Option<Handoff>) -> Arc<Members> {
    let m = Members::new(Arc::clone(store) as Arc<dyn Store>, config(replication));
    Arc::new(match handoff {
        Some(handoff) => m.with_handoff(handoff),
        None => m,
    })
}

pub fn scene_with(replication: u8, handoff: Option<Handoff>) -> Scene {
    let store = Hooked::new();
    let m = members(&store, replication, handoff);
    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    store.clock.advance(Duration::from_secs(120));
    let before_map = m.current_map().expect("read").expect("a map");
    let before = digests()
        .into_iter()
        .map(|d| {
            let owners = before_map.write_owners(&d);
            (d, owners)
        })
        .collect();
    let c = m.register(&node(3)).expect("c");
    assert_eq!(state_of(&c.map, c.id), Some(MemberState::Joining));
    Scene {
        store,
        m,
        a,
        b,
        c,
        before,
    }
}

pub fn scene() -> Scene {
    scene_with(2, Some(handoff()))
}

pub fn state_of(map: &Map, id: MemberId) -> Option<MemberState> {
    map.member(id).map(|m| m.state)
}

pub fn flushed_to(generation: u64) -> Fill {
    Fill {
        flush_clear_gen: Some(generation),
        ..Fill::default()
    }
}

impl Scene {
    pub fn map(&self) -> Map {
        self.m.current_map().expect("read").expect("a map")
    }

    pub fn state(&self) -> Option<MemberState> {
        state_of(&self.map(), self.c.id)
    }

    pub fn beat(&self, m: &Members, who: &Registration, progress: Progress, fill: Fill) {
        m.heartbeat_with(who.id, who.boot, progress, Reported::default(), fill, None)
            .expect("beat");
    }

    /// Every member reports that everything it staged before map `generation` is flushed.
    pub fn flush_all(&self, generation: u64) {
        for who in [&self.a, &self.b, &self.c] {
            self.beat(&self.m, who, Progress::CaughtUp, flushed_to(generation));
        }
    }

    /// The flush barrier of the join's map holds, the sweeper runs, and the fence is open:
    /// the epoch it opened.
    pub fn open_fence(&self) -> u64 {
        self.flush_all(self.map().generation());
        self.m.refresh().expect("refresh");
        let epoch = self.m.tail_epoch().expect("epoch");
        assert!(epoch > 0, "the fence did not open");
        epoch
    }

    /// What the joiner's node says once it finished the pass for `epoch` under the map as
    /// it is now.
    pub fn done(&self, epoch: u64) -> Fill {
        Fill {
            flush_clear_gen: Some(self.map().generation()),
            tail_epoch_done: epoch,
            fill_fp: fill_fp(&self.map(), self.c.id).expect("a member"),
        }
    }

    pub fn beat_joiner(&self, progress: Progress, fill: Fill) {
        self.beat(&self.m, &self.c, progress, fill);
    }

    /// The joiner beats caught up, saying `fill`, `after` the clock moved on.
    pub fn joiner_after(&self, after: Duration, fill: Fill) {
        self.store.clock.advance(after);
        self.beat_joiner(Progress::CaughtUp, fill);
    }

    /// Whether every blob that existed before the join can still be found through each
    /// member that owned it, by the read order of the map and the maps before it.
    pub fn reads_find_every_old_blob(&self) -> bool {
        let (latest, history) = self.m.view().expect("the maps seen");
        let history: Vec<&Map> = history.iter().map(|m| &**m).collect();
        self.before.iter().all(|(digest, owners)| {
            let order: Vec<MemberId> = latest
                .read_order_with_history(&history, digest)
                .iter()
                .map(|m| m.id)
                .collect();
            owners.iter().all(|o| order.contains(o))
        })
    }

    pub fn blocked(&self) -> Vec<Block> {
        self.m.blocked_on(self.c.id).expect("blocked_on")
    }

    /// The beats that fill a joiner and leave it one settle short of promotion: the fence
    /// is open, the joiner acknowledges it at once and again 29 seconds later.
    pub fn nearly_promoted(&self) -> u64 {
        let epoch = self.open_fence();
        self.beat_joiner(Progress::CaughtUp, self.done(epoch));
        self.joiner_after(SETTLE - SECOND, self.done(epoch));
        assert_eq!(self.state(), Some(MemberState::Joining), "one second early");
        epoch
    }
}
