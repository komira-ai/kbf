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
//! (seconds). A span or key may still go stale before it is used (a pack compacted, an object
//! gone): that is by design, and safe, as the task checks the SHA-256 of every blob it takes and
//! falls back to the listener for one that does not verify or is gone.
//!
//! A `large` key is readable by a task only once it holds a read credential for tierstore (a
//! per-job grant or a presigned GET); until then kbf-task treats it as `absent`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::join_all;
use kbf_types::Digest;
use serde_json::json;
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

/// What an entry of a table is charged to the read budget, until its reply is sent: more than
/// the longest line a pack entry makes.
const ENTRY_BYTES: u64 = 256;

/// The presence checks of large objects in flight at once.
const HEADS_IN_FLIGHT: usize = 16;

/// The digests whose presence is remembered, at most; at the bound the answers that have run
/// out go, and if none has, all of them.
const PRESENCE_BOUND: usize = 4096;

/// The kinds of entry, in the order they are counted.
const KINDS: [&str; 3] = ["pack", "large", "absent"];

/// What a locate keeps between requests: the presence of large objects for a few seconds,
/// and the counts.
pub(super) struct Locating {
    seen: Mutex<HashMap<Digest, (Instant, bool)>>,
    present_for: Duration,
    absent_for: Duration,
    requests: Tally,
    entries: [Counter; KINDS.len()],
}

impl Locating {
    pub(super) fn new(limits: &Limits) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            present_for: limits.locate_present_for,
            absent_for: limits.locate_absent_for,
            requests: Default::default(),
            entries: Default::default(),
        }
    }

    /// Whether tierstore holds the object of `digest`, if that was found out lately enough.
    fn recalled(&self, digest: &Digest) -> Option<bool> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (until, present) = *seen.get(digest)?;
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
        if seen.len() >= PRESENCE_BOUND {
            seen.retain(|_, (until, _)| now < *until);
            if seen.len() >= PRESENCE_BOUND {
                seen.clear();
            }
        }
        seen.insert(digest, (now + keep, present));
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
        let digests = scope.digests();
        // The table is held until it is sent: its size is charged first.
        let room = self.budget.take(digests.len() as u64 * ENTRY_BYTES).await;
        let Ok(entries) = self.entries_of(&digests).await else {
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
        let mut spans = match self.source.locate_packed(&packed).await {
            Ok(spans) if spans.len() == packed.len() => spans.into_iter(),
            _ => return Err(()),
        };
        let large: Vec<Digest> = digests
            .iter()
            .filter(|d| d.size_bytes > over)
            .copied()
            .collect();
        let mut held: Vec<Result<Option<u64>, ()>> = Vec::with_capacity(large.len());
        for group in large.chunks(HEADS_IN_FLIGHT) {
            held.extend(join_all(group.iter().map(|d| self.large_object(d))).await);
        }
        let mut held = large.iter().zip(held);
        digests
            .iter()
            .map(|digest| {
                if digest.size_bytes > over {
                    let (asked, length) = held.next().ok_or(())?;
                    debug_assert_eq!(asked, digest);
                    return Ok(match length? {
                        Some(size) if size == digest.size_bytes => Entry::Large(size),
                        _ => Entry::Absent,
                    });
                }
                if is_empty_blob(digest) {
                    return Ok(Entry::Absent);
                }
                Ok(spans.next().ok_or(())?.map_or(Entry::Absent, Entry::Pack))
            })
            .collect()
    }

    /// The length of the large object of `digest` if tierstore holds it, from what was found out
    /// lately or else a HEAD. A failed check is not remembered.
    async fn large_object(&self, digest: &Digest) -> Result<Option<u64>, ()> {
        match self.locate.recalled(digest) {
            Some(true) => return Ok(Some(digest.size_bytes)),
            Some(false) => return Ok(None),
            None => {}
        }
        let length = self.source.large_len(digest).await.map_err(|_| ())?;
        // What is remembered as present is an object of the digest's length only.
        self.locate
            .remember(*digest, length == Some(digest.size_bytes));
        Ok(length)
    }
}
