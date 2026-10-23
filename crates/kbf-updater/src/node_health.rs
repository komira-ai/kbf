//! The health of a storage or daemon host, from the frontend's loopback operator API
//! ([`crate::operator`]; `docs/api.md`).
//!
//! # A frontend host
//!
//! Not installed on and not judged: [`FRONTEND_REFUSAL`]. A frontend update has to move
//! traffic away and back at kbf-proxy's drain control, which is not built yet. The roll asks
//! [`HealthCheck::refusal`] before it calls the installer for a host and fails there, with
//! nothing changed on the host and none after it touched. [`NodeHealth::check`] says the same
//! ([`Health::Abandoned`]) as a second line of defence.
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
//! The daemon reports no build commit to the frontend (`Hello` carries only
//! `daemon_version`), so the commit comes from the host itself: the `running_commit` of its
//! `kbf-host-install query`, asked over ssh through the installer the roll installs with.
//! A daemon host is **healthy** when `GET /v1/nodes/{id}` (the node id the inventory lists
//! for it, else the alias) says it is `connected` and its placement is `serving`, and the
//! host's `running_commit` is the target's by the rule storage uses ([`names`]).
//!
//! A node the roll holds drained is judged by [`NodeHealth::check_held`]: the same, without
//! `serving`, which it cannot be until the roll undrains it. The roll then undrains it and
//! asks [`NodeHealth::check`], so `serving` is shown too before the host is done.
//!
//! The roll waits while the node has not registered with this frontend since it started
//! (404), is not connected, is not serving (unless held), or the host has not reported a
//! commit or reports another. It fails at once for a version that names no commit.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::health::{commit_of, names};
use crate::inventory::{Host, Role};
use crate::operator::{CHECK, OperatorApi, WHAT};
use crate::ports::{Health, HealthCheck, Installer, PortError};
use crate::version::Version;

/// What the check says of a frontend host.
pub const FRONTEND_REFUSAL: &str =
    "frontend updates need the kbf-proxy drain control, which is not built yet";

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
}

#[derive(Deserialize)]
struct Placement {
    state: String,
}

/// Health of storage and daemon hosts, over the frontend's operator API.
pub struct NodeHealth {
    api: OperatorApi,
    /// Asked what commit a daemon host runs.
    installer: Arc<dyn Installer>,
}

impl NodeHealth {
    /// A health check over a client that was made (and its files checked) already, and the
    /// installer that tells what commit a daemon host runs.
    pub fn with_api(api: OperatorApi, installer: Arc<dyn Installer>) -> NodeHealth {
        NodeHealth { api, installer }
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

    /// `held`: the roll holds the node drained, so it is not asked to be serving.
    fn daemon(&self, host: &Host, target: &Version, held: bool) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let alias = &host.alias;
        let waiting = |why: &str| Ok(Health::Waiting(format!("{alias} {why}")));
        let node_id = host.node_id.as_deref().unwrap_or(alias);
        let Some(node) = self.read::<DaemonNode>(&format!("/v1/nodes/{node_id}"))? else {
            return waiting("has not registered with the frontend since it started");
        };
        if !node.connected {
            return waiting("is not connected");
        }
        if !held && node.placement.state != "serving" {
            return Ok(Health::Waiting(format!(
                "{alias} is {}, not serving",
                node.placement.state
            )));
        }
        match self.installer.report(host)?.running_commit {
            Some(built) if names(&built, commit) => Ok(Health::Healthy),
            Some(built) => Ok(Health::Waiting(format!(
                "{alias} runs build {built}, not the target's"
            ))),
            None => waiting("has not reported the commit it runs"),
        }
    }
}

impl HealthCheck for NodeHealth {
    /// A frontend is refused before it is installed, not after (see [`FRONTEND_REFUSAL`]).
    fn refusal(&self, host: &Host) -> Option<String> {
        (host.role == Role::Frontend).then(|| FRONTEND_REFUSAL.to_owned())
    }

    /// A daemon-only host is judged as a daemon, a storage host as a storage node, and a
    /// frontend not at all.
    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        match host.role {
            Role::Frontend => Ok(Health::Abandoned(FRONTEND_REFUSAL.to_owned())),
            Role::Daemon => self.daemon(host, target, false),
            Role::Storage => self.storage(host, target),
        }
    }

    /// As [`HealthCheck::check`], but a daemon is not asked to be serving.
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        match host.role {
            Role::Daemon => self.daemon(host, target, true),
            Role::Frontend | Role::Storage => self.check(host, target),
        }
    }
}
