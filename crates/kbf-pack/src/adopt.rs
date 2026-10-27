//! Adopting the segments a farm's storage members already wrote into the pack index, in place.
//!
//! A segment a storage member flushed to the cold store has the layout of a pack: the blobs
//! back to back, then a footer that indexes them. The pack index can therefore point at it
//! where it lies. [`Adopter`] lists the members' keys, reads the footer of each segment
//! ([`kbf_objstore::footers`]; never the records) and records one pack per segment under the
//! segment's own object key, with the digests, offsets and lengths its footer lists. No byte
//! is copied and nothing is written to or deleted from the object store; the only writes are
//! the index's.
//!
//! **Keys.** A member's cold store holds three layouts of key under a prefix (`<prefix>` is
//! empty or ends in `/`; each number is 16 lowercase hex digits). The writer the index
//! records for a segment says which:
//!
//! - `<prefix>cas/<member>/<boot>/<seq>`, a storage node's: writer `legacy/<member>`;
//! - `<prefix>cas/<epoch>/<seq>`, a metadata log's epoch: writer `legacy/epoch-<epoch>`;
//! - `<prefix><log>/cas/<epoch>/<seq>`, the epoch of another log: writer
//!   `legacy/<log>/epoch-<epoch>`.
//!
//! A segment under any other key is counted ([`AdoptReport::foreign`]) and left alone, as is
//! an object that is not a segment (too short, a torn or foreign tail), one that was deleted
//! between the listing and the read, and one whose footer could not be read.
//!
//! **Dry run and apply.** [`AdoptMode::DryRun`] does everything but record: for each segment it
//! asks the index whether the pack is recorded already and, if not, which of its digests have
//! a row, and totals the rows and bytes that recording would add, at most (a digest in two
//! segments not yet recorded counts as new in both). [`AdoptMode::Apply`] records
//! each pack with [`PackIndex::record_pack`], which is idempotent on the key, so a run can be
//! repeated: a pack recorded before is counted ([`AdoptReport::packs_known`]) and changes
//! nothing, and a blob that has a row keeps it (the first location wins). A key recorded
//! before with other contents is a conflict: reported, and fatal only when
//! [`AdoptConfig::fail_on_conflict`] says so. A dry run finds the conflicts that show in the
//! pack's writer, size and number of blobs, and so reports no more than an apply would.
//!
//! **Verification.** The first [`AdoptConfig::verify_sample`] segments of a run each have one
//! blob read back (a ranged read) and hashed. A blob that does not hash to its footer's digest,
//! or cannot be read, is a problem; in apply mode its segment is then not recorded.
//!
//! **Checkpoints.** The report so far is the checkpoint: [`Adopter::run`] hands it to the
//! caller's `save` every [`AdoptConfig::checkpoint_every`] objects, after the records of those
//! objects are in the index, and once more when the run ends. A run given a checkpoint that
//! stopped part-way lists after its last key and carries its counters on; a checkpoint that
//! finished starts a run afresh, since an object may appear below keys already seen. A
//! checkpoint of another mode, prefix or source store is refused. An object that was not
//! adopted (its footer could not be read, a sampled blob did not match, its key is a
//! conflict) is passed by a checkpoint like any other and is not tried again by a run that
//! resumes from it.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt as _;
use kbf_objstore::footers::{FooterRead, FooterReads, SegmentFooter, SkipReason, read_footers};
use kbf_objstore::{
    ByteRange, KeyError, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError,
};
use kbf_pack_index::{BlobDigest, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError};
use kbf_segments::IndexEntry;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::blocking::offload;
use crate::digest::{from_segment, verifies};

/// The default of [`AdoptConfig::checkpoint_every`].
pub const CHECKPOINT_EVERY: NonZeroUsize = NonZeroUsize::new(256).expect("256 is not zero");

/// The default of [`AdoptConfig::max_problems`].
pub const MAX_PROBLEMS: usize = 100;

/// What a run does with the segments it reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdoptMode {
    /// Reads and counts, and records nothing.
    #[default]
    DryRun,
    /// Records each segment as a pack in the index.
    Apply,
}

/// Why a segment is listed in [`AdoptReport::problems`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProblemKind {
    /// The footer of the object could not be read (the store failed the read).
    Unreadable,
    /// The index holds the key as a pack with other contents.
    Conflict,
    /// A sampled blob did not hash to its footer's digest, or could not be read back.
    Mismatch,
}

/// One object a run could not adopt, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// The object's key.
    pub key: String,
    /// What went wrong.
    pub kind: ProblemKind,
}

/// What a run found and did. It is also the checkpoint: a report saved part-way resumes the
/// run (see the module docs). A field a saved report lacks reads as zero, so a report written
/// by an older build still resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdoptReport {
    /// Whether the run records.
    pub mode: AdoptMode,
    /// The prefix that was listed.
    pub prefix: String,
    /// The store that was listed, as [`AdoptConfig::source`] names it.
    pub source: String,
    /// The last key a checkpoint had handled, when this run resumed from one.
    pub resumed_after: Option<String>,
    /// The last key handled.
    pub last_key: Option<String>,
    /// The listing reached its end. `false`: the run stopped at its limit or failed, and a
    /// run given this report as its checkpoint goes on after [`AdoptReport::last_key`].
    pub finished: bool,
    /// Objects listed and read.
    pub objects: u64,
    /// Of those, segments (an object whose footer is a segment's).
    pub segments: u64,
    /// Objects deleted between the listing and the read of their footer.
    pub gone: u64,
    /// Objects shorter than a segment's trailer.
    pub short: u64,
    /// Objects whose tail is not a segment footer: foreign bytes, or a torn or damaged footer.
    pub not_a_segment: u64,
    /// Objects whose footer could not be read because the store failed the read. They are not
    /// adopted, and a checkpoint that passes them does not bring them back: run again from the
    /// start. The same holds for the segments counted in `conflicts` and `sample_bad`.
    pub read_failed: u64,
    /// Segments under a key that is in none of the layouts the members write. Not adopted.
    pub foreign: u64,
    /// Blobs listed by the footers of the segments that were adopted or would be.
    pub blobs: u64,
    /// The bytes of those blobs.
    pub bytes: u64,
    /// Packs the index did not hold: recorded (apply), or that recording would add (dry run).
    pub packs_new: u64,
    /// Packs the index held already, with these contents.
    pub packs_known: u64,
    /// Blobs that got a row: the rows apply created, or the rows a dry run says recording
    /// would add at most (each segment is looked up on its own, so a digest in two segments
    /// not yet recorded is new in both). A blob that has a row in another pack keeps it and is
    /// in [`AdoptReport::rows_existing`].
    pub rows_new: u64,
    /// Blobs of packs that were new whose digest had a row already.
    pub rows_existing: u64,
    /// The bytes of the blobs counted in `rows_new`. Worked out by a dry run only.
    pub bytes_new: u64,
    /// Packs the index holds under the same key with other contents. Not adopted.
    pub conflicts: u64,
    /// Blobs read back and hashed.
    pub sampled: u64,
    /// Of those, blobs that did not match their footer, or could not be read.
    pub sample_bad: u64,
    /// The first of the objects that were a problem, at most [`AdoptConfig::max_problems`]
    /// (the counters above are exact).
    pub problems: Vec<Problem>,
}

impl AdoptReport {
    fn new(config: &AdoptConfig) -> Self {
        Self {
            mode: config.mode,
            prefix: config.prefix.as_str().to_owned(),
            source: config.source.clone(),
            ..Self::default()
        }
    }

    /// Lists a problem unless `limit` are listed already.
    fn note(&mut self, limit: usize, key: &str, kind: ProblemKind) {
        if self.problems.len() < limit {
            self.problems.push(Problem {
                key: key.to_owned(),
                kind,
            });
        }
    }
}

/// Why a run ended before the end of the listing.
#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    /// The listing of the source store failed.
    #[error("listing the source store: {0}")]
    Listing(ObjectStoreError),
    /// The pack index failed.
    #[error(transparent)]
    Index(#[from] PackIndexError),
    /// The index holds the pack under `key` with other contents, and the run was told to stop
    /// at a conflict.
    #[error("pack {key} is recorded already with other contents")]
    Conflict {
        /// The pack's key.
        key: String,
    },
    /// The caller could not save the checkpoint.
    #[error("saving the checkpoint: {0}")]
    Checkpoint(io::Error),
    /// The checkpoint is of a run in another mode, over another prefix or of another store.
    #[error("the checkpoint is of a run with another mode, prefix or source store")]
    ResumeMismatch,
    /// The last key of the checkpoint is not an object key.
    #[error("the last key of the checkpoint: {0}")]
    ResumeKey(#[from] KeyError),
}

/// How [`Adopter::run`] reads, paces and records.
#[derive(Clone, Debug)]
pub struct AdoptConfig {
    /// Whether to record, or only to report what recording would do.
    pub mode: AdoptMode,
    /// The prefix of the members' keys: empty, or ending in `/`.
    pub prefix: KeyPrefix,
    /// Names the store that is listed (the caller's own spelling of its endpoint and bucket).
    /// A checkpoint of a run with another name is refused, so that the keys it passed are not
    /// taken for the keys of a different store.
    pub source: String,
    /// How the footers are read: the listing's page, the reads in flight, the tail.
    pub reads: FooterReads,
    /// How many objects are handled between two saves of the checkpoint.
    pub checkpoint_every: NonZeroUsize,
    /// Stop after this many segments. The run's report says where it stopped.
    pub limit_segments: Option<u64>,
    /// The most segments handled a second. `None`: no limit.
    pub segments_per_second: Option<NonZeroU32>,
    /// How many blobs, over the whole run, are read back and hashed, one from each of the
    /// first segments.
    pub verify_sample: u64,
    /// End the run, as a failure, at the first conflict.
    pub fail_on_conflict: bool,
    /// The most problems the report lists.
    pub max_problems: usize,
}

impl AdoptConfig {
    /// A configuration for `mode` over `prefix`: the footer reads of [`FooterReads::default`],
    /// a checkpoint every [`CHECKPOINT_EVERY`] objects, no limit, no pacing, no sample, a
    /// conflict reported, not fatal, and a source with no name.
    #[must_use]
    pub fn new(mode: AdoptMode, prefix: KeyPrefix) -> Self {
        Self {
            mode,
            prefix,
            source: String::new(),
            reads: FooterReads::default(),
            checkpoint_every: CHECKPOINT_EVERY,
            limit_segments: None,
            segments_per_second: None,
            verify_sample: 0,
            fail_on_conflict: false,
            max_problems: MAX_PROBLEMS,
        }
    }
}

/// What the caller does with a checkpoint: save it where a later run can find it.
pub type Saver<'a> = dyn FnMut(&AdoptReport) -> io::Result<()> + Send + 'a;

/// A run's report, and why it ended early, if it did.
#[derive(Debug)]
pub struct AdoptRun {
    /// What was found and done, up to the end or the failure.
    pub report: AdoptReport,
    /// Why the run ended before the end of the listing and before its limit.
    pub failure: Option<AdoptError>,
}

/// Adopts the segments of one store into one pack index (see the module docs).
pub struct Adopter<S: ObjectStore> {
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    config: AdoptConfig,
}

impl<S: ObjectStore> Adopter<S> {
    /// An adopter that reads segments from `store` and records packs in `index`.
    #[must_use]
    pub fn new(store: Arc<S>, index: Arc<dyn PackIndex>, config: AdoptConfig) -> Self {
        Self {
            store,
            index,
            config,
        }
    }

    /// Lists the store under the configured prefix and adopts what it finds, resuming from
    /// `resume` (see the module docs), and hands the checkpoint to `save` as the module docs
    /// say. The report comes back whatever happens.
    pub async fn run(&self, resume: Option<AdoptReport>, save: &mut Saver<'_>) -> AdoptRun {
        let mut report = AdoptReport::new(&self.config);
        let from = match resume_point(resume, &mut report) {
            Ok(from) => from,
            Err(error) => {
                return AdoptRun {
                    report,
                    failure: Some(error),
                };
            }
        };
        let failure = self.drive(from, &mut report, save).await.err();
        AdoptRun { report, failure }
    }

    /// Walks the listing, then saves the checkpoint a last time, finished or not.
    async fn drive(
        &self,
        from: Option<ListToken>,
        report: &mut AdoptReport,
        save: &mut Saver<'_>,
    ) -> Result<(), AdoptError> {
        let walked = self.walk(from, report, save).await;
        report.finished = matches!(walked, Ok(true));
        let saved = save(report).map_err(AdoptError::Checkpoint);
        walked.and(saved)
    }

    /// Handles each object in key order until the listing ends (`true`) or the limit is
    /// reached (`false`).
    async fn walk(
        &self,
        from: Option<ListToken>,
        report: &mut AdoptReport,
        save: &mut Saver<'_>,
    ) -> Result<bool, AdoptError> {
        let store: &S = &self.store;
        let footers = read_footers(store, &self.config.prefix, from, self.config.reads);
        let mut footers = pin!(footers);
        let mut pace = Pace::new(self.config.segments_per_second);
        let recorder = self.recorder();
        let (mut handled, mut unsaved) = (0_u64, 0_usize);
        while self.config.limit_segments != Some(handled) {
            let next = footers.try_next().await.map_err(AdoptError::Listing)?;
            let Some(read) = next else { return Ok(true) };
            let key = match read {
                FooterRead::Skipped(skipped) => {
                    recorder.skip(skipped.why, skipped.key.as_str(), report);
                    skipped.key
                }
                FooterRead::Segment(segment) => {
                    pace.wait().await;
                    self.adopt(&segment, report).await?;
                    report.segments += 1;
                    handled += 1;
                    segment.key
                }
            };
            // Counted with the key, once the object is handled. A failure leaves every counter
            // and the key as they were, so a run resumed from the checkpoint counts the object
            // (and its conflict, its sample, its rows) once.
            report.objects += 1;
            report.last_key = Some(key.to_string());
            unsaved += 1;
            if unsaved >= self.config.checkpoint_every.get() {
                save(report).map_err(AdoptError::Checkpoint)?;
                unsaved = 0;
            }
        }
        Ok(false)
    }

    /// The part of a run that needs the index and the configuration and not the store.
    fn recorder(&self) -> Recorder<'_> {
        Recorder {
            index: &self.index,
            config: &self.config,
        }
    }

    /// Adopts one segment, or counts why it is not.
    async fn adopt(
        &self,
        segment: &SegmentFooter,
        report: &mut AdoptReport,
    ) -> Result<(), AdoptError> {
        let recorder = self.recorder();
        let Some(pack) = recorder.pack_of(segment) else {
            report.foreign += 1;
            return Ok(());
        };
        let sample = self.sample(segment, report).await;
        if sample == Sample::Bad {
            recorder.mismatch(&pack.key, report);
            return Ok(());
        }
        let bytes = segment.footer.records_len();
        recorder.adopt(pack, bytes, report).await?;
        report.sampled += u64::from(sample == Sample::Good);
        Ok(())
    }

    /// Reads one blob of `segment` back and hashes it, while the run's sample is not complete.
    /// Counts nothing: the blob is counted with its segment, once the segment is handled.
    async fn sample(&self, segment: &SegmentFooter, report: &AdoptReport) -> Sample {
        if report.sampled >= self.config.verify_sample {
            return Sample::Unchecked;
        }
        let Some(entry) = pick(segment.footer.entries(), report.segments) else {
            return Sample::Unchecked;
        };
        let digest = from_segment(&entry.digest);
        let range = ByteRange::new(entry.offset, digest.size);
        let range = range.expect("a sampled blob has bytes");
        let read = self.store.get_range(&segment.key, range).await;
        let good = read.is_ok_and(|bytes| verifies(&digest, &bytes));
        if good { Sample::Good } else { Sample::Bad }
    }
}

/// What reading a blob back of a segment found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    /// No blob was read: the sample is complete, or the segment has no blob with bytes.
    Unchecked,
    /// The blob hashes to its footer's digest.
    Good,
    /// The blob does not, or could not be read.
    Bad,
}

/// What a run does with the index and the report, which does not depend on the store.
struct Recorder<'a> {
    index: &'a Arc<dyn PackIndex>,
    config: &'a AdoptConfig,
}

impl Recorder<'_> {
    /// Counts an object that was not read as a segment.
    fn skip(&self, why: SkipReason, key: &str, report: &mut AdoptReport) {
        match why {
            SkipReason::Gone => report.gone += 1,
            SkipReason::Short => report.short += 1,
            SkipReason::NotASegment(_) => report.not_a_segment += 1,
            SkipReason::Read(_) => {
                report.read_failed += 1;
                report.note(self.config.max_problems, key, ProblemKind::Unreadable);
            }
        }
    }

    /// The pack the index would hold for `segment`, or `None` when its key is in none of the
    /// layouts the members write.
    fn pack_of(&self, segment: &SegmentFooter) -> Option<NewPack> {
        let writer = writer_of(&self.config.prefix, &segment.key)?;
        Some(NewPack {
            key: segment.key.to_string(),
            writer,
            size: segment.size,
            blobs: segment.footer.entries().iter().map(to_blob).collect(),
        })
    }

    /// Counts a sampled blob that is not its footer's. Its segment is not adopted.
    fn mismatch(&self, key: &str, report: &mut AdoptReport) {
        report.sampled += 1;
        report.sample_bad += 1;
        report.note(self.config.max_problems, key, ProblemKind::Mismatch);
    }

    /// Records `pack`, or counts what recording it would do, as the mode says, then counts the
    /// blobs and the `bytes` of its records.
    async fn adopt(
        &self,
        pack: NewPack,
        bytes: u64,
        report: &mut AdoptReport,
    ) -> Result<(), AdoptError> {
        let blobs = pack.blobs.len() as u64;
        match self.config.mode {
            AdoptMode::Apply => self.record(pack, report).await?,
            AdoptMode::DryRun => self.estimate(&pack, report).await?,
        }
        report.blobs += blobs;
        report.bytes += bytes;
        Ok(())
    }

    /// Records `pack` and counts what the index did.
    async fn record(&self, pack: NewPack, report: &mut AdoptReport) -> Result<(), AdoptError> {
        let blobs = pack.blobs.len() as u64;
        let key = pack.key.clone();
        let index = Arc::clone(self.index);
        match offload(move || index.record_pack(&pack)).await {
            Ok(recorded) if recorded.created => {
                let indexed = u64::from(recorded.indexed);
                report.packs_new += 1;
                report.rows_new += indexed;
                report.rows_existing += blobs.saturating_sub(indexed);
            }
            Ok(_) => report.packs_known += 1,
            Err(PackIndexError::Conflict { .. }) => return self.conflict(&key, report),
            Err(other) => return Err(AdoptError::Index(other)),
        }
        Ok(())
    }

    /// Counts what recording `pack` would do, and changes nothing.
    async fn estimate(&self, pack: &NewPack, report: &mut AdoptReport) -> Result<(), AdoptError> {
        let index = Arc::clone(self.index);
        let key = pack.key.clone();
        if let Some(old) = offload(move || index.pack(&key)).await? {
            let kept = (old.writer.as_str(), old.size, u64::from(old.blob_count));
            let offered = (pack.writer.as_str(), pack.size, pack.blobs.len() as u64);
            if kept != offered {
                return self.conflict(&pack.key, report);
            }
            report.packs_known += 1;
            return Ok(());
        }
        let (mut existing, mut new, mut bytes) = (0_u64, 0_u64, 0_u64);
        for chunk in pack.blobs.chunks(MAX_BATCH) {
            let index = Arc::clone(self.index);
            let digests: Vec<BlobDigest> = chunk.iter().map(|blob| blob.digest).collect();
            let located = offload(move || index.locate(&digests)).await?;
            for (blob, place) in chunk.iter().zip(located) {
                if place.is_some() {
                    existing += 1;
                } else {
                    new += 1;
                    bytes += blob.digest.size;
                }
            }
        }
        // Counted once the pack is looked up whole, so that a failed lookup counts nothing.
        report.packs_new += 1;
        report.rows_existing += existing;
        report.rows_new += new;
        report.bytes_new += bytes;
        Ok(())
    }

    /// Counts a pack the index holds under `key` with other contents, or, when the run was
    /// told to stop at a conflict, ends it with the conflict uncounted: the segment is not
    /// handled, and a run resumed from the checkpoint meets it again.
    fn conflict(&self, key: &str, report: &mut AdoptReport) -> Result<(), AdoptError> {
        if self.config.fail_on_conflict {
            let key = key.to_owned();
            return Err(AdoptError::Conflict { key });
        }
        report.conflicts += 1;
        report.note(self.config.max_problems, key, ProblemKind::Conflict);
        Ok(())
    }
}

/// Where a run lists from, with `report` set to the counters it starts with: nothing carried
/// over for no checkpoint or for one that finished, the checkpoint's own for one that stopped
/// part-way.
fn resume_point(
    resume: Option<AdoptReport>,
    report: &mut AdoptReport,
) -> Result<Option<ListToken>, AdoptError> {
    let Some(before) = resume.filter(|before| !before.finished) else {
        return Ok(None);
    };
    let saved = (before.mode, &before.prefix, &before.source);
    if saved != (report.mode, &report.prefix, &report.source) {
        return Err(AdoptError::ResumeMismatch);
    }
    let from = match &before.last_key {
        Some(key) => Some(ListToken::after(&ObjectKey::new(key.as_str())?)),
        None => None,
    };
    *report = AdoptReport {
        resumed_after: before.last_key.clone(),
        ..before
    };
    Ok(from)
}

/// Spaces the segments of a run: each starts at least `every` after the one before.
struct Pace {
    every: Option<Duration>,
    next: Option<Instant>,
}

impl Pace {
    fn new(rate: Option<NonZeroU32>) -> Self {
        Self {
            every: rate.map(|rate| Duration::from_secs(1) / rate.get()),
            next: None,
        }
    }

    async fn wait(&mut self) {
        let Some(every) = self.every else { return };
        if let Some(next) = self.next {
            tokio::time::sleep_until(next).await;
        }
        self.next = Some(Instant::now() + every);
    }
}

/// The blob of a footer's entry, as the index records it.
fn to_blob(entry: &IndexEntry) -> PackBlob {
    PackBlob {
        digest: from_segment(&entry.digest),
        offset: entry.offset,
    }
}

/// The entry of `entries` to read back for the segment numbered `seed`: one that has bytes,
/// and another one for each segment.
fn pick(entries: &[IndexEntry], seed: u64) -> Option<&IndexEntry> {
    let sized: Vec<&IndexEntry> = entries
        .iter()
        .filter(|entry| entry.digest.size_bytes > 0)
        .collect();
    let at = seed as usize % sized.len().max(1);
    sized.get(at).copied()
}

/// The writer the index records for the segment `key`, from the layout the key has under
/// `prefix` (see the module docs), or `None` when it is in none of them.
fn writer_of(prefix: &KeyPrefix, key: &ObjectKey) -> Option<String> {
    let rest = key.as_str().strip_prefix(prefix.as_str())?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts[..] {
        ["cas", epoch, seq] if canonical(&[epoch, seq]) => Some(format!("legacy/epoch-{epoch}")),
        ["cas", member, boot, seq] if canonical(&[member, boot, seq]) => {
            Some(format!("legacy/{member}"))
        }
        [log, "cas", epoch, seq] if canonical(&[log, epoch, seq]) => {
            Some(format!("legacy/{log}/epoch-{epoch}"))
        }
        _ => None,
    }
}

/// Whether every part is 16 lowercase hex digits, as the members spell their numbers.
fn canonical(parts: &[&str]) -> bool {
    parts.iter().copied().all(is_hex16)
}

fn is_hex16(part: &str) -> bool {
    let parsed = u64::from_str_radix(part, 16);
    parsed.is_ok_and(|n| format!("{n:016x}") == part)
}
