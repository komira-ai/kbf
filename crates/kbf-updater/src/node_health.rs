//! The health of a storage or daemon host, from the frontend's loopback operator API
//! ([`crate::operator`]; `docs/api.md`).
//!
//! # A storage host
//!
//! The host's alias is the id the node enrolled with (the id safe-to-stop takes too). Its
//! storage member is found the way the server joins a node to its member: the address the
//! node advertised at enrollment (`GET /v1/enroll/nodes`, `advertise`) is the address one
//! member registered (`GET /v1/members`, `addr`). The host is **healthy** when that member is
//! `active` and its `build_commit` names the target's commit (`docs/api.md`: kbf#1495).
//!
//! A member is `active` only while its lease is current, so an `active` member is a connected
//! one; there is no separate connected flag on a member. A restarted node registers again and
//! its `build_commit` is the new build's, so the old process cannot pass for the new. The
//! daemon half of a host that runs both is not judged: the inventory does not say whether
//! there is one.
//!
//! More than one member at the address is ambiguous (safe-to-stop answers 409 for it): none
//! is taken, so a stale `down` row beside the live one cannot pass for it, nor hide it.
//!
//! The roll **waits** while the member is absent from the map (a lapsed member leaves it for a
//! while), is ambiguous, is not `active`, or has not reported a build or reports another. It **fails** at
//! once when the registry does not know the id, or the node has no storage address (it is not
//! a storage node, and no wait will make it one).
//!
//! # A daemon host
//!
//! The daemon reports no build commit (`Hello` carries only `daemon_version`), so **the
//! version is not verified for a daemon**. It is **healthy** when `GET /v1/nodes/{id}` says
//! it is `connected` and its placement is `serving`, and, when this process made the install
//! call, its newest `NodeStatus` arrived after that call. So `Healthy` for a daemon says only
//! "connected, serving, and spoke after the first install call for this version": any status
//! (a reconnect, a resend by the old process) counts. After an updater restart the install
//! time is unknown and `connected` plus `serving` alone is `Healthy`, with no freshness at
//! all. A daemon sends `NodeStatus` on
//! every stream (`docs/design/worker-protocol.md`), so a status newer than the install is a
//! daemon that restarted and reconnected; a daemon still running its old process shows an
//! older one. The status time is the server's clock and the install time the updater's; the
//! two run on the frontend host.
//!
//! The roll waits while the node has not registered with this frontend since it started
//! (404), is not connected, is not serving, or has sent no status after the install.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::health::{InstallTimes, WallClock, commit_of, names, system_wall_clock};
use crate::inventory::{Host, Role};
use crate::operator::{CHECK, OperatorApi, WHAT};
use crate::ports::{Health, HealthCheck, PortError};
use crate::version::Version;

#[derive(Deserialize)]
struct Enrolled {
    nodes: Vec<EnrolledNode>,
}

#[derive(Deserialize)]
struct EnrolledNode {
    id: String,
    advertise: String,
}

#[derive(Deserialize)]
struct Members {
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    id: u64,
    addr: String,
    state: String,
    build_commit: Option<String>,
}

#[derive(Deserialize)]
struct DaemonNode {
    connected: bool,
    placement: Placement,
    software: Option<Software>,
}

#[derive(Deserialize)]
struct Placement {
    state: String,
}

#[derive(Deserialize)]
struct Software {
    received_at_unix_ms: u64,
}

/// Health of storage and daemon hosts, over the frontend's operator API.
pub struct NodeHealth {
    api: OperatorApi,
    installs: InstallTimes,
}

impl NodeHealth {
    pub fn new(
        addr: SocketAddr,
        token_file: PathBuf,
        proxy_auth_file: Option<PathBuf>,
    ) -> Result<NodeHealth, String> {
        Ok(NodeHealth::with_api(
            OperatorApi::new(addr, token_file, proxy_auth_file)?,
            system_wall_clock(),
        ))
    }

    /// [`NodeHealth::new`] over a client made, and the wall clock given.
    pub fn with_api(api: OperatorApi, now: WallClock) -> NodeHealth {
        NodeHealth {
            api,
            installs: InstallTimes::new(now),
        }
    }

    /// `GET path` and its JSON: `None` for a 404, an error for any other status than 200.
    fn read<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, PortError> {
        let reply = self.api.get(path)?;
        match reply.status {
            200 => serde_json::from_slice(&reply.body).map(Some).map_err(|_| {
                PortError::other(format!(
                    "{WHAT}'s answer for {path} is not what it should be"
                ))
            }),
            404 => Ok(None),
            _ => Err(reply.failure(WHAT, CHECK)),
        }
    }

    fn storage(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let alias = &host.alias;
        let enrolled: Enrolled = self.read("/v1/enroll/nodes")?.ok_or_else(|| {
            PortError::other(format!(
                "{WHAT} has no enrollment registry (404 on /v1/enroll/nodes)"
            ))
        })?;
        let Some(node) = enrolled.nodes.iter().find(|n| &n.id == alias) else {
            return Ok(Health::Abandoned(format!(
                "{alias} is not an enrolled node; its alias must be the id it enrolled with"
            )));
        };
        if node.advertise.is_empty() {
            return Ok(Health::Abandoned(format!(
                "{alias} advertises no storage address, so it has no storage member; list it as a daemon"
            )));
        }
        let members: Members = self.read("/v1/members")?.ok_or_else(|| {
            PortError::other(format!(
                "{WHAT} keeps no storage members (404 on /v1/members)"
            ))
        })?;
        let mut at_address = members
            .members
            .iter()
            .filter(|m| m.addr.eq_ignore_ascii_case(&node.advertise));
        let Some(member) = at_address.next() else {
            return Ok(Health::Waiting(format!(
                "no storage member is registered at {alias}'s address"
            )));
        };
        // Two members at one address: which one is this node is not known (safe-to-stop
        // answers 409 for it, docs/api.md). Neither is taken; the roll waits out the stale
        // one's lapse.
        if at_address.next().is_some() {
            return Ok(Health::Waiting(format!(
                "more than one storage member is registered at {alias}'s address"
            )));
        }
        let id = member.id;
        if member.state != "active" {
            return Ok(Health::Waiting(format!(
                "member m{id} is {}, not active",
                member.state
            )));
        }
        Ok(match member.build_commit.as_deref() {
            Some(built) if names(built, commit) => Health::Healthy,
            Some(built) => {
                Health::Waiting(format!("member m{id} runs build {built}, not the target's"))
            }
            None => Health::Waiting(format!("member m{id} has not reported its build commit")),
        })
    }

    fn daemon(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        let alias = &host.alias;
        let waiting = |why: &str| Ok(Health::Waiting(format!("{alias} {why}")));
        let Some(node) = self.read::<DaemonNode>(&format!("/v1/nodes/{alias}"))? else {
            return waiting("has not registered with the frontend since it started");
        };
        if !node.connected {
            return waiting("is not connected");
        }
        if node.placement.state != "serving" {
            return Ok(Health::Waiting(format!(
                "{alias} is {}, not serving",
                node.placement.state
            )));
        }
        let Some(installed) = self.installs.at(alias, target) else {
            return Ok(Health::Healthy);
        };
        match node.software {
            Some(software) if software.received_at_unix_ms >= installed => Ok(Health::Healthy),
            Some(_) => waiting("has sent no status since the install, so it has not restarted"),
            None => waiting("has sent no status"),
        }
    }
}

impl HealthCheck for NodeHealth {
    fn installing(&self, host: &Host, target: &Version) {
        self.installs.note(&host.alias, target);
    }

    /// A daemon-only host is judged as a daemon; every other host as a storage node.
    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        match host.role {
            Role::Daemon => self.daemon(host, target),
            Role::Frontend | Role::Storage => self.storage(host, target),
        }
    }
}
