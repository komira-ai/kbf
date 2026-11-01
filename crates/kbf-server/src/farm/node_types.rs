//! The types of the operator actions on a node and of the update step's read of one:
//! what [`Farm`](super::Farm) answers with, and the errors it refuses with.

use std::time::Duration;

use kbf_types::WorkerId;

use crate::update_status::UpdateReport;

/// An operator action named a node that never registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no node {0} has registered")]
pub struct UnknownNode(pub String);

/// A read of a node another frontend of the state store holds, when the store could not be
/// read ([`Farm::node_detail`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the state store could not be read for node {0}, which another frontend holds")]
pub struct NodeUnread(pub String);

/// An operator action on a node that was not applied.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlaceError {
    /// No frontend of the state store has held the node.
    #[error(transparent)]
    Unknown(#[from] UnknownNode),
    /// The store could not say whether another frontend holds the node.
    #[error(transparent)]
    Unread(#[from] NodeUnread),
    /// The store did not take the node's `cordon/` row: the action was not applied here
    /// either, and the other frontend that holds the node never sees it.
    #[error("the state store could not be written for node {0}; the action was not applied")]
    Unwritten(String),
}

/// What an operator does to a node's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    /// Offer it no new lease; its leases run on.
    Cordon,
    /// Cordon it and wait this long for its leases to end; then the drain pauses.
    /// `Duration::MAX` ([`NO_DRAIN_DEADLINE`](crate::api::NO_DRAIN_DEADLINE)) waits for
    /// ever: the stored deadline and the scheduler's saturate, and the views show none.
    Drain(Duration),
    /// Return it to placement, ending any drain.
    Uncordon,
}

/// A node as the update node step reads it ([`Farm::update_look`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLook {
    /// Its id.
    pub id: WorkerId,
    /// Whether its newest stream is open.
    pub connected: bool,
    /// The `os` its newest `Hello` reported.
    pub os: String,
    /// The `arch` it reported.
    pub arch: &'static str,
    /// Whether placement has it cordoned (any cordon, a drain included).
    pub cordoned: bool,
    /// Leases the scheduler holds on it.
    pub leases: u32,
    /// Of those, leases whose `Start` was not sent.
    pub undelivered: u32,
    /// It is switching its session for a whole-machine lease (`Preparing`, `Restoring`).
    pub whole_machine_hold: bool,
    /// The newest `UpdateStatus` of its current stream; `None` before one.
    pub update: Option<UpdateReport>,
}
