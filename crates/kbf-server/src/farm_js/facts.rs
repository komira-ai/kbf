//! What the farm knows of a worker's stream that the core does not keep: the `os` and
//! `arch` its `Hello` reported and the protocol version `Welcome` named for it. The farm's
//! own driver keeps them in the worker's link; here they sit beside the adapter's
//! connections, written by the hot path and read by the node views.

use kbf_caps::NodeCaps;
use kbf_types::WorkerId;

use super::adapter::KbfAdapter;
use crate::farm::{NodeLoads, Reported, identity};

/// A worker's newest stream, as its `Hello` described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerFacts {
    /// The `os` it reported; empty if it named none.
    pub(crate) os: String,
    /// The `arch` it reported.
    pub(crate) arch: &'static str,
    /// The version `Welcome` named for its first `Hello`; `None` if the `Hello` named none.
    pub(crate) protocol: Option<u32>,
    /// The daemon process that opened the stream, as its `Hello` named itself; empty if it
    /// named none ([`KbfAdapter::note_instance`]). A node's load readings are of one session
    /// per process.
    pub(crate) instance: String,
}

impl KbfAdapter {
    /// Keeps what `reported` says of `worker`'s new stream: its `os` and `arch`, and the
    /// protocol its `Welcome` named. A stream that replaces an earlier one replaces all
    /// three.
    pub(crate) fn note_registered(&mut self, worker: &WorkerId, reported: &Reported) {
        let (os, arch) = identity(&reported.caps);
        let protocol = reported.protocol;
        let facts = WorkerFacts {
            os,
            arch,
            protocol,
            instance: String::new(),
        };
        self.facts.insert(worker.clone(), facts);
    }

    /// Keeps the daemon process `instance` that opened `worker`'s newest stream; called with
    /// [`KbfAdapter::note_registered`]. A worker that has not registered gains nothing.
    pub(crate) fn note_instance(&mut self, worker: &WorkerId, instance: &str) {
        if let Some(facts) = self.facts.get_mut(worker) {
            facts.instance = instance.to_owned();
        }
    }

    /// Takes the `os` and `arch` a resent `Hello` reports. The protocol is kept: the
    /// stream's is the first `Hello`'s, which `Welcome` answered. A worker that was never
    /// registered, or has been forgotten, gains nothing. The call takes no stream: the
    /// caller ignores a resize from a replaced stream, as the farm's own driver does.
    pub(crate) fn note_resized(&mut self, worker: &WorkerId, caps: &NodeCaps) {
        if let Some(facts) = self.facts.get_mut(worker) {
            (facts.os, facts.arch) = identity(caps);
        }
    }

    /// Forgets `worker`'s facts: its stream is gone for good. Only tests call this: the farm's
    /// own driver keeps a worker's link for as long as the farm runs, so the hot path on the
    /// js core keeps its facts too, and the node views never lose a worker's `os`, `arch` and
    /// protocol.
    #[cfg(test)]
    pub(crate) fn forget_worker(&mut self, worker: &WorkerId) {
        self.facts.remove(worker);
    }

    /// What each node's load readings have come to.
    pub(crate) fn node_loads(&self) -> &NodeLoads {
        &self.node_loads
    }

    /// [`KbfAdapter::node_loads`], to take a reading into.
    pub(crate) fn node_loads_mut(&mut self) -> &mut NodeLoads {
        &mut self.node_loads
    }

    /// What `worker`'s newest stream said of itself, if it has registered.
    pub(crate) fn worker_facts(&self, worker: &WorkerId) -> Option<&WorkerFacts> {
        self.facts.get(worker)
    }
}
