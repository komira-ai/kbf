//! One invocation's record (sections 3.1 and 3.2): its header fields, its counters and
//! its action rows. [`crate::Records`] builds it from events; this module holds the
//! values and the rules that touch one record only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, LeaseId, WorkerId};

use crate::event::{Call, InvocationId, Listing, Metadata, Outcome, Output, Owner, WallTime};

/// Rows one record keeps; past it, actions are counted but not rowed (section 3.3).
pub const ROW_CAP: usize = 1_000_000;
/// Outputs one row keeps; past it, `outputs_truncated` (section 3.2).
pub const OUTPUTS_PER_ROW: usize = 1_000;
/// Bytes kept of each `RequestMetadata` text field (section 2.2).
pub const TEXT_CAP: usize = 512;
/// Bytes kept of a reason or status message (section 3.2).
pub const REASON_CAP: usize = 1024;
/// How long an invocation with no operation in flight goes without an event before it
/// is idle (section 3.1).
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// A record's key: the invocation id, or `<id>~<n>` (n from 2) for the n-th caller
/// whose authentication metadata differs from the owner's (section 2.4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    id: InvocationId,
    collision: Option<usize>,
}

impl RecordKey {
    /// The owner's record of `id`.
    #[must_use]
    pub const fn owner(id: InvocationId) -> Self {
        Self {
            id,
            collision: None,
        }
    }

    /// The record of the `n`-th caller of `id` (n from 2).
    #[must_use]
    pub const fn collision(id: InvocationId, n: usize) -> Self {
        Self {
            id,
            collision: Some(n),
        }
    }

    /// The invocation id.
    #[must_use]
    pub const fn id(&self) -> &InvocationId {
        &self.id
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id.as_str())?;
        match self.collision {
            Some(n) => write!(f, "~{n}"),
            None => Ok(()),
        }
    }
}

/// A `RequestMetadata` text field, kept up to [`TEXT_CAP`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    /// The text, or its first [`TEXT_CAP`] bytes cut at a character boundary.
    pub value: String,
    /// Whether the client sent more.
    pub truncated: bool,
}

impl Text {
    /// `value` as the client sent it.
    #[must_use]
    pub fn sent(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            truncated: false,
        }
    }

    /// The field as a row keeps it: `None` when empty, else its first [`TEXT_CAP`]
    /// bytes, truncated if the client sent more (here or before it came in).
    fn kept(value: Option<&Self>) -> Option<Self> {
        let sent = value.filter(|v| !v.value.is_empty())?;
        let value = capped(&sent.value, TEXT_CAP);
        Some(Self {
            truncated: sent.truncated || value.len() < sent.value.len(),
            value,
        })
    }
}

/// The first `cap` bytes of `text`, cut back to a character boundary.
pub(crate) fn capped(text: &str, cap: usize) -> String {
    text[..text.floor_char_boundary(cap)].to_owned()
}

/// How the farm answered an action (section 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// GetActionResult found it in the action cache.
    CacheHit,
    /// Execute was answered from the action cache.
    CacheHitExecute,
    /// Execute attached to a twin's operation already in flight.
    Joined,
    /// Execute queued a new operation.
    Ran,
}

/// One lease of an action's operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The lease.
    pub lease: LeaseId,
    /// The worker it was granted to.
    pub worker: WorkerId,
    /// When the operation was queued for it: submitted, or requeued after the lease
    /// before it.
    pub queued: WallTime,
    /// When it was granted.
    pub granted: WallTime,
    /// When its worker was sent the Start.
    pub started: Option<WallTime>,
    /// When it ended.
    pub ended: Option<WallTime>,
    /// How it ended; `None` while it is held.
    pub end: Option<AttemptEnd>,
}

/// How a lease ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptEnd {
    /// Its result was accepted.
    Accepted,
    /// It was given up and the operation queued again.
    Requeued(RequeueReason),
    /// Its worker reported a failure that ended the operation, with the daemon's
    /// reason text (up to [`REASON_CAP`] bytes).
    Failed(Failure, String),
}

/// One action of an invocation (section 3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRow {
    /// The Action digest.
    pub action: Digest,
    /// The call's instance name.
    pub instance: String,
    /// `target_id`, when sent.
    pub target: Option<Text>,
    /// `action_mnemonic`, when sent.
    pub mnemonic: Option<Text>,
    /// `configuration_id`, when sent.
    pub configuration: Option<Text>,
    /// `action_id`, when sent.
    pub action_id: Option<Text>,
    /// How the farm answered it.
    pub how: How,
    /// The REAPI operation; `None` for a GetActionResult hit.
    pub operation: Option<String>,
    /// Every lease of its operation, in grant order; a joined action's are its twin's.
    pub attempts: Vec<Attempt>,
    /// What the caller was answered; `None` while the operation is in flight.
    pub outcome: Option<Outcome>,
    /// The result and its listing, its outputs cut to [`OUTPUTS_PER_ROW`].
    pub listing: Option<Listing>,
    /// Whether the result listed more outputs than the row keeps.
    pub outputs_truncated: bool,
    /// The Command digest, once the recorder has read it from the Action blob.
    pub command: Option<Digest>,
}

impl ActionRow {
    pub(crate) fn new(action: Digest, call: &Call, how: How, operation: Option<String>) -> Self {
        let m: &Metadata = &call.metadata;
        Self {
            action,
            instance: call.instance.clone(),
            target: Text::kept(m.target.as_ref()),
            mnemonic: Text::kept(m.mnemonic.as_ref()),
            configuration: Text::kept(m.configuration.as_ref()),
            action_id: Text::kept(m.action_id.as_ref()),
            how,
            operation,
            attempts: Vec::new(),
            outcome: None,
            listing: None,
            outputs_truncated: false,
            command: None,
        }
    }

    /// Copies the result's listing into the row, keeping the first
    /// [`OUTPUTS_PER_ROW`] outputs.
    pub(crate) fn set_listing(&mut self, mut listing: Listing) {
        self.outputs_truncated = listing.outputs.len() > OUTPUTS_PER_ROW;
        if self.outputs_truncated {
            // Truncating keeps the capacity: give the rest back.
            listing.outputs.truncate(OUTPUTS_PER_ROW);
            listing.outputs.shrink_to_fit();
        }
        self.listing = Some(listing);
    }

    /// What adding the row to a record costs: its slots in the row list and cost list,
    /// and its [`Self::heap_bytes`]. Exactly what [`Invocation::bytes`] grows by.
    pub(crate) fn cost(&self) -> usize {
        size_of::<Self>() + size_of::<usize>() + self.heap_bytes()
    }

    /// The memory the row holds beyond its own slot in the record's row list, as the
    /// bound of section 3.5 counts it: the bytes of its texts, its attempts and its
    /// outputs, and its index entry. Lengths, not capacities; the allocator's own
    /// overhead is not counted.
    pub(crate) fn heap_bytes(&self) -> usize {
        let text = |t: &Option<Text>| t.as_ref().map_or(0, |t| t.value.len());
        let attempts: usize = self
            .attempts
            .iter()
            .map(|a| {
                let reason = match &a.end {
                    Some(AttemptEnd::Failed(_, why)) => why.len(),
                    _ => 0,
                };
                size_of::<Attempt>() + a.worker.as_str().len() + reason
            })
            .sum();
        let outcome = match &self.outcome {
            Some(Outcome::Failed { message, .. }) => message.len(),
            Some(Outcome::Refused(why)) => why.len(),
            _ => 0,
        };
        let outputs: usize = self.listing.as_ref().map_or(0, |l| {
            l.outputs
                .iter()
                .map(|o| size_of::<Output>() + o.path.len())
                .sum()
        });
        self.instance.len()
            + text(&self.target)
            + text(&self.mnemonic)
            + text(&self.configuration)
            + text(&self.action_id)
            + self.operation.as_ref().map_or(0, String::len)
            + attempts
            + outcome
            + outputs
            + INDEX_ENTRY_BYTES
    }
}

/// What one row's entries in its record's action index and cost list take: the key,
/// the index, the cost, and a B-tree node's share (estimated).
const INDEX_ENTRY_BYTES: usize = size_of::<Digest>() + 3 * size_of::<usize>() + 16;

/// What one entry of a set of strings takes beside the string's bytes (estimated).
const SET_ENTRY_BYTES: usize = size_of::<String>() + 16;

/// The counters of section 3.1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Execute calls.
    pub execute: u64,
    /// Of those, answered from the action cache.
    pub cache_hit_execute: u64,
    /// Of those, joined to a running twin.
    pub joined: u64,
    /// Of those, queued to run.
    pub ran: u64,
    /// Execute answers that were not OK with exit code 0.
    pub failed: u64,
    /// GetActionResult calls answered with a result.
    pub ac_hit: u64,
    /// GetActionResult calls answered without one.
    pub ac_miss: u64,
    /// FindMissingBlobs digests asked about.
    pub blobs_asked: u64,
    /// FindMissingBlobs digests answered missing.
    pub blobs_missing: u64,
    /// CAS and ByteStream bytes the client sent.
    pub bytes_uploaded: u64,
    /// CAS and ByteStream bytes the client received.
    pub bytes_downloaded: u64,
}

/// Whether an invocation is still being written (section 3.1). There is no
/// "finished": REAPI has no end-of-build call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// An operation is in flight, or the last event is less than [`IDLE_AFTER`] old.
    Open,
    /// No operation in flight and no event for [`IDLE_AFTER`].
    Idle,
}

/// One invocation's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub(crate) key: RecordKey,
    pub(crate) minted: bool,
    pub(crate) owner: Owner,
    pub(crate) tool: Option<String>,
    pub(crate) tool_version: Option<String>,
    pub(crate) correlated_invocations_id: Option<String>,
    pub(crate) instances: BTreeSet<String>,
    pub(crate) first_seen: WallTime,
    pub(crate) last_seen: WallTime,
    pub(crate) complete: bool,
    pub(crate) rows_capped: bool,
    pub(crate) counts: Counts,
    pub(crate) collisions: Vec<RecordKey>,
    pub(crate) rows: Vec<ActionRow>,
    /// Each rowed action's index in `rows`.
    pub(crate) by_action: BTreeMap<Digest, usize>,
    /// Operations this invocation's Execute calls wait on.
    pub(crate) in_flight: usize,
    /// Each row's [`ActionRow::heap_bytes`], as last counted.
    pub(crate) row_bytes: Vec<usize>,
    /// The sum of `row_bytes`.
    pub(crate) rows_bytes: usize,
    /// What [`crate::Records`] counts for this record toward its bound.
    pub(crate) counted: usize,
    /// The `last_seen` [`crate::Records`] indexes this record under.
    pub(crate) indexed: WallTime,
    /// Whether the memory bound stopped this record taking rows (section 3.5, step 3).
    pub(crate) rows_closed: bool,
}

impl Invocation {
    pub(crate) fn new(key: RecordKey, call: &Call) -> Self {
        Self {
            key,
            minted: call.minted,
            owner: call.owner.clone(),
            tool: None,
            tool_version: None,
            correlated_invocations_id: None,
            instances: BTreeSet::new(),
            first_seen: call.at,
            last_seen: call.at,
            complete: true,
            rows_capped: false,
            counts: Counts::default(),
            collisions: Vec::new(),
            rows: Vec::new(),
            by_action: BTreeMap::new(),
            in_flight: 0,
            row_bytes: Vec::new(),
            rows_bytes: 0,
            counted: 0,
            indexed: call.at,
            rows_closed: false,
        }
    }

    /// Takes what any call says about the invocation as a whole.
    pub(crate) fn seen(&mut self, call: &Call) {
        let m = &call.metadata;
        if self.tool.is_none() && m.tool.is_some() {
            self.tool.clone_from(&m.tool);
            self.tool_version.clone_from(&m.tool_version);
        }
        if self.correlated_invocations_id.is_none() {
            self.correlated_invocations_id = m
                .correlated_invocations_id
                .clone()
                .filter(|c| !c.is_empty());
        }
        self.instances.insert(call.instance.clone());
        self.touch(call.at);
    }

    pub(crate) fn touch(&mut self, at: WallTime) {
        self.last_seen = self.last_seen.max(at);
    }

    /// The index of `action`'s row, if it has one.
    pub(crate) fn row_of(&self, action: &Digest) -> Option<usize> {
        self.by_action.get(action).copied()
    }

    /// Whether the record takes another row: it holds fewer than `cap` and the memory
    /// bound has not closed it.
    pub(crate) fn takes_rows(&self, cap: usize) -> bool {
        self.rows.len() < cap && !self.rows_closed
    }

    /// Adds `row` and returns its index; `None` once the record holds `cap` rows or
    /// the memory bound has closed it.
    pub(crate) fn push_row(&mut self, cap: usize, row: ActionRow) -> Option<usize> {
        if !self.takes_rows(cap) {
            self.rows_capped = true;
            return None;
        }
        let bytes = row.heap_bytes();
        self.by_action.insert(row.action, self.rows.len());
        self.rows.push(row);
        self.row_bytes.push(bytes);
        self.rows_bytes += bytes;
        Some(self.rows.len() - 1)
    }

    /// Counts row `i` again after it changed.
    pub(crate) fn refresh_row(&mut self, i: usize) {
        let bytes = self.rows[i].heap_bytes();
        self.rows_bytes = self.rows_bytes - self.row_bytes[i] + bytes;
        self.row_bytes[i] = bytes;
    }

    /// The memory the record holds, as the bound of section 3.5 counts it: the record
    /// itself, its header's texts, a slot in its row list for each row and each row's
    /// heap bytes (texts, attempts, outputs and index entry). An estimate: lengths, not
    /// capacities, and not the allocator's overhead.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let option = |o: &Option<String>| o.as_ref().map_or(0, String::len);
        let instances: usize = self
            .instances
            .iter()
            .map(|i| i.len() + SET_ENTRY_BYTES)
            .sum();
        let collisions: usize = self
            .collisions
            .iter()
            .map(|k| size_of::<RecordKey>() + k.id().as_str().len())
            .sum();
        size_of::<Self>()
            + self.key.id().as_str().len()
            + self.owner.0.len()
            + option(&self.tool)
            + option(&self.tool_version)
            + option(&self.correlated_invocations_id)
            + instances
            + collisions
            + self.rows.len() * (size_of::<ActionRow>() + size_of::<usize>())
            + self.rows_bytes
    }

    /// The record's key.
    #[must_use]
    pub const fn key(&self) -> &RecordKey {
        &self.key
    }

    /// Whether the server minted the id (section 2.3).
    #[must_use]
    pub const fn minted(&self) -> bool {
        self.minted
    }

    /// The `public` authentication metadata of its first caller.
    #[must_use]
    pub const fn owner(&self) -> &Owner {
        &self.owner
    }

    /// `tool_details` of the first call that sent them: name and version.
    #[must_use]
    pub fn tool(&self) -> (Option<&str>, Option<&str>) {
        (self.tool.as_deref(), self.tool_version.as_deref())
    }

    /// The first non-empty `correlated_invocations_id` seen.
    #[must_use]
    pub fn correlated_invocations_id(&self) -> Option<&str> {
        self.correlated_invocations_id.as_deref()
    }

    /// The instance names its calls used.
    #[must_use]
    pub const fn instances(&self) -> &BTreeSet<String> {
        &self.instances
    }

    /// The first event's time.
    #[must_use]
    pub const fn first_seen(&self) -> WallTime {
        self.first_seen
    }

    /// The last event's time: a call, a grant, a start, a requeue or a finish.
    #[must_use]
    pub const fn last_seen(&self) -> WallTime {
        self.last_seen
    }

    /// Whether the record holds every event of the invocation.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    /// Marks the record as missing events: one was dropped, or a server restarted
    /// while it was open (section 3.4).
    pub const fn mark_incomplete(&mut self) {
        self.complete = false;
    }

    /// Whether the record stopped taking rows.
    #[must_use]
    pub const fn rows_capped(&self) -> bool {
        self.rows_capped
    }

    /// Its counters.
    #[must_use]
    pub const fn counts(&self) -> &Counts {
        &self.counts
    }

    /// The keys of other callers' records of the same id (the owner's record only).
    #[must_use]
    pub fn collisions(&self) -> &[RecordKey] {
        &self.collisions
    }

    /// Its action rows, in first-seen order.
    #[must_use]
    pub fn rows(&self) -> &[ActionRow] {
        &self.rows
    }

    /// Its state at `now`.
    #[must_use]
    pub fn state(&self, now: WallTime) -> State {
        let quiet = now
            .unix_micros()
            .saturating_sub(self.last_seen.unix_micros());
        if self.in_flight == 0 && u128::from(quiet) >= IDLE_AFTER.as_micros() {
            State::Idle
        } else {
            State::Open
        }
    }
}
