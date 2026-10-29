//! The health of a storage or daemon host, from the frontend's loopback operator API
//! ([`crate::operator`]; `docs/api.md`).
//!
//! # A frontend host
//!
//! A frontend that is a member of a pool of frontends is judged by the roll itself, through the
//! member's own API (`updater_frontend.rs`: ready and built from the target's commit), because
//! that needs the member's own operator port and not the operator API of the frontend this
//! module asks. [`NodeHealth::check`] says so ([`Health::Abandoned`]) if it is asked about one.
//!
//! A frontend whose inventory line lists no `instance=` and `base=` is not a member, and
//! nothing can move traffic away from it: it is not installed on and not judged
//! ([`NO_MEMBER_REFUSAL`]). The roll asks [`HealthCheck::refusal`] before it calls the
//! installer for a host and fails there, with nothing changed on the host and none after it
//! touched.
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
//! # A storage host asked by its own updater
//!
//! A host's updater has no operator token; the frontend's worker listener lets its certificate
//! read exactly one more thing about its own node, `GET /v1/nodes/{id}/storage-health`
//! (`docs/api.md`), and [`NodeHealth::for_updater`] judges a storage host from that alone.
//! The frontend has joined the node to its member (as above) and says `member` (that
//! member's own `state` and `build_commit`, and no other), `no_storage`, `unknown_node` or
//! `unlinked`, with two numbers about the storage as a whole. The host is **healthy** when
//! its member is `active` on the target's commit, at least one member is active, and no
//! blob is known to be owed a copy (`under_replicated_blobs` is zero or unknown: a number
//! that cannot be told because another member is down is not this host's to wait for). The
//! roll waits otherwise, and fails at once for `no_storage` and `unknown_node`.
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
//! A node is connected to exactly one frontend, and a host's updater reaches the frontends
//! through the farm's load balancer, which picks one per connection and which the updater
//! cannot steer. A frontend that does not hold the node answers from the shared store: the
//! node, `connected: false`, or a 404. So one answer of "not connected" proves nothing, and
//! reading one answer made the check flap with the balancer's choice. [`NodeHealth::for_updater`]
//! therefore asks again, each time on a new connection, up to [`FRONTEND_SAMPLES`] times, and
//! takes the first answer that says connected: the node is as healthy as the frontend that
//! holds it says. Only when no answer says so does the check wait ("is not connected", or
//! "has not registered"), and it fails as before when no sample could be read at all.
//!
//! The roll waits while the node has not registered with this frontend since it started
//! (404), is not connected, is not serving (unless held), or the host has not reported a
//! commit or reports another. It fails at once for a version that names no commit.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::health::{commit_of, names};
use crate::inventory::{Host, Role, valid_node_id};
use crate::operator::{CHECK, OperatorApi, WHAT};
use crate::ports::{Health, HealthCheck, Installer, PortError};
use crate::version::Version;

/// How many times a host's updater asks the frontends (through the load balancer, one new
/// connection each) whether its node is connected, before it believes that none holds it. With
/// the balancer sharing connections out in turn, two frontends are both reached by the second
/// ask; the rest covers a balancer that picks at random (one miss in 2^8 for two frontends).
pub const FRONTEND_SAMPLES: usize = 8;

/// What the check says of a frontend host that is listed without `instance=` and `base=`.
pub const NO_MEMBER_REFUSAL: &str =
    "a frontend is rolled as a pool member: list it with instance=<name> and base=<port>";

/// What the check says if it is asked about a frontend member.
pub const MEMBER_ELSEWHERE: &str = "a frontend member is judged by the roll, not here";

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

/// How a node stands to the storage members, in `GET /v1/nodes/{id}/storage-health`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Half {
    Member,
    NoStorage,
    UnknownNode,
    Unlinked,
}

/// The answer of `GET /v1/nodes/{id}/storage-health`.
#[derive(Deserialize)]
struct OwnStorage {
    half: Half,
    why: Option<String>,
    member: Option<OwnMember>,
    storage: Whole,
}

#[derive(Deserialize)]
struct OwnMember {
    id: u64,
    state: String,
    build_commit: Option<String>,
}

#[derive(Deserialize)]
struct Whole {
    members_active: usize,
    under_replicated_blobs: Option<u64>,
}

/// Where a storage host's health is read from.
#[derive(Clone, Copy)]
enum StorageView {
    /// The enrollment registry and the member list: the operator's token reads both.
    Lists,
    /// The node's own view: all a host's updater certificate may read.
    Own,
}

/// Health of storage and daemon hosts, over the frontend's operator API.
pub struct NodeHealth {
    api: OperatorApi,
    /// Asked what commit a daemon host runs.
    installer: Arc<dyn Installer>,
    storage_view: StorageView,
    /// How many answers about a daemon's node are read, at most, to find the one that says it
    /// is connected (see the module docs); 1 for a client that reaches one frontend.
    samples: usize,
}

impl NodeHealth {
    /// A health check over a client that was made (and its files checked) already, and the
    /// installer that tells what commit a daemon host runs.
    pub fn with_api(api: OperatorApi, installer: Arc<dyn Installer>) -> NodeHealth {
        NodeHealth {
            api,
            installer,
            storage_view: StorageView::Lists,
            samples: 1,
        }
    }

    /// As [`NodeHealth::with_api`], for a client that is a host's updater (no token): a storage
    /// host is judged from its own view and the two storage numbers, which its certificate may
    /// read.
    pub fn for_updater(api: OperatorApi, installer: Arc<dyn Installer>) -> NodeHealth {
        NodeHealth {
            storage_view: StorageView::Own,
            samples: FRONTEND_SAMPLES,
            ..NodeHealth::with_api(api, installer)
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

    /// A daemon's node as the frontends say it, asking up to `samples` times for an answer
    /// that says it is connected (the first is returned at once). Without one: the last
    /// answer that named the node, else a 404 if every answer was one, else the last error
    /// if no answer could be read.
    fn daemon_node(&self, path: &str) -> Result<Option<DaemonNode>, PortError> {
        let mut named = None;
        let mut answered = false;
        let mut failed = None;
        for _ in 0..self.samples.max(1) {
            match self.read::<DaemonNode>(path) {
                Ok(Some(node)) if node.connected => return Ok(Some(node)),
                Ok(Some(node)) => {
                    named = Some(node);
                    answered = true;
                }
                Ok(None) => answered = true,
                Err(e) => failed = Some(e),
            }
        }
        match failed {
            Some(e) if !answered => Err(e),
            _ => Ok(named),
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

    /// A storage host judged from its own view (see the module docs).
    fn storage_own(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let alias = &host.alias;
        let node = host
            .node_id
            .as_deref()
            .filter(|id| valid_node_id(id))
            .ok_or_else(|| PortError::other(format!("{alias} has no usable node id")))?;
        let path = format!("/v1/nodes/{node}/storage-health");
        let own: OwnStorage = self.read(&path)?.ok_or_else(|| {
            PortError::other(format!("{WHAT} keeps no storage members (404 on {path})"))
        })?;
        Ok(own_health(alias, commit, own))
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
        let Some(node) = self.daemon_node(&format!("/v1/nodes/{node_id}"))? else {
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

/// What a storage host's own view says of it, for the target's `commit`.
fn own_health(alias: &str, commit: &str, own: OwnStorage) -> Health {
    match (own.half, own.member) {
        (Half::UnknownNode, _) => Health::Abandoned(format!(
            "{alias} is not an enrolled node; its alias must be the id it enrolled with"
        )),
        (Half::NoStorage, _) => Health::Abandoned(format!(
            "{alias} advertises no storage address, so it has no storage member"
        )),
        (Half::Unlinked, _) => Health::Waiting(
            own.why
                .unwrap_or_else(|| format!("no storage member is registered at {alias}'s address")),
        ),
        (Half::Member, None) => Health::Waiting(format!("{alias}'s member is not named")),
        (Half::Member, Some(member)) => member_own_health(commit, &member, &own.storage),
    }
}

/// The health of a host whose member is `member`, with the storage as it is.
fn member_own_health(commit: &str, member: &OwnMember, storage: &Whole) -> Health {
    let id = member.id;
    if member.state != "active" {
        return Health::Waiting(format!("member m{id} is {}, not active", member.state));
    }
    match member.build_commit.as_deref() {
        None => return Health::Waiting(format!("member m{id} has not reported its build commit")),
        Some(built) if !names(built, commit) => {
            return Health::Waiting(format!("member m{id} runs build {built}, not the target's"));
        }
        Some(_) => {}
    }
    if storage.members_active == 0 {
        return Health::Waiting("no storage member is active".to_owned());
    }
    match storage.under_replicated_blobs {
        Some(owed @ 1..) => Health::Waiting(format!("{owed} blobs are still owed their copies")),
        Some(0) | None => Health::Healthy,
    }
}

impl HealthCheck for NodeHealth {
    /// A frontend that is no pool member is refused before it is installed, not after (see
    /// [`NO_MEMBER_REFUSAL`]).
    fn refusal(&self, host: &Host) -> Option<String> {
        match (host.role, &host.member) {
            (Role::Frontend, None) => Some(NO_MEMBER_REFUSAL.to_owned()),
            _ => None,
        }
    }

    /// A daemon-only host is judged as a daemon, a storage host as a storage node, and a
    /// frontend not here (see [`MEMBER_ELSEWHERE`]).
    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        match host.role {
            Role::Frontend => Ok(Health::Abandoned(MEMBER_ELSEWHERE.to_owned())),
            Role::Daemon | Role::MacDaemon => self.daemon(host, target, false),
            Role::Storage => match self.storage_view {
                StorageView::Lists => self.storage(host, target),
                StorageView::Own => self.storage_own(host, target),
            },
        }
    }

    /// As [`HealthCheck::check`], but a daemon is not asked to be serving.
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        match host.role {
            Role::Daemon | Role::MacDaemon => self.daemon(host, target, true),
            Role::Frontend | Role::Storage => self.check(host, target),
        }
    }
}
