//! `GET /locate`: where the blobs of a grant's Action are, so that a task fetches them from
//! the stores directly and asks the listener only for what it cannot find.
//!
//! The answer covers exactly the grant's scope (the Action, its Command and the input closure
//! the memoised scope walk computed) and nothing else, and carries no digest that is not in it.
//! The body is JSON lines, one object per digest of the scope, in digest order:
//!
//! - `{"digest":"<hex>/<size>","pack":{"key":"<pack key>","offset":<n>,"length":<n>}}`: the blob
//!   is the `length` bytes at `offset` of that pack, as the pack index has it;
//! - `{"digest":"<hex>/<size>","large":{"key":"cas/l/<hex>","size":<n>}}`: a blob over
//!   [`super::Limits::stream_over`] that tierstore holds whole under its own key, `size` being the
//!   length of that object (always the digest's size: an object of another length is `absent`);
//! - `{"digest":"<hex>/<size>","absent":true}`: neither; the task asks the listener's GET or
//!   batch for it. The empty blob is always `absent`: the listener answers it without a read.
//!
//! The table is built for each request from the memoised closure and a fresh pack index lookup,
//! so it is as current as the index; the presence of a large object is remembered for
//! [`super::Limits::locate_present_for`] and its absence for [`super::Limits::locate_absent_for`]
//! (seconds), in a memory of at most [`super::Limits::locate_remembered`] digests that drops its
//! oldest answer first. A span or key may still go stale before it is used (a pack compacted, an
//! object gone): that is by design.
//!
//! **Bounds.** The digests of a scope are client-declared, so the work a request makes is
//! bounded without trusting them: at most [`super::Limits::locate_heads_per_request`] large
//! digests are checked in tierstore (the rest are `absent`, which is always safe), the checks of
//! all requests together run at most [`HEADS_IN_FLIGHT`] at a time, and one that is not answered
//! within [`super::Limits::locate_head_timeout`] is `absent` for that request and not
//! remembered. The table is charged to the read budget before any of it is built, at
//! [`MAX_LOCATE_LINE_BYTES`] for each digest (the longest line there can be), and a request takes
//! one of the reads in flight while it asks the stores. A span whose length is not the digest's
//! size, or whose pack key is longer than an object key can be, is `absent`.
//!
//! [`super::Limits::stream_over`] must equal the pack store's `max_blob_bytes`: a blob between
//! them is asked of the index, which does not hold it, and is `absent`.
//!
//! **Contract for the consumer (not implemented yet: the kbf-task client is the next step).**
//! kbf-task is to check the SHA-256 of every blob it takes through a table and to fall back to
//! the listener's GET or batch for one that does not verify or is gone, which is what makes a
//! stale entry safe. A `large` key is readable only by a holder of a tierstore read credential (a
//! per-job grant or a presigned GET); until a task has one it is to treat `large` as `absent`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::join_all;
use kbf_types::Digest;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::walk::ScopeError;
use super::{Hold, JobRead, LOCATE_OUTCOMES, Limits, Outcome, Reply, Tally};
use crate::cache::is_empty_blob;
use crate::metrics::{Counter, Families};

/// The path of the locate read, which takes a GET.
pub const LOCATE_PATH: &str = "/locate";

/// Where a blob lies in a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The key of the pack.
    pub key: String,
    /// Where the blob's bytes start in the pack.
    pub offset: u64,
    /// The blob's length in bytes.
    pub length: u64,
}

/// The key a blob over the record cap has in tierstore: `cas/l/<lowercase hex of its hash>`.
#[must_use]
pub fn large_key(digest: &Digest) -> String {
    format!("cas/l/{}", digest.hash_hex())
}

/// The longest line a table has, in bytes: a pack entry with a pack key of the most an object
/// key can be (1024 bytes), digests and offsets at their widest. Each digest of a scope is
/// charged to the read budget at this, before the table is built.
pub const MAX_LOCATE_LINE_BYTES: u64 = 1208;

/// The longest pack key a line carries: the most an object key can be.
const MAX_PACK_KEY_BYTES: usize = 1024;

/// The presence checks in tierstore in flight at once, for all requests together.
pub const HEADS_IN_FLIGHT: usize = 16;

/// The kinds of entry, in the order they are counted.
const KINDS: [&str; 3] = ["pack", "large", "absent"];

/// The presence answers remembered, the oldest dropped first at the bound.
struct Memory {
    answers: HashMap<Digest, (Instant, bool, u64)>,
    /// The digests by the order they were remembered in.
    order: BTreeMap<u64, Digest>,
    next: u64,
    bound: usize,
}

impl Memory {
    fn put(&mut self, digest: Digest, until: Instant, present: bool) {
        if let Some((_, _, old)) = self.answers.remove(&digest) {
            self.order.remove(&old);
        }
        if self.answers.len() >= self.bound.max(1) {
            // The oldest, if there is one: with room for none the memory holds nothing.
            self.order.pop_first().into_iter().for_each(|(_, oldest)| {
                self.answers.remove(&oldest);
            });
        }
        self.answers.insert(digest, (until, present, self.next));
        self.order.insert(self.next, digest);
        self.next += 1;
    }
}

/// What a locate keeps between requests: the presence of large objects for a few seconds,
/// the places for presence checks, and the counts.
pub(super) struct Locating {
    seen: Mutex<Memory>,
    present_for: Duration,
    absent_for: Duration,
    heads: Semaphore,
    requests: Tally,
    entries: [Counter; KINDS.len()],
}

impl Locating {
    pub(super) fn new(limits: &Limits) -> Self {
        Self {
            seen: Mutex::new(Memory {
                answers: HashMap::new(),
                order: BTreeMap::new(),
                next: 0,
                bound: limits.locate_remembered,
            }),
            present_for: limits.locate_present_for,
            absent_for: limits.locate_absent_for,
            heads: Semaphore::new(HEADS_IN_FLIGHT),
            requests: Default::default(),
            entries: Default::default(),
        }
    }

    /// Whether tierstore holds the object of `digest`, if that was found out lately enough.
    fn recalled(&self, digest: &Digest) -> Option<bool> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (until, present, _) = *seen.answers.get(digest)?;
        (Instant::now() < until).then_some(present)
    }

    fn remember(&self, digest: Digest, present: bool) {
        let now = Instant::now();
        let keep = if present {
            self.present_for
        } else {
            self.absent_for
        };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.put(digest, now + keep, present);
    }

    pub(super) fn render_metrics(&self, families: &mut Families) {
        let name = "kbf_job_locate_requests_total";
        families.head(
            name,
            "counter",
            "Job-facing locate requests answered, by outcome",
        );
        for outcome in LOCATE_OUTCOMES {
            let count = self.requests[outcome as usize].get();
            let label = outcome.label();
            let _ = writeln!(families.0, "{name}{{outcome=\"{label}\"}} {count}");
        }
        let name = "kbf_job_locate_entries_total";
        families.head(
            name,
            "counter",
            "Entries of job-facing locate answers, by kind: pack, large or absent",
        );
        for (kind, counter) in KINDS.iter().zip(&self.entries) {
            let _ = writeln!(families.0, "{name}{{kind=\"{kind}\"}} {}", counter.get());
        }
    }
}

/// How one digest of the scope is answered.
enum Entry {
    Pack(Span),
    Large(u64),
    Absent,
}

impl JobRead {
    /// The answer to a `GET /locate`: see the module's account of the table.
    pub(super) async fn locate(&self, authorization: Option<&str>) -> Reply {
        let claims = match self.claims(authorization) {
            Ok(claims) => claims,
            Err(reason) => return self.refuse_locate(Outcome::Unauthorized, reason),
        };
        let scope = match self.scope(claims.action).await {
            Ok(scope) => scope,
            Err(ScopeError::Unusable) => return self.refuse_locate(Outcome::Forbidden, "no_scope"),
            Err(ScopeError::Store) => {
                return self.refuse_locate(Outcome::Unavailable, "store_unavailable");
            }
        };
        // The table is held until it is sent: the most it can be is charged before any of it is
        // built (a place first, as a GET takes them, so that the two never wait on each other).
        let place = Arc::clone(&self.reads).acquire_owned().await;
        let room = self
            .budget
            .take(scope.most_digests() as u64 * MAX_LOCATE_LINE_BYTES)
            .await;
        let digests = scope.digests();
        let entries = self.entries_of(&digests).await;
        drop(place);
        let Ok(entries) = entries else {
            return self.refuse_locate(Outcome::Unavailable, "store_unavailable");
        };
        let mut body = String::new();
        for (digest, entry) in digests.iter().zip(&entries) {
            let (kind, line) = match entry {
                Entry::Pack(at) => (
                    0,
                    json!({"digest": digest.to_string(), "pack":
                        {"key": at.key, "offset": at.offset, "length": at.length}}),
                ),
                Entry::Large(size) => (
                    1,
                    json!({"digest": digest.to_string(), "large":
                        {"key": large_key(digest), "size": size}}),
                ),
                Entry::Absent => (2, json!({"digest": digest.to_string(), "absent": true})),
            };
            self.locate.entries[kind].add(1);
            body.push_str(&line.to_string());
            body.push('\n');
        }
        let mut reply = self.tally(&self.locate.requests, Outcome::Ok, Bytes::from(body));
        reply.hold = Some(Hold {
            _permits: vec![room],
            idle: self.limits.held_idle,
        });
        reply
    }

    fn refuse_locate(&self, outcome: Outcome, reason: &'static str) -> Reply {
        self.tally(
            &self.locate.requests,
            outcome,
            Bytes::from(format!("{reason}\n")),
        )
    }

    /// The entry of each of `digests`, in order: one batched ask of the pack index for the blobs
    /// that fit a pack, and a presence check (remembered for a few seconds) for each over
    /// [`Limits::stream_over`]. `Err` when the index or the object store could not answer.
    async fn entries_of(&self, digests: &[Digest]) -> Result<Vec<Entry>, ()> {
        let over = self.limits.stream_over;
        let packed: Vec<Digest> = digests
            .iter()
            .filter(|d| d.size_bytes <= over && !is_empty_blob(d))
            .copied()
            .collect();
        let spans = self.source.locate_packed(&packed).await.map_err(|_| ())?;
        let large: Vec<Digest> = digests
            .iter()
            .filter(|d| d.size_bytes > over)
            .take(self.limits.locate_heads_per_request)
            .copied()
            .collect();
        let held = join_all(large.iter().map(|d| self.large_object(d))).await;
        // What the source did not answer for is absent: the task asks the listener for it.
        let mut entries: HashMap<Digest, Entry> = HashMap::new();
        for (digest, at) in packed.iter().zip(spans) {
            let entry = match at {
                Some(at)
                    if at.length == digest.size_bytes && at.key.len() <= MAX_PACK_KEY_BYTES =>
                {
                    Entry::Pack(at)
                }
                _ => Entry::Absent,
            };
            entries.insert(*digest, entry);
        }
        for (digest, length) in large.iter().zip(held) {
            let entry = match length? {
                Some(size) if size == digest.size_bytes => Entry::Large(size),
                _ => Entry::Absent,
            };
            entries.insert(*digest, entry);
        }
        Ok(digests
            .iter()
            .map(|digest| entries.remove(digest).unwrap_or(Entry::Absent))
            .collect())
    }

    /// The length of the large object of `digest` if tierstore holds it, from what was found out
    /// lately or else a HEAD. A failed check is not remembered.
    async fn large_object(&self, digest: &Digest) -> Result<Option<u64>, ()> {
        match self.locate.recalled(digest) {
            Some(true) => return Ok(Some(digest.size_bytes)),
            Some(false) => return Ok(None),
            None => {}
        }
        // A place among the checks of all requests, and a deadline for the check itself.
        let _place = self.locate.heads.acquire().await;
        let probe = tokio::time::timeout(
            self.limits.locate_head_timeout,
            self.source.large_len(digest),
        )
        .await;
        // Not answered in time: not known, so absent for this request and not remembered.
        let Ok(answer) = probe else { return Ok(None) };
        let length = answer.map_err(|_| ())?;
        // What is remembered as present is an object of the digest's length only.
        self.locate
            .remember(*digest, length == Some(digest.size_bytes));
        Ok(length)
    }
}
