//! The local write tier of the simulated server (docs/design/write-path.md, section 6),
//! run as `kbf-front` runs it (`cache/tier.rs`, `cache/flush.rs`) over `kbf-tier`'s
//! [`Tier`] on the server's second disk:
//!
//! - **Start** ([`Server::attach_tier`]): the log is raised to machine version 2, the
//!   tier is the registered one or a new id (this start's writer epoch), the directory
//!   is opened against the committed staged table ([`Tier::open`]: the `TIER` check,
//!   each staged block cut to its committed length, the orphan sweep), `AddTier`
//!   commits for a new tier, and each lost block is marked `Missing`. What the open
//!   did is recorded, and the directory is checked against the committed state
//!   (invariant E).
//! - **Writes** ([`Server::stage`]): an upload's bytes are written to a range of the
//!   open block, synced, indexed with one `PutStaged`, and only then acknowledged. A
//!   range the tier does not take (its budget, a disk error) goes down the direct path.
//! - **The flusher** ([`Server::flush_round`], every tick): `SealStaged` for each block
//!   the tier names, then up to [`FLUSHES_PER_ROUND`] sealed objects, oldest first:
//!   each re-read and checked, PUT whole (records and the footer `kbf_segments` builds
//!   from the committed list) under its own id, `ObjectFlushed` committed once the
//!   store has it, and only then renamed to a flushed copy. A failed or late PUT, or a
//!   read error, is retried after [`FLUSH_BACKOFF_FIRST`] doubling to
//!   [`FLUSH_BACKOFF_MAX`]; a record that fails its check marks the object `Corrupt`.
//! - **Reads**: each blob a `FindMissingBlobs` answers present is read [`READ_AFTER`]
//!   later, at the location the index gave then, as a client's fetch would: from the
//!   tier's block or flushed copy, else from the store. Between the two the block may
//!   be flushed, renamed and evicted, which is the race of scenario 16.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::{FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSHES_PER_ROUND, MetaLog};
use std::num::NonZeroU64;

use kbf_log::{DecodeError, FarmLogId, Format, LogCommand, MachineVersion};
use kbf_meta::{
    Applied, BlobAnswer, Command, Location, ObjectId, Retention, ServerId, StagedPut, StagedState,
    TierId, UnreachableReason,
};
use kbf_raft::Payload;
use kbf_segments::IndexEntry;
use kbf_segments::layout::encode_footer;
use kbf_server::raft_log::RUNS;
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_sim::{NodeId, Output};
use kbf_store::{Fault, FaultFs, Fs};
use kbf_tier::{
    Recovery, StagedSnapshot, Tier, TierError, TierOptions, TierStart, WriteTier, block_name,
};
use kbf_types::{Digest, FarmTime};

use super::{PUT_TIMEOUT, Server, T_PUT, T_READ};
use crate::hook::{Down, TierPoint};
use crate::store::Body;
use crate::{Msg, content};

/// How long after a `FindMissingBlobs` answer the server reads each present blob.
pub const READ_AFTER: Duration = Duration::from_secs(2);

/// The one server's id in the tier registry.
const SERVER: ServerId = ServerId::new(1);

/// A server's local write tier: `--write-tier-bytes`, `--write-tier-seal-after`, and
/// the size at which the open block is sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierPlan {
    pub budget: u64,
    pub seal_after: Duration,
    pub segment_limit: u64,
}

impl TierPlan {
    fn options(self) -> TierOptions {
        TierOptions {
            budget: self.budget,
            seal_after: self.seal_after,
            segment_limit: self.segment_limit,
        }
    }
}

/// A crash at a [`TierPoint`]: when, how, the object it was writing or flushing, and
/// what the next start's open did (`None` until it ran).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierCut {
    pub at: FarmTime,
    pub point: TierPoint,
    pub down: Down,
    pub object: Option<ObjectId>,
    pub recovery: Option<Recovery>,
}

/// What the local write tier did, for the checks and tests to read.
#[derive(Clone, Debug, Default)]
pub struct TierSeen {
    /// Each start's tier and what its open found and did.
    pub recoveries: Vec<(FarmTime, TierId, Recovery)>,
    /// Each start's disagreement between the tier directory and the committed state
    /// (invariant E): a file nothing names, a block longer than committed, a budget
    /// that counts bytes nothing holds.
    pub leaks: Vec<String>,
    /// Staged objects whose block a start found missing or short.
    pub lost: Vec<ObjectId>,
    /// Uploads acknowledged through the tier, and the objects they were staged in.
    pub staged_acks: u64,
    pub staged: BTreeSet<ObjectId>,
    /// Uploads the tier did not take at its budget (backpressure), and after a disk
    /// error (`ENOSPC`); both went down the direct path.
    pub full: u64,
    pub disk_errors: u64,
    /// `PutStaged` commits refused by the state machine (never expected).
    pub refused: Vec<String>,
    /// `SealStaged` commits, objects flushed, failed flush attempts.
    pub sealed: u64,
    pub flushed: Vec<ObjectId>,
    pub flush_failures: u64,
    /// Objects a record of which failed the flusher's check: marked corrupt.
    pub corrupt: Vec<ObjectId>,
    /// Objects a [`TierFlip`](super::ServerFault::TierFlip) damaged, and flips that
    /// found nothing staged.
    pub damaged: BTreeSet<ObjectId>,
    pub flips_missed: u64,
    /// Reads answered with the right bytes; of them, those served by the store after
    /// the local copy the index answer implied (a staged block, or a copy flushed by
    /// this start) was gone; reads of damaged local bytes refused.
    pub reads: u64,
    pub fallbacks: u64,
    pub refused_damaged: u64,
    /// Reads of a blob the index answered present that found its bytes nowhere, or
    /// found other bytes (invariant A).
    pub unread: Vec<String>,
    /// Each crash at a tier point.
    pub cuts: Vec<TierCut>,
    /// The most bytes the tier counted at once, and copies it evicted.
    pub peak: u64,
    pub evicted: u64,
    /// Each step's budget overrun (invariant D): used and flushed bytes past it.
    pub over_budget: Vec<String>,
}

/// A read waiting for its time or the store's answer.
#[derive(Clone, Copy, Debug)]
pub(super) struct Read {
    digest: Digest,
    location: Location,
    /// Whether a local copy was there when the index answered: staged, or flushed by
    /// this start (a copy that may be evicted before the read).
    local: bool,
    /// Whether the local copy was gone when it was read.
    missed: bool,
}

/// The tier of one start.
pub(super) struct CellTier {
    tier: Arc<Tier<FaultFs>>,
    id: TierId,
    /// Failed flushes: failures in a row, and when the next attempt may start.
    retries: BTreeMap<ObjectId, (u32, FarmTime)>,
    /// Flush PUTs waiting for the store, by deadline tag.
    flushing: BTreeMap<u64, ObjectId>,
    /// Reads waiting, by tag.
    reads: BTreeMap<u64, Read>,
}

/// The wait before the next flush attempt after `failures` failures in a row, as the
/// flusher's (`kbf-front`, `flush_backoff`).
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FLUSH_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(FLUSH_BACKOFF_MAX)
}

/// The footer entries of `blobs` (sorted by digest) over `records`, or `None` if a
/// record is not all there or is not its blob's bytes.
fn entries(records: &[u8], blobs: &[(Digest, u64)]) -> Option<Vec<IndexEntry>> {
    let mut out = Vec::with_capacity(blobs.len());
    for (digest, offset) in blobs {
        let from = usize::try_from(*offset).ok()?;
        let to = from.checked_add(usize::try_from(digest.size_bytes).ok()?)?;
        let record = records.get(from..to)?;
        if record != content(digest) {
            return None;
        }
        out.push(IndexEntry {
            digest: *digest,
            offset: *offset,
            crc32c: kbf_segments::crc32c(record),
        });
    }
    Some(out)
}

impl Server {
    fn instant(&self, now: FarmTime) -> std::time::Instant {
        self.base + Duration::from_millis(now.as_millis())
    }

    /// The tier and its id, while the server is up with one.
    fn cell_tier(&self) -> Option<(Arc<Tier<FaultFs>>, TierId)> {
        let t = self.up.as_ref()?.tier.as_ref()?;
        Some((Arc::clone(&t.tier), t.id))
    }

    fn cell_tier_mut(&mut self) -> Option<&mut CellTier> {
        self.up.as_mut()?.tier.as_mut()
    }

    /// The tier's budget use, while it is up.
    #[must_use]
    pub fn tier_usage(&self) -> Option<kbf_tier::Usage> {
        Some(self.cell_tier()?.0.usage())
    }

    /// What a power cut now would leave of the tier's directory.
    #[must_use]
    pub fn tier_durable(&self) -> FaultFs {
        self.tier_disk.crash(0)
    }

    /// The tier reaches `point` with `object`: whether an armed crash fires there.
    fn reach_tier(&mut self, now: FarmTime, point: TierPoint, object: Option<ObjectId>) -> bool {
        let Some((point, down)) = self.tier_hooks.reached(point) else {
            return false;
        };
        self.cut(now, point, down, object);
        self.dying = Some((self.out.len(), down));
        true
    }

    fn cut(&mut self, now: FarmTime, point: TierPoint, down: Down, object: Option<ObjectId>) {
        self.seen.tier.cuts.push(TierCut {
            at: now,
            point,
            down,
            object,
            recovery: None,
        });
    }

    /// A crash armed part-way through the next commit of `kind` (an in-commit point)
    /// is planted in the raft disk at the commit's `after`-th operation.
    fn arm_tier_commit(&mut self, kind: fn(TierPoint) -> bool) -> Option<(TierPoint, Down)> {
        let (point, _, _) = self.tier_hooks.peek().find(|(p, _, _)| kind(*p))?;
        let (point, down) = self.tier_hooks.reached(point)?;
        let after = point.in_commit().unwrap_or_default();
        self.disk.fail_at(self.disk.ops() + after, Fault::Crash);
        self.armed = Some(down);
        Some((point, down))
    }

    /// After a commit with a planted crash: the crash is a cut of this commit if the
    /// commit failed with it.
    fn settle_tier_commit(
        &mut self,
        now: FarmTime,
        planted: Option<(TierPoint, Down)>,
        object: ObjectId,
    ) {
        let failed = self.up.as_ref().is_some_and(|up| up.failed_at.is_some());
        if let (Some((point, down)), true, Some(_)) = (planted, failed, self.armed) {
            self.cut(now, point, down, Some(object));
        }
    }

    /// Attaches the tier at a start, as `Cache::attach_tier` does: `Err(None)` if a
    /// commit failed (the log has stopped), `Err(Some(why))` if the open was refused.
    pub(super) fn attach_tier(&mut self, now: FarmTime) -> Result<(), Option<String>> {
        let Some(plan) = self.plan.tier else {
            return Ok(());
        };
        let up = self.up.as_mut().ok_or(None)?;
        up.log
            .raise_machine_version(MachineVersion::V2)
            .map_err(|_| None)?;
        let current = up.query(|m| m.tier(SERVER));
        let id = current.unwrap_or_else(|| TierId::new(up.writer.get()));
        let snapshot = up.query(StagedSnapshot::of);
        let start = TierStart {
            tier: id,
            epoch: up.writer,
            options: plan.options(),
        };
        let next = Arc::clone(&up.next_object);
        let seq: kbf_tier::NextSeq = Box::new(move || next.fetch_add(1, Ordering::Relaxed));
        let (tier, recovery) = Tier::open(self.tier_disk.clone(), start, &snapshot, seq)
            .map_err(|e| Some(format!("write tier: {e}")))?;
        if current.is_none() {
            let add = Command::AddTier {
                server: SERVER,
                tier: id,
            };
            match up.commit(&self.out, add) {
                Some(Applied::Tier(Ok(()))) => {}
                Some(other) => return Err(Some(format!("AddTier answered {other:?}"))),
                None => return Err(None),
            }
        }
        for object in &recovery.lost {
            let mark = Command::ObjectUnreachable {
                object: *object,
                reason: UnreachableReason::Missing,
            };
            up.commit(&self.out, mark).ok_or(None)?;
        }
        let names: Vec<(String, u64)> = self
            .tier_disk
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|n| {
                let len = self.tier_disk.file_len(&n).unwrap_or(u64::MAX);
                (n, len)
            })
            .collect();
        let staged = up.query(move |m| {
            let on = m.staged_objects().filter(|(_, s)| s.tier() == id);
            on.map(|(o, s)| (o, s.committed_len()))
                .collect::<BTreeMap<_, _>>()
        });
        let used = tier.usage().used;
        let leaks = crate::check::tier::directory_leaks(&names, &staged, used);
        self.seen.tier.leaks.extend(leaks);
        self.seen.tier.lost.extend(recovery.lost.iter().copied());
        for cut in self
            .seen
            .tier
            .cuts
            .iter_mut()
            .filter(|c| c.recovery.is_none())
        {
            cut.recovery = Some(recovery.clone());
        }
        self.seen.tier.recoveries.push((now, id, recovery));
        up.tier = Some(CellTier {
            tier: Arc::new(tier),
            id,
            retries: BTreeMap::new(),
            flushing: BTreeMap::new(),
            reads: BTreeMap::new(),
        });
        Ok(())
    }

    /// Records the tier's use after a change, and any budget overrun (invariant D).
    fn note_usage(&mut self, now: FarmTime) {
        let Some(usage) = self.tier_usage() else {
            return;
        };
        let held = usage.used.saturating_add(usage.flushed);
        self.seen.tier.peak = self.seen.tier.peak.max(usage.used);
        self.seen.tier.evicted = self.seen.tier.evicted.max(usage.evicted);
        if let Some(over) = crate::check::tier::over_budget(usage) {
            let at = now.as_millis();
            self.seen
                .tier
                .over_budget
                .push(format!("at {at} ms: {over} ({held})"));
        }
    }

    /// Stages an upload on the tier: whether the tier took it (acknowledged, answered
    /// `UNAVAILABLE` after a failed commit, or the server died on the way). `false`:
    /// no tier, or it did not take the range, and the direct path writes it.
    pub(super) fn stage(&mut self, now: FarmTime, from: &NodeId, digest: Digest) -> bool {
        let Some((tier, id)) = self.cell_tier() else {
            return false;
        };
        let bytes = content(&digest);
        let reserved = tier.reserve_group(bytes.len() as u64, self.instant(now));
        let reservation = match reserved {
            Ok(r) => r,
            Err(TierError::Full { .. }) => {
                self.seen.tier.full += 1;
                return false;
            }
            Err(_) => {
                self.seen.tier.disk_errors += 1;
                return false;
            }
        };
        self.note_usage(now);
        let object = reservation.object();
        if tier.write(&reservation, 0, &bytes).is_err() {
            self.seen.tier.disk_errors += 1;
            return false;
        }
        if self.reach_tier(now, TierPoint::AfterAppend, Some(object)) {
            return true;
        }
        let synced = match tier.sync(reservation) {
            Ok(synced) => synced,
            Err(_) => {
                self.seen.tier.disk_errors += 1;
                return false;
            }
        };
        if self.reach_tier(now, TierPoint::AfterSync, Some(object)) {
            return true;
        }
        let planted = self.arm_tier_commit(|p| matches!(p, TierPoint::InPutStaged { .. }));
        let put = Command::PutStaged {
            tier: id,
            objects: vec![StagedPut {
                object,
                end: synced.end(),
                seal: false,
                blobs: vec![(digest, synced.offset())],
            }],
        };
        let Some(up) = self.up.as_mut() else {
            return true;
        };
        let applied = up.commit(&self.out, put);
        self.settle_tier_commit(now, planted, object);
        let answer = match applied {
            Some(Applied::Staged(Ok(_))) => {
                tier.committed(synced);
                if self.reach_tier(now, TierPoint::BeforeReply, Some(object)) {
                    return true;
                }
                self.seen.acked.push((now, digest, object));
                self.seen.tier.staged_acks += 1;
                self.seen.tier.staged.insert(object);
                Msg::Uploaded { digest }
            }
            other => {
                if let Some(refused) = other {
                    self.seen.tier.refused.push(format!("{refused:?}"));
                }
                Msg::UploadFailed { digest }
            }
        };
        self.out.push(Output::Send {
            to: from.clone(),
            msg: answer,
        });
        self.reach_tier(now, TierPoint::Acked, Some(object));
        true
    }

    /// One round of the flusher at `now`.
    pub(super) fn flush_round(&mut self, now: FarmTime) {
        let Some((tier, id)) = self.cell_tier() else {
            return;
        };
        for object in tier.poll_seals(self.instant(now)) {
            let Some(up) = self.up.as_mut() else { return };
            match up.commit(&self.out, Command::SealStaged { object }) {
                Some(Applied::Tier(Ok(()))) => {
                    // Named by `poll_seals` just now, so the tier takes it.
                    let _ = tier.sealed(object);
                    self.seen.tier.sealed += 1;
                }
                _ => return,
            }
        }
        let Some(t) = self.cell_tier_mut() else {
            return;
        };
        let mut busy: BTreeSet<ObjectId> = t.flushing.values().copied().collect();
        let waiting = t.retries.iter().filter(|(_, (_, next))| *next > now);
        busy.extend(waiting.map(|(o, _)| *o));
        let Some(up) = self.up.as_ref() else { return };
        let due = up.query(move |m| {
            let ours = m.staged_objects().filter(|(o, s)| {
                (s.tier(), s.state()) == (id, StagedState::Sealed) && !busy.contains(o)
            });
            let due = ours.take(FLUSHES_PER_ROUND).map(|(o, s)| {
                let blobs: Vec<(Digest, u64)> = s.blobs().map(|(d, at)| (*d, at)).collect();
                (o, s.committed_len(), blobs)
            });
            due.collect::<Vec<_>>()
        });
        for (object, len, blobs) in due {
            self.flush_one(now, &tier, object, len, blobs);
            if self.up.as_ref().is_none_or(|up| up.stopped) {
                return;
            }
        }
    }

    /// Re-reads and checks `object`'s block and PUTs it whole.
    fn flush_one(
        &mut self,
        now: FarmTime,
        tier: &Tier<FaultFs>,
        object: ObjectId,
        len: u64,
        blobs: Vec<(Digest, u64)>,
    ) {
        let records = match tier.read(object, 0, usize::try_from(len).unwrap_or(usize::MAX)) {
            Ok(records) => records,
            Err(_) => return self.flush_failed(now, object),
        };
        let Some(entries) = entries(&records, &blobs) else {
            let mark = Command::ObjectUnreachable {
                object,
                reason: UnreachableReason::Corrupt,
            };
            let Some(up) = self.up.as_mut() else { return };
            if let Some(Applied::Marked(Ok(()))) = up.commit(&self.out, mark) {
                let _ = tier.discard(object);
                self.seen.tier.corrupt.push(object);
            }
            return;
        };
        let mut bytes = records;
        bytes.extend_from_slice(&encode_footer(len, &entries));
        let tag = T_PUT + self.puts_sent;
        self.puts_sent += 1;
        let Some(t) = self.cell_tier_mut() else {
            return;
        };
        t.flushing.insert(tag, object);
        let body = Body { blobs, bytes };
        self.out.push(Output::Send {
            to: self.store.clone(),
            msg: Msg::Flush { object, body },
        });
        self.timer(PUT_TIMEOUT, tag);
    }

    /// A flush attempt of `object` failed at `now`: the next waits for the backoff.
    fn flush_failed(&mut self, now: FarmTime, object: ObjectId) {
        self.seen.tier.flush_failures += 1;
        let Some(t) = self.cell_tier_mut() else {
            return;
        };
        let failures = t.retries.get(&object).map_or(0, |(n, _)| *n) + 1;
        let next = now.saturating_add(backoff(failures));
        t.retries.insert(object, (failures, next));
    }

    /// A PUT deadline that is a flush's: whether `tag` was one. The flush is failed;
    /// a later answer to it is ignored.
    pub(super) fn flush_deadline(&mut self, now: FarmTime, tag: u64) -> bool {
        let Some(object) = self.cell_tier_mut().and_then(|t| t.flushing.remove(&tag)) else {
            return false;
        };
        self.flush_failed(now, object);
        true
    }

    /// The tier's messages: the store's answers to flushes and reads. Any other
    /// message is handed back.
    pub(super) fn tier_path(&mut self, now: FarmTime, msg: Msg) -> Option<Msg> {
        match msg {
            Msg::FlushDone { object } => self.flush_done(now, object),
            Msg::FlushFailed { object } => {
                let t = self.cell_tier_mut()?;
                let tag = t
                    .flushing
                    .iter()
                    .find(|(_, o)| **o == object)
                    .map(|(t, _)| *t)?;
                t.flushing.remove(&tag);
                self.flush_failed(now, object);
            }
            Msg::Got { tag, held } => self.got(tag, held),
            other => return Some(other),
        }
        None
    }

    /// The store has the flushed `object`: `ObjectFlushed`, then the rename.
    fn flush_done(&mut self, now: FarmTime, object: ObjectId) {
        let Some((tier, _)) = self.cell_tier() else {
            return;
        };
        let Some(t) = self.cell_tier_mut() else {
            return;
        };
        let Some(tag) = t
            .flushing
            .iter()
            .find(|(_, o)| **o == object)
            .map(|(t, _)| *t)
        else {
            return; // answered after its deadline, or to an earlier start
        };
        t.flushing.remove(&tag);
        if self.reach_tier(now, TierPoint::AfterFlushPut, Some(object)) {
            return;
        }
        let planted = self.arm_tier_commit(|p| matches!(p, TierPoint::InFlushCommit { .. }));
        let Some(up) = self.up.as_mut() else { return };
        let applied = up.commit(&self.out, Command::ObjectFlushed { object });
        self.settle_tier_commit(now, planted, object);
        if !matches!(applied, Some(Applied::Tier(Ok(())))) {
            return;
        }
        if self.reach_tier(now, TierPoint::BeforeRename, Some(object)) {
            return;
        }
        // A failed rename leaves the `.blk`, which the next start deletes.
        let _ = tier.flushed(object);
        if let Some(t) = self.cell_tier_mut() {
            t.retries.remove(&object);
        }
        self.seen.tier.flushed.push(object);
        self.note_usage(now);
    }

    /// Starts a read of each of `asked` the index answers present now.
    pub(super) fn start_reads(&mut self, asked: &[Digest]) {
        let Some(up) = self.up.as_ref() else { return };
        if up.tier.is_none() || asked.is_empty() {
            return;
        }
        let asked = asked.to_vec();
        let present = up.query(move |m| {
            let answers = asked.into_iter().map(|d| (d, m.blob(&d)));
            let present = answers.filter_map(|(d, a)| match a {
                BlobAnswer::Present {
                    location,
                    staged_on,
                } => Some((d, location, staged_on)),
                BlobAnswer::Absent | BlobAnswer::Unavailable => None,
            });
            present.collect::<Vec<_>>()
        });
        for (digest, location, staged_on) in present {
            let tag = T_READ + self.reads_sent;
            self.reads_sent += 1;
            let flushed = self.seen.tier.flushed.contains(&location.object);
            let read = Read {
                digest,
                location,
                local: staged_on.is_some() || flushed,
                missed: false,
            };
            if let Some(t) = self.cell_tier_mut() {
                t.reads.insert(tag, read);
            }
            self.timer(READ_AFTER, tag);
        }
    }

    /// A read's time has come: from the tier, else from the store.
    pub(super) fn read_due(&mut self, now: FarmTime, tag: u64) {
        let Some((tier, _)) = self.cell_tier() else {
            return;
        };
        let Some(read) = self.cell_tier_mut().and_then(|t| t.reads.remove(&tag)) else {
            return; // a read of an earlier start
        };
        let Read {
            digest, location, ..
        } = read;
        let len = usize::try_from(digest.size_bytes).unwrap_or(usize::MAX);
        match tier.read(location.object, location.offset, len) {
            Ok(bytes) if bytes == content(&digest) => self.seen.tier.reads += 1,
            Ok(_) if self.seen.tier.damaged.contains(&location.object) => {
                self.seen.tier.refused_damaged += 1;
            }
            Ok(_) => {
                let at = now.as_millis();
                let unread = format!(
                    "{digest:?} read other bytes from {} at {at} ms",
                    location.object
                );
                self.seen.tier.unread.push(unread);
            }
            Err(_) => {
                let read = Read {
                    missed: true,
                    ..read
                };
                if let Some(t) = self.cell_tier_mut() {
                    t.reads.insert(tag, read);
                }
                let msg = Msg::Get {
                    object: location.object,
                    digest,
                    tag,
                };
                self.out.push(Output::Send {
                    to: self.store.clone(),
                    msg,
                });
            }
        }
    }

    /// The store's answer to a read the tier could not serve.
    fn got(&mut self, tag: u64, held: bool) {
        let Some(read) = self.cell_tier_mut().and_then(|t| t.reads.remove(&tag)) else {
            return;
        };
        let object = read.location.object;
        if held {
            self.seen.tier.reads += 1;
            self.seen.tier.fallbacks += u64::from(read.missed && read.local);
        } else if !self.seen.tier.damaged.contains(&object) {
            let digest = read.digest;
            let unread =
                format!("{digest:?} in {object} found neither on the tier nor in the store");
            self.seen.tier.unread.push(unread);
        }
    }

    /// A bad sector: flips the first byte of the first record of the oldest staged
    /// object on this tier whose block holds committed bytes.
    pub(super) fn flip_a_staged_record(&mut self) {
        let Some((_, id)) = self.cell_tier() else {
            return;
        };
        let Some(up) = self.up.as_ref() else { return };
        let first = up.query(move |m| {
            let mut on = m.staged_objects().filter(|(_, s)| s.tier() == id);
            on.find_map(|(o, s)| s.blobs().map(|(_, at)| at).min().map(|at| (o, at)))
        });
        let Some((object, at)) = first else {
            self.seen.tier.flips_missed += 1;
            return;
        };
        let name = block_name(object);
        let offset = usize::try_from(at).unwrap_or(usize::MAX);
        if self
            .tier_disk
            .contents(&name)
            .is_some_and(|b| b.len() > offset)
        {
            self.tier_disk.flip(&name, offset);
            self.seen.tier.damaged.insert(object);
        } else {
            self.seen.tier.flips_missed += 1;
        }
    }
}

/// A rollback of the binary at the end of a run (scenario 30): the durable log opened
/// by a binary that runs only machine version 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rollback {
    /// The index of the entry raising the log to machine version 2.
    pub raised_at: Option<u64>,
    /// The index of the first write tier command.
    pub first_tier_command: Option<u64>,
    /// The entry the version-1 open stopped at; `Err` if it opened, or failed
    /// another way.
    pub refused_at: Result<u64, String>,
    /// Whether this binary still opens the log after that, to the same staged table.
    pub reopened: bool,
}

impl Server {
    /// Opens what a power cut now would leave of the raft directory as a binary that
    /// runs only machine version 1 would, then as this one; `None` while down.
    #[must_use]
    pub fn rollback(&self) -> Option<Rollback> {
        let up = self.up.as_ref()?;
        let durable = self.disk.crash(0);
        let entries = crate::replica::durable_entries(durable.restart()).ok()?;
        let (mut raised_at, mut first_tier_command) = (None, None);
        for entry in &entries {
            let Payload::Command(bytes) = &entry.payload else {
                continue;
            };
            let index = entry.id.index.0;
            match kbf_log::decode(Format::V1, bytes) {
                Ok(LogCommand::MachineVersion(v)) if v == MachineVersion::V2 => {
                    raised_at.get_or_insert(index);
                }
                Err(DecodeError::UnknownCommand("MetaCommand")) => {
                    first_tier_command.get_or_insert(index);
                }
                _ => {}
            }
        }
        let _context = self.runtime.enter();
        let open = |runs| {
            super::ready(RaftMetaLog::open_running(
                durable.restart(),
                Retention::default(),
                "cell/",
                || Ok(FarmLogId::new(NonZeroU64::MIN)),
                runs,
            ))
        };
        let refused_at = match open(MachineVersion::V1) {
            Err(RaftLogError::Decode { index, .. }) => Ok(index),
            Err(e) => Err(e.to_string()),
            Ok(_) => Err("a binary running version 1 opened the log".to_owned()),
        };
        let staged = up.query(StagedSnapshot::of);
        let reopened = open(RUNS)
            .ok()
            .is_some_and(|log| super::ready(log.query(StagedSnapshot::of)) == Ok(staged));
        Some(Rollback {
            raised_at,
            first_tier_command,
            refused_at,
            reopened,
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_types::DigestFunction;

    use super::*;

    fn digest(n: u8, size: u64) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], size)
    }

    /// Catches a backoff that does not grow, grows without bound, or overflows.
    #[test]
    fn the_flush_backoff_doubles_to_its_cap() {
        assert_eq!(backoff(1), FLUSH_BACKOFF_FIRST);
        assert_eq!(backoff(2), FLUSH_BACKOFF_FIRST * 2);
        assert_eq!(backoff(u32::MAX), FLUSH_BACKOFF_MAX);
    }

    /// Catches a flusher check that passes a record whose bytes differ, or one cut
    /// short by the block's end, or that lists entries out of the committed order.
    #[test]
    fn a_record_must_be_whole_and_its_bytes() {
        let (a, b) = (digest(1, 3), digest(2, 2));
        let mut records = content(&a);
        records.extend(content(&b));
        let blobs = [(a, 0), (b, 3)];
        let listed = entries(&records, &blobs).expect("both whole");
        let order: Vec<(Digest, u64)> = listed.iter().map(|e| (e.digest, e.offset)).collect();
        assert_eq!(order, blobs);
        assert_eq!(listed[1].crc32c, kbf_segments::crc32c(&content(&b)));
        assert!(entries(&records[..4], &blobs).is_none(), "a short record");
        let mut damaged = records.clone();
        damaged[3] ^= 0xFF;
        assert!(entries(&damaged, &blobs).is_none(), "damaged bytes");
    }
}
