//! A storage node's side of registration (`kbf storage --frontend`): register with a
//! frontend, then heartbeat at the interval it names, registering again when the frontend
//! says the row lapsed, and backing off when it cannot be reached
//! (`docs/design/architecture-v2.md`, section 6). The node never touches the state store.
//!
//! Every call says whether the node is still rebuilding its index ([`CatchUp`], the state
//! its storage service reads): a node that registers while rebuilding enters the map as
//! `CatchingUp`, and the first heartbeat after [`CatchUp::finish`] makes it `Active`.

use std::time::Duration;

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
        Ok(Joined {
            member_id: answer.member_id,
            boot: answer.boot,
            interval: Duration::from_millis(answer.heartbeat_interval_ms),
            generation: answer.map.map_or(0, |map| map.generation),
        })
    }

    /// Heartbeats once; returns the generation of the frontend's current map.
    ///
    /// # Errors
    /// NOT_FOUND when the row lapsed (register again); the call failing.
    pub async fn beat(&mut self, joined: &Joined) -> Result<u64, Status> {
        let answer = self
            .client
            .heartbeat(pb::HeartbeatRequest {
                member_id: joined.member_id,
                boot: joined.boot,
                catching_up: self.catch_up.is_catching_up(),
            })
            .await?;
        Ok(answer.into_inner().map_generation)
    }

    /// Registers and heartbeats for ever. A failed call waits [`backoff`] and tries
    /// again; a NOT_FOUND heartbeat registers again.
    pub async fn run(mut self) {
        let mut failures = 0_u32;
        loop {
            let this = loop {
                match self.register().await {
                    Ok(this) => break this,
                    Err(e) => {
                        tracing::warn!("registering with the frontend: {}", e.message());
                        tokio::time::sleep(backoff(self.retry, failures)).await;
                        failures = failures.saturating_add(1);
                    }
                }
            };
            failures = 0;
            tracing::info!(member = this.member_id, boot = this.boot, "registered");
            loop {
                tokio::time::sleep(this.interval).await;
                match self.beat(&this).await {
                    Ok(_) => failures = 0,
                    Err(e) if e.code() == Code::NotFound => {
                        tracing::warn!("the membership row lapsed; registering again");
                        break;
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
}
