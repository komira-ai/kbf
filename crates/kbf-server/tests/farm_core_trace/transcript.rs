//! The text of a scripted session, and what the drivers of the scripts share to write it:
//! the state store's tape, the action cache, the nodes' inboxes and the callers' stages.
//!
//! A driver (the farm's, in `session.rs`, the scheduler core's, in `tests/farm_core_adapter`)
//! makes the calls and says what each was ([`Transcript::say`], [`Transcript::note`]); after
//! each call [`Transcript::sync`] writes down what the call led to, in the order
//! `session.rs` describes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kbf_front::{Cache, MemoryMetaLog, Stage};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::Digest as ProtoDigest;
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_types::LeaseId;
use tokio::sync::{mpsc, watch};
use tonic::Status;

use crate::tape::{Normalise, Tape, as_written};
use crate::text::{Job, digest_of, message_text, stage_text};

/// The action cache of a scripted session.
pub type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// A node's stream: what the server sends it, and the seq of its next heartbeat.
pub struct Node<S> {
    /// The stream the server numbered.
    pub stream: S,
    /// What the server sends on it.
    pub inbox: mpsc::UnboundedReceiver<Result<ServerMessage, Status>>,
    /// The seq of the last heartbeat it sent.
    pub beat: u64,
    /// The drivers its `Hello` named.
    pub drivers: Vec<String>,
}

/// A caller's stage, watched for changes.
struct Watched {
    label: String,
    stage: watch::Receiver<Stage>,
    last: String,
}

/// A `Start` a node was sent.
struct Seen {
    node: String,
    label: &'static str,
    lease: LeaseId,
    digest: ProtoDigest,
}

/// The server, as a script sees it from outside: the store under it, and the text of
/// everything the script did.
pub struct Transcript<S> {
    /// The state store the server writes through.
    pub tape: Arc<Tape>,
    /// The action cache it answers into.
    pub cache: Arc<MemoryCache>,
    out: Vec<String>,
    /// The connected nodes by name.
    pub nodes: BTreeMap<String, Node<S>>,
    watched: Vec<Watched>,
    seen: Vec<Seen>,
    labels: BTreeMap<String, &'static str>,
    tags: BTreeMap<&'static str, u8>,
    submits: BTreeMap<&'static str, u32>,
    /// The operation name each caller was given.
    pub names: BTreeMap<String, String>,
    cached: BTreeSet<&'static str>,
    /// How far the script has moved the clock on, in milliseconds.
    pub t_ms: u64,
}

impl<S: Copy> Transcript<S> {
    /// An empty transcript of a script called `title`, over an empty store whose tape writes
    /// values down as `normalise` says.
    pub fn new(title: &str, normalise: Normalise) -> Self {
        Self {
            tape: Arc::new(Tape::new(normalise)),
            cache: Arc::new(Cache::memory()),
            out: vec![format!("# {title}")],
            nodes: BTreeMap::new(),
            watched: Vec::new(),
            seen: Vec::new(),
            labels: BTreeMap::new(),
            tags: BTreeMap::new(),
            submits: BTreeMap::new(),
            names: BTreeMap::new(),
            cached: BTreeSet::new(),
            t_ms: 0,
        }
    }

    /// The text, one line per line, ending in a newline.
    pub fn text(&self) -> String {
        let mut text = self.out.join("\n");
        text.push('\n');
        text
    }

    /// Starts the lines of a call.
    pub fn say(&mut self, call: &str) {
        self.out.push(format!("> [t={}ms] {call}", self.t_ms));
    }

    /// A line of what the call answered.
    pub fn note(&mut self, line: String) {
        self.out.push(format!("  {line}"));
    }

    /// Notes that `job` is submitted once more, and returns how many times it has been.
    pub fn submitted(&mut self, job: &Job) -> u32 {
        let count = self.submits.entry(job.label).or_default();
        *count += 1;
        let proto = kbf_front::digest_to_proto(&job.digest());
        self.labels.insert(proto.hash, job.label);
        self.tags.insert(job.label, job.tag);
        *count
    }

    /// Follows the stage of `stage` as `label` from now on.
    pub fn follow(&mut self, label: String, stage: watch::Receiver<Stage>) {
        self.watched.push(Watched {
            label,
            stage,
            last: String::new(),
        });
    }

    /// The process dies: its nodes and callers die with it, and the store, on the same tape,
    /// restarts. The cache stays.
    pub fn restart(&mut self) {
        self.nodes.clear();
        self.watched.clear();
        let old = std::mem::replace(&mut self.tape, Arc::new(Tape::new(as_written)));
        let old = Arc::into_inner(old).expect("nothing but the dead server held the store");
        self.tape = Arc::new(old.restart());
    }

    /// The lease the newest `Start` of `label` named.
    pub fn lease_of(&self, label: &str) -> LeaseId {
        self.seen
            .iter()
            .rev()
            .find(|s| s.label == label)
            .map(|s| s.lease)
            .unwrap_or_else(|| panic!("no Start was sent for {label}:\n{}", self.text()))
    }

    /// The digest the `Start` of `label` under `lease` named, if one was sent.
    pub fn digest_of_start(&self, label: &str, lease: LeaseId) -> Option<ProtoDigest> {
        self.seen
            .iter()
            .rev()
            .find(|s| s.label == label && s.lease == lease)
            .map(|s| s.digest.clone())
    }

    /// The node the newest `Start` of `label` went to.
    pub fn holder_of(&self, label: &str) -> String {
        self.seen
            .iter()
            .rev()
            .find(|s| s.label == label)
            .map(|s| s.node.clone())
            .unwrap_or_else(|| panic!("no Start was sent for {label}:\n{}", self.text()))
    }

    /// How many `Start`s were sent for `label` so far.
    pub fn starts_of(&self, label: &str) -> usize {
        self.seen.iter().filter(|s| s.label == label).count()
    }

    /// Writes down what happened since the last call: the store's transactions, the
    /// action-cache entries that appeared, the messages sent to the nodes, and the
    /// stages that changed.
    pub async fn sync(&mut self) {
        let mut lines = self.tape.take();
        for (label, tag) in self.tags.clone() {
            if self.cached.contains(label) {
                continue;
            }
            let found = self.cache.action_result(&digest_of(tag)).await;
            if let Ok(Some((_, result))) = found {
                self.cached.insert(label);
                lines.push(format!("  cache {label} exit={}", result.exit_code));
            }
        }
        for (name, node) in &mut self.nodes {
            while let Ok(sent) = node.inbox.try_recv() {
                lines.push(format!(
                    "  msg {name} {}",
                    message_text(&sent, &self.labels)
                ));
                if let Ok(ServerMessage {
                    message: Some(server_message::Message::Start(start)),
                }) = &sent
                    && let Some(lease) = &start.lease_id
                    && let Some(digest) = &start.action_digest
                    && let Some(label) = self.labels.get(&digest.hash)
                {
                    self.seen.push(Seen {
                        node: name.clone(),
                        label,
                        lease: LeaseId::new(lease.term, lease.seq),
                        digest: digest.clone(),
                    });
                }
            }
        }
        for caller in &mut self.watched {
            let now = stage_text(&caller.stage.borrow());
            if now != caller.last {
                lines.push(format!("  stage {} {now}", caller.label));
                caller.last = now;
            }
        }
        self.out.extend(lines);
    }
}
