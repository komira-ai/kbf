//! What the farm knows of a worker's stream that the core does not keep: the `os` and
//! `arch` its `Hello` reported and the protocol version `Welcome` named for it. The farm's
//! own driver keeps them in the worker's link; here they sit beside the adapter's
//! connections, written by the hot path and read by the node views.

// The hot path of the farm on the js core calls the setters; until that is built in, only
// tests do.
#![allow(dead_code)]

use kbf_caps::NodeCaps;
use kbf_types::WorkerId;

use super::adapter::KbfAdapter;
use crate::farm::{Reported, identity};

/// A worker's newest stream, as its `Hello` described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerFacts {
    /// The `os` it reported; empty if it named none.
    pub(crate) os: String,
    /// The `arch` it reported.
    pub(crate) arch: &'static str,
    /// The version `Welcome` named for its first `Hello`; `None` if the `Hello` named none.
    pub(crate) protocol: Option<u32>,
}

impl KbfAdapter {
    /// Keeps what `reported` says of `worker`'s new stream: its `os` and `arch`, and the
    /// protocol its `Welcome` named. A stream that replaces an earlier one replaces all
    /// three.
    pub(crate) fn note_registered(&mut self, worker: &WorkerId, reported: &Reported) {
        let (os, arch) = identity(&reported.caps);
        let protocol = reported.protocol;
        self.facts
            .insert(worker.clone(), WorkerFacts { os, arch, protocol });
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

    /// Forgets `worker`'s facts: its stream is gone for good. The farm's own driver keeps a
    /// worker's link for as long as the farm runs, so a caller that mirrors it does not
    /// call this where that driver would keep the link: the node views would lose the
    /// worker's `os`, `arch` and protocol.
    pub(crate) fn forget_worker(&mut self, worker: &WorkerId) {
        self.facts.remove(worker);
    }

    /// What `worker`'s newest stream said of itself, if it has registered.
    pub(crate) fn worker_facts(&self, worker: &WorkerId) -> Option<&WorkerFacts> {
        self.facts.get(worker)
    }
}
