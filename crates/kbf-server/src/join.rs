//! A storage node's side of registration (`kbf storage --frontend`): register with a
//! frontend ([`Joiner::join`]), then heartbeat at the interval it names ([`Joiner::keep`]), registering again when the frontend
//! says the row lapsed, and backing off when it cannot be reached
//! (`docs/design/architecture-v2.md`, section 6). The node never touches the state store.
//!
//! **The map.** The registration answers with the frontend's map, and a heartbeat that
//! says which generation the node holds is answered with the map when the frontend's is
//! newer. Each goes into the node's [`MapHolder`] (which keeps the newest and never goes
//! back), so a node learns every bump from its own beat, with no push to lose.
//! Every call says whether the node is still rebuilding its index ([`CatchUp`], the state
//! its storage service reads): a node that registers while rebuilding enters the map as
//! `CatchingUp`, and the first heartbeat after [`CatchUp::finish`] makes it `Active`.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::Map;
use kbf_mapsync::MapHolder;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_storage::service::CatchUp;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::members::PROTOCOL;

/// The first wait after a failure.
pub const RETRY_BASE: Duration = Duration::from_secs(1);
/// The longest wait after repeated failures, `connect.rs`'s bound.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// What this node is, as it tells a frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Stable across restarts.
    pub instance_id: String,
    /// The address callers dial.
    pub addr: String,
    /// The zone label; may be empty.
    pub zone: String,
    /// Its relative capacity, at least 1.
    pub weight: u32,
}

/// What a registration gave this node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Joined {
    /// The member id the store assigned.
    pub member_id: u64,
    /// This registration's boot counter.
    pub boot: u64,
    /// How often to heartbeat.
    pub interval: Duration,
    /// The generation of the map the frontend answered with.
    pub generation: u64,
}

/// The wait after `failures` failures in a row: [`RETRY_BASE`] doubling to [`RETRY_MAX`].
#[must_use]
pub fn backoff(base: Duration, failures: u32) -> Duration {
    base.saturating_mul(1_u32.checked_shl(failures).unwrap_or(u32::MAX))
        .min(RETRY_MAX)
}

/// A node's registration with one frontend.
pub struct Joiner {
    client: MembershipClient<Channel>,
    identity: Identity,
    retry: Duration,
    catch_up: CatchUp,
    maps: Arc<MapHolder>,
}

impl Joiner {
    /// A joiner that calls `channel` as `identity`, waiting `retry` after the first
    /// failure, for a node whose index is whole.
    #[must_use]
    pub fn new(channel: Channel, identity: Identity, retry: Duration) -> Self {
        Self {
            client: MembershipClient::new(channel),
            identity,
            retry,
            catch_up: CatchUp::caught_up(),
            maps: Arc::new(MapHolder::new()),
        }
    }

    /// Keeps the maps this node is sent in `maps`, which whatever else the node runs
    /// (the checks of its storage service, its replication) reads. The joiner starts
    /// with a holder of its own.
    #[must_use]
    pub fn with_maps(mut self, maps: Arc<MapHolder>) -> Self {
        self.maps = maps;
        self
    }

    /// Puts `map`, as the frontend sent it, in the holder. One the node cannot read is
    /// warned about and ignored: the next beat sends it again.
    fn take(&self, map: Option<pb::Map>) {
        let Some(map) = map else { return };
        match Map::try_from(map) {
            Ok(map) => {
                let generation = map.generation();
                if self.maps.offer(map) {
                    tracing::info!(generation, "storage map taken");
                }
            }
            Err(error) => tracing::warn!(%error, "a storage map this node cannot read ignored"),
        }
    }

    /// The joiner of a node whose index may be being rebuilt: every call reports what
    /// `catch_up` says at the time.
    #[must_use]
    pub fn with_catch_up(mut self, catch_up: CatchUp) -> Self {
        self.catch_up = catch_up;
        self
    }

    /// Registers once.
    ///
    /// # Errors
    /// The frontend's refusal, or the call failing.
    pub async fn register(&mut self) -> Result<Joined, Status> {
        let who = &self.identity;
        let answer = self
            .client
            .register(pb::RegisterRequest {
                role: pb::Role::Storage.into(),
                instance_id: who.instance_id.clone(),
                addr: who.addr.clone(),
                weight: who.weight,
                zone: who.zone.clone(),
                protocol: PROTOCOL,
                catching_up: self.catch_up.is_catching_up(),
            })
            .await?
            .into_inner();
        self.take(answer.map.clone());
        Ok(Joined {
            member_id: answer.member_id,
            boot: answer.boot,
            interval: Duration::from_millis(answer.heartbeat_interval_ms),
            generation: answer.map.map_or(0, |map| map.generation),
        })
    }

    /// Heartbeats once, taking the frontend's map if it is newer than the node's; returns
    /// the generation of the frontend's current map.
    ///
    /// # Errors
    /// NOT_FOUND when the row lapsed (register again); the call failing.
    pub async fn beat(&mut self, joined: &Joined) -> Result<u64, Status> {
        let answer = self
            .client
            .heartbeat(pb::HeartbeatRequest {
                member_id: joined.member_id,
                boot: joined.boot,
                have_generation: self.maps.generation(),
                catching_up: self.catch_up.is_catching_up(),
            })
            .await?
            .into_inner();
        self.take(answer.map);
        Ok(answer.map_generation)
    }

    /// Registers, retrying after [`backoff`] until the frontend answers: what a storage
    /// node waits for before it opens its cache, which writes as the member and boot
    /// this returns.
    pub async fn join(&mut self) -> Joined {
        let mut failures = 0_u32;
        loop {
            match self.register().await {
                Ok(this) => {
                    tracing::info!(member = this.member_id, boot = this.boot, "registered");
                    return this;
                }
                Err(e) => {
                    tracing::warn!("registering with the frontend: {}", e.message());
                    tokio::time::sleep(backoff(self.retry, failures)).await;
                    failures = failures.saturating_add(1);
                }
            }
        }
    }

    /// Registers ([`Joiner::join`]) and heartbeats for ever, as [`Joiner::keep`] does.
    pub async fn run(mut self) {
        loop {
            let this = self.join().await;
            self.beat_until_lapse(&this).await;
        }
    }

    /// Heartbeats as `joined` for ever: a failed call waits [`backoff`] and tries
    /// again; a NOT_FOUND heartbeat registers again ([`Joiner::join`]). A registration
    /// after a lapse has a larger boot; the node's cache keeps writing as the boot it
    /// opened with, which no other process is ever given.
    pub async fn keep(mut self, mut this: Joined) {
        loop {
            self.beat_until_lapse(&this).await;
            this = self.join().await;
        }
    }

    /// Heartbeats as `this` until the frontend says the row lapsed (NOT_FOUND). A failed
    /// call waits [`backoff`] and tries again.
    async fn beat_until_lapse(&mut self, this: &Joined) {
        let mut failures = 0_u32;
        loop {
            tokio::time::sleep(this.interval).await;
            match self.beat(this).await {
                Ok(_) => failures = 0,
                Err(e) if e.code() == Code::NotFound => {
                    tracing::warn!("the membership row lapsed; registering again");
                    return;
                }
                Err(e) => {
                    tracing::warn!("heartbeat to the frontend: {}", e.message());
                    tokio::time::sleep(backoff(self.retry, failures)).await;
                    failures = failures.saturating_add(1);
                }
            }
        }
    }
}
