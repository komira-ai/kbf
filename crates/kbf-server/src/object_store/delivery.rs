//! What the frontend hands the storage nodes: the registration as the nodes fetch it, and the
//! nodes' acknowledgements as the operator API shows them. The two adapters that join the
//! registry ([`super::Registry`]) and the acks ([`crate::store_config::acks::Acks`]) to the
//! seams the rest of the frontend is written against.
//!
//! - [`RegistrySource`] is the production [`StoreConfigSource`]: the `cold` binding, the
//!   connection it names and that connection's secret file, read as ONE generation. It is
//!   what `Membership` announces the generation from and what `ColdStore.GetStoreConfig`
//!   answers from.
//! - [`Reports`] is the production [`AckSource`]: the members of the map, each named by the
//!   address it registered and shown with what its last heartbeat reported.
//!
//! Neither adds a table or a key: the registry's rows and the `objstore/ack/` rows already
//! exist, and the frontend children that share the state store share these too.

use std::sync::Arc;

use kbf_map::MemberState;
use kbf_proto::kbf::enroll::v1::StoreConfig;
use kbf_types::object_store_api::{BindingRole, MemberStanding, NodeConvergence, NodeStatus};

use super::api::AckSource;
use super::{Registry, RegistryError};
use crate::members::Members;
use crate::store_config::acks::{Acks, NodeStatus as Reported, denied};
use crate::store_config::{Snapshot, SourceError, StoreConfigSource};

/// What the worker listener serves from the registry: the registration the nodes fetch and
/// the acks their heartbeats leave. One value, so `Membership` (which announces the
/// generation and records the acks) and `ColdStore` (which answers the fetch) cannot be
/// handed different ones.
#[derive(Clone)]
pub struct Delivery {
    /// Where the registration is read.
    pub source: Arc<dyn StoreConfigSource>,
    /// Where the heartbeats' reports are kept.
    pub acks: Arc<Acks>,
}

/// How many times a reading is made again because a write came between its pieces.
const READINGS: usize = 8;

/// The registry as the nodes' source.
pub struct RegistrySource {
    registry: Arc<Registry>,
}

impl RegistrySource {
    /// The source over `registry`, the one the operator API writes.
    #[must_use]
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// The `cold` binding's store with the secret, stamped `generation`; `None` when the
    /// role is not bound.
    fn read(&self, generation: u64) -> Result<Option<StoreConfig>, RegistryError> {
        let Some(binding) = self
            .registry
            .bindings()?
            .into_iter()
            .find(|b| b.role == BindingRole::Cold)
        else {
            return Ok(None);
        };
        let connection = self
            .registry
            .connection(&binding.connection)?
            .ok_or(RegistryError::NoSuchConnection)?;
        let secret = self.registry.secret(&binding.connection)?;
        Ok(Some(StoreConfig {
            connection: connection.name.to_string(),
            role: "cold".to_owned(),
            endpoint: connection.endpoint,
            region: connection.region,
            bucket: binding.bucket,
            prefix: binding.prefix,
            conditional_put: connection.conditional_put,
            min_put_rate: connection.min_put_rate.map_or(0, std::num::NonZeroU64::get),
            access_key_id: connection.access_key_id,
            secret_access_key: secret.expose().to_owned(),
            // A new generation is the only way the secret changes, so the generation is
            // the secret's version.
            secret_version: generation,
        }))
    }
}

impl StoreConfigSource for RegistrySource {
    fn generation(&self) -> Result<u64, SourceError> {
        self.registry.generation().map_err(|_| SourceError::Store)
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        // The pieces are read apart: a write between them changes the generation, and the
        // reading is made again, so a node never pairs one generation's endpoint with
        // another's key.
        for _ in 0..READINGS {
            let before = self.generation()?;
            let config = self.read(before).map_err(|e| match e {
                RegistryError::Secret(_) => SourceError::SecretUnavailable,
                _ => SourceError::Store,
            })?;
            if self.generation()? == before {
                return Ok(Snapshot {
                    // Nothing bound is nothing registered, whatever the generation of the
                    // rows that exist.
                    generation: if config.is_some() { before } else { 0 },
                    config,
                });
            }
        }
        Err(SourceError::Store)
    }
}

/// The nodes' acks as the operator API reads them.
pub struct Reports {
    members: Arc<Members>,
    acks: Arc<Acks>,
    registry: Arc<Registry>,
}

impl Reports {
    /// The reports of `members`, as `acks` keeps them, judged against `registry`'s
    /// generation.
    #[must_use]
    pub fn new(members: Arc<Members>, acks: Arc<Acks>, registry: Arc<Registry>) -> Self {
        Self {
            members,
            acks,
            registry,
        }
    }
}

impl AckSource for Reports {
    fn nodes(&self) -> Vec<NodeConvergence> {
        let Some(map) = self.members.refresh().ok().flatten() else {
            return Vec::new();
        };
        let generation = self.registry.generation().unwrap_or(0);
        let Ok(got) = self.acks.convergence(&map, generation) else {
            return Vec::new();
        };
        got.nodes
            .iter()
            .filter_map(|ack| {
                let member = map.members().iter().find(|m| m.id == ack.member)?;
                Some(NodeConvergence {
                    id: member.addr.clone(),
                    // A denied node is never handed the registration; a draining one is
                    // leaving. Every other member stays in the count: a joining or lapsed
                    // one that has not applied the generation keeps the farm from
                    // `converged`, which is the safe side.
                    state: if denied(&map, &member.addr) {
                        MemberStanding::Denied
                    } else if ack.state == MemberState::Draining {
                        MemberStanding::Draining
                    } else {
                        MemberStanding::Active
                    },
                    applied_generation: ack.applied_generation,
                    status: match ack.status {
                        Reported::Ok => NodeStatus::Ok,
                        Reported::Pending => NodeStatus::Pending,
                        Reported::ProbeFailed => NodeStatus::ProbeFailed,
                        Reported::IdentityMismatch => NodeStatus::IdentityMismatch,
                        Reported::Unsupported => NodeStatus::Unsupported,
                    },
                    last_seen_secs: ack.last_seen.map(|d| d.as_secs()),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
