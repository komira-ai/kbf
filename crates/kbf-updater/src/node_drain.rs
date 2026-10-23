//! Taking a daemon host's node out of placement and giving it back, over the frontend's
//! loopback operator API ([`crate::operator`]; `docs/api.md`, "`POST /v1/nodes/{node}:cordon`,
//! `:drain`, `:uncordon`"). The calls are the ones of `kbf cluster drain` and `kbf cluster
//! undrain`.
//!
//! - **drain**: `POST /v1/nodes/<node>:drain` with `{"deadline_secs": N}`. The frontend
//!   cordons the node (no new lease; its leases run on) and waits for the leases for `N`
//!   seconds. The call returns at once with the node as it is then.
//! - **leases**: `GET /v1/nodes/<node>`, the length of `leases`. A node that is `serving`
//!   is not drained (somebody returned it), and that is an error, not zero leases.
//! - **undrain**: `POST /v1/nodes/<node>:uncordon`.
//!
//! The node is the host's `node_id` from the inventory, a DNS label, so it stands as one path
//! segment as it is. A host without one, or with one that is not a label, is refused here
//! as well as by the roll's identity check, so that no path is built from free text.

use std::time::Duration;

use serde::Deserialize;

use crate::inventory::{Host, valid_node_id};
use crate::operator::{CHECK, OperatorApi, WHAT};
use crate::ports::{Drain, PortError};
use crate::reply::Reply;

/// Draining over the operator API.
pub struct NodeDrain {
    api: OperatorApi,
}

#[derive(Deserialize)]
struct Node {
    placement: Placement,
    leases: Vec<String>,
}

#[derive(Deserialize)]
struct Placement {
    state: String,
}

impl NodeDrain {
    pub fn with_api(api: OperatorApi) -> NodeDrain {
        NodeDrain { api }
    }

    fn node(host: &Host) -> Result<&str, PortError> {
        host.node_id
            .as_deref()
            .filter(|id| valid_node_id(id))
            .ok_or_else(|| PortError::other(format!("{} has no usable node id", host.alias)))
    }

    /// A 200 is `Ok`; a 404 says the frontend does not know the node; anything else is the
    /// failure of the status.
    fn judged(reply: Reply, node: &str) -> Result<Reply, PortError> {
        match reply.status {
            200 => Ok(reply),
            404 => Err(PortError::other(format!(
                "{WHAT} does not know node {node} (it has not registered since the frontend started)"
            ))),
            _ => Err(reply.failure(WHAT, CHECK)),
        }
    }
}

impl Drain for NodeDrain {
    fn drain(&self, host: &Host, deadline: Duration) -> Result<(), PortError> {
        let node = Self::node(host)?;
        let body = serde_json::json!({ "deadline_secs": deadline.as_secs() }).to_string();
        let reply = self.api.post(&format!("/v1/nodes/{node}:drain"), &body)?;
        Self::judged(reply, node).map(drop)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        let node = Self::node(host)?;
        let reply = Self::judged(self.api.get(&format!("/v1/nodes/{node}"))?, node)?;
        let read: Node = serde_json::from_slice(&reply.body).map_err(|_| {
            PortError::other(format!(
                "{WHAT}'s answer for node {node} is not what it should be"
            ))
        })?;
        if read.placement.state == "serving" {
            return Err(PortError::other(format!(
                "node {node} is serving again; it is no longer drained"
            )));
        }
        Ok(read.leases.len())
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        let node = Self::node(host)?;
        let reply = self.api.post(&format!("/v1/nodes/{node}:uncordon"), "")?;
        Self::judged(reply, node).map(drop)
    }
}
