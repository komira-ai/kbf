//! The executor's seams, over what the frontend owns: [`Front`] implements [`Ingest`] and
//! [`Inputs`].
//!
//! - **A put directory** ([`FrontParts::put_dir`]): a run commits its objects into a directory
//!   the frontend mints for it. The preparation ([`RunPrepare`]) mints the run's put directory
//!   and collects its closure ([`RunPrepare::prepare_put_dir`]), and [`Ingest::mint`] names the
//!   action tree over the published base tree ([`TreeWriter::write_named`]) and parks its name,
//!   which [`Inputs::tree`] hands out once. The tree's writes go on, on a task of their own, after
//!   the mint answers: [`Ingest::stored`] waits for them, once for a run, and answers their
//!   failure as the mint would have. The run it is given is the executor run's build id
//!   ([`build_id`]), the same string the task tool is told with `--lease`. The ingest is
//!   [`kbf_front::ingest_put_dir_with`] of the directory, over the frontend's [`Packs`] and a
//!   [`Backend`] built over the same packs (the Action it reads to check the outputs may be a
//!   blob the frontend keeps in a pack). A run that cannot be prepared is the caller's
//!   ([`MintError::Refused`]) unless the store could not be read ([`MintError::Unavailable`]):
//!   see [`PrepareFault`].
//! - **The action's facts** are read from the Action and the Command in the CAS: the
//!   `container-image` platform property, which names the BASE environment the Command sees and
//!   must be, by digest, the base the executor serves (an action that names none uses it); and
//!   the timeout. The image a run is launched from is the kbf image, built from that base and
//!   set by the operator: it keeps its `ENTRYPOINT` (the task tool), so no action's image is
//!   launched and no entrypoint is looked for. The platform's system is read too (`OSFamily`
//!   or `os`, and a Mac's `os_version`), as `kbf-front` reads it: a Mac action becomes a Mac job
//!   ([`crate::executor::mac`]).
//! - **Two caches, by digest**, so that a warm prepare does no store call (`front_cache.rs`).
//!   Each is bounded by its entries, the oldest going first. Both live and die with the `Front`
//!   and the base and kbf images it is pinned to. The action trees remembered are those of an
//!   action and a form ([`InputTree`]): the tree's name, good for
//!   [`kbf_front::action_tree::MEMO_TTL`] on the frontend's clock, since a tree names the pack
//!   spans and whole objects its files are held in, and the writer's own memo keeps a place for
//!   no longer than that. A tree is remembered once every write of it succeeded, never when it
//!   is only named. A hit skips the
//!   closure, the lookup of the places and every write; only the run's put directory is minted
//!   ([`RunPrepare::mint_put_dir`]). The facts remembered are those of an action's digest, which
//!   never expire: the Action, the Command and the platform they name are named by their
//!   digests, so what they say cannot change. Only a read that succeeded is remembered.
//! - **The steps of a mint are timed** into the executor's `kbf_executor_step_seconds`
//!   ([`Front::with_metrics`]): the put directory, the closure's parts and the tree's name.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::action_tree::{BaseTree, MEMO_TTL, PutRetry, TreeWriter};
use kbf_front::prepare_run::{
    KeptClosure, PrepareError as RunPrepError, PrepareFault, PrepareTook, PutDirPrep, PutDirRun,
};
use kbf_front::{Backend, Described, IssuedPutDir, PackKeyMint, Packs};
use kbf_proto::reapi::{self, Action, Command, Digest};
use kbf_types::{DigestFunction, ImageRef};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::action_usage::Shape;
use crate::executor::config::InputTree;
use crate::executor::front_cache::Bounded;
use crate::executor::mac::System;
use crate::executor::metrics::Metrics;
use crate::executor::size_class::SizeClass;
use crate::executor::steps::Step;
use crate::executor::task::{PackKey, RunId, build_id};
use crate::executor::traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, RejectReason, UntrustedLimits,
};

/// The most action trees [`Front`] remembers.
const TREE_ENTRIES: usize = 100_000;

/// The most action facts [`Front`] remembers.
const FACTS_ENTRIES: usize = 100_000;

/// Why a run could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// The inputs are not all held, are over budget, or the wall is over the key ceiling.
    #[error("{0}")]
    Refused(String),
    /// The store could not answer.
    #[error("{0}")]
    Unavailable(String),
}

/// The frontend's preparation of a run: its put directory, and the closure of its inputs.
#[tonic::async_trait]
pub trait RunPrepare: Send + Sync {
    /// The run of `action` on `run`, started at `now`, which may take `wall`, that commits
    /// into a put directory: the directory and the closure its action tree is written from.
    ///
    /// # Errors
    /// [`PrepareError`].
    async fn prepare_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError>;

    /// The put directory of the run alone, for a run whose action tree is cached: no closure is
    /// collected. The same checks and errors as [`RunPrepare::prepare_put_dir`] without the
    /// closure.
    ///
    /// # Errors
    /// [`PrepareError`].
    async fn mint_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<MintedDir, PrepareError>;
}

/// What the frontend's run preparation gives a run that commits into a put directory
/// (`PutDirRun` of `kbf_front::prepare_run`).
#[derive(Debug)]
pub struct PreparedPutDir {
    /// The put directory, `cas/b/<build>/<nonce>`.
    pub dir: String,
    /// The input closure, every `Directory` kept.
    pub kept: KeptClosure,
    /// The Unix second the directory's keys stop being good for the run at.
    pub not_after: u64,
    /// How long the parts of the closure's collection took.
    pub took: PrepareTook,
}

/// What the frontend's run preparation gives a run whose action tree is cached
/// (`MintedPutDir` of `kbf_front::prepare_run`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintedDir {
    /// The put directory, `cas/b/<build>/<nonce>`.
    pub dir: String,
    /// The Unix second the directory's keys stop being good for the run at.
    pub not_after: u64,
}

impl From<PrepareError> for MintError {
    fn from(error: PrepareError) -> Self {
        match error {
            PrepareError::Refused(why) => Self::Refused(why),
            PrepareError::Unavailable(why) => Self::Unavailable(why),
        }
    }
}

/// The frontend's real preparation, [`PutDirPrep`], as the executor asks for it.
#[tonic::async_trait]
impl RunPrepare for PutDirPrep {
    async fn prepare_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        PutDirPrep::prepare_put_dir(self, run, action, now, wall)
            .await
            .map(prepared_put_dir)
            .map_err(prepare_error)
    }

    async fn mint_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<MintedDir, PrepareError> {
        let minted = PutDirPrep::mint_put_dir(self, run, action, now, wall)
            .await
            .map_err(prepare_error)?;
        Ok(MintedDir {
            dir: minted.dir.dir().to_owned(),
            not_after: minted.not_after,
        })
    }
}

fn prepared_put_dir(run: PutDirRun) -> PreparedPutDir {
    PreparedPutDir {
        dir: run.dir.dir().to_owned(),
        kept: run.kept,
        not_after: run.not_after,
        took: run.took,
    }
}

fn prepare_error(error: RunPrepError) -> PrepareError {
    let why = error.to_string();
    match error.fault() {
        PrepareFault::Unavailable => PrepareError::Unavailable(why),
        PrepareFault::Refused => PrepareError::Refused(why),
    }
}

/// What the preparation of task runs is made of: the mint and the preparation, both built from
/// the same secrets as the frontend's task flags. The frontend makes one from its
/// [`PutDirPrep`] ([`RunSource::of_put_dir`]); a test may start one with another preparation
/// ([`crate::frontend::start_frontend_with`]).
pub struct RunSource {
    /// The mint that issues the keys and verifies them in the ingest.
    pub mint: PackKeyMint,
    /// The preparation of a run's put directory.
    pub prepare: Arc<dyn RunPrepare>,
}

impl RunSource {
    /// The source over `prep`, which makes put directories: its own mint, so the ingest
    /// verifies the directories that were issued.
    #[must_use]
    pub fn of_put_dir(prep: &Arc<PutDirPrep>) -> Self {
        Self {
            mint: prep.mint().clone(),
            prepare: prep.clone(),
        }
    }
}

/// What a frontend builds the executor's seams from.
pub struct FrontParts {
    /// The frontend's packs.
    pub packs: Packs,
    /// The frontend's CAS, built over the same packs.
    pub backend: Backend,
    /// The mint that issued the put directories and verifies them.
    pub mint: PackKeyMint,
    /// The preparation of a run.
    pub prepare: Arc<dyn RunPrepare>,
    /// The base environment this executor serves, pinned by digest: the one image an action's
    /// `container-image` may name (an action that names none gets it).
    pub base_image: ImageRef,
    /// The kbf image, pinned by digest, built from [`FrontParts::base_image`]: the image the
    /// facts of every action name.
    pub kbf_image: ImageRef,
    /// How the action trees are written.
    pub put_dir: PutDirParts,
}

/// What the frontend writes the action trees of the runs' put directories with.
pub struct PutDirParts {
    /// The writer of the action trees, over the frontend's CAS and packs.
    pub writer: TreeWriter,
    /// The kbf base tree every action tree is written over: set when the frontend has published
    /// it, before the executor is started.
    pub base: Arc<OnceLock<BaseTree>>,
}

impl PutDirParts {
    /// The parts over `packs` and `backend`, writing over the base tree `base` will hold.
    #[must_use]
    pub fn new(packs: &Packs, backend: &Backend, base: Arc<OnceLock<BaseTree>>) -> Self {
        let clock: kbf_front::Clock = Arc::new(SystemTime::now);
        Self {
            writer: TreeWriter::new(Arc::new(backend.clone()), Arc::new(packs.clone()), clock),
            base,
        }
    }

    /// These parts writing action trees with `retry`.
    #[must_use]
    pub fn with_put_retry(mut self, retry: PutRetry) -> Self {
        self.writer = self.writer.with_put_retry(retry);
        self
    }

    /// These parts with at most `slots` manifest writes open at once.
    #[must_use]
    pub fn with_write_slots(mut self, slots: usize) -> Self {
        self.writer = self.writer.with_write_slots(slots);
        self
    }
}

/// What [`Front`] holds of one run between [`Ingest::mint`] and [`Inputs::tree`].
struct Parked {
    /// The action tree of the run: its digest in hex, or its tree list's key.
    tree: String,
    /// When the directory's keys stop being good: the entry is dropped after.
    not_after: u64,
}

/// The writes of a run's action tree, between [`Ingest::mint`] and [`Ingest::stored`]: the task
/// that writes the tree, which ends with the writes' answer.
struct Pending {
    writes: JoinHandle<Result<(), MintError>>,
    /// When the directory's keys stop being good: the entry is dropped after, and the writes
    /// go on with no one waiting for them.
    not_after: u64,
}

/// The executor's seams over the frontend.
pub struct Front {
    packs: Packs,
    backend: Backend,
    mint: PackKeyMint,
    prepare: Arc<dyn RunPrepare>,
    base_image: ImageRef,
    kbf_image: ImageRef,
    /// The writer of the action trees, shared with the tasks that write them.
    writer: Arc<TreeWriter>,
    /// The kbf base tree every action tree is written over.
    base: Arc<OnceLock<BaseTree>>,
    now: fn() -> SystemTime,
    parked: Mutex<BTreeMap<RunId, Parked>>,
    pending: Mutex<BTreeMap<RunId, Pending>>,
    /// The name of the action tree stored for an action in a form, and when it stops being
    /// good.
    trees: Arc<Mutex<Trees>>,
    /// The facts of an action, by the digest of its Action.
    facts: Mutex<Bounded<(String, i64), ActionFacts>>,
    /// Where the steps of a preparation are timed ([`Step`]).
    metrics: Arc<Metrics>,
}

type Trees = Bounded<(kbf_types::Digest, InputTree), (String, SystemTime)>;

impl Front {
    /// The seams over `parts`.
    #[must_use]
    pub fn new(parts: FrontParts) -> Self {
        Self::with_clock(parts, SystemTime::now)
    }

    /// As [`Front::new`], reading the time from `now`.
    #[must_use]
    pub fn with_clock(parts: FrontParts, now: fn() -> SystemTime) -> Self {
        let FrontParts {
            packs,
            backend,
            mint,
            prepare,
            base_image,
            kbf_image,
            put_dir: PutDirParts { writer, base },
        } = parts;
        Self {
            // The ingest judges the age of a key by the clock the keys are minted by.
            packs: packs.with_clock(Arc::new(now)),
            backend,
            mint,
            prepare,
            base_image,
            kbf_image,
            writer: Arc::new(writer),
            base,
            now,
            parked: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(BTreeMap::new()),
            trees: Arc::new(Mutex::new(Bounded::new(TREE_ENTRIES))),
            facts: Mutex::new(Bounded::new(FACTS_ENTRIES)),
            metrics: Arc::new(Metrics::default()),
        }
    }

    /// These seams timing the steps of each preparation into `metrics`: the executor's own, so
    /// that `GET /metrics` shows them beside the steps the executor times.
    #[must_use]
    pub fn with_metrics(self, metrics: Arc<Metrics>) -> Self {
        Self { metrics, ..self }
    }

    /// These seams remembering at most `trees` action trees and `facts` actions' facts.
    #[cfg(test)]
    fn with_bounds(self, trees: usize, facts: usize) -> Self {
        Self {
            trees: Arc::new(Mutex::new(Bounded::new(trees))),
            facts: Mutex::new(Bounded::new(facts)),
            ..self
        }
    }

    fn parked(&self) -> MutexGuard<'_, BTreeMap<RunId, Parked>> {
        self.parked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> MutexGuard<'_, BTreeMap<RunId, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn trees(&self) -> MutexGuard<'_, Trees> {
        lock_trees(&self.trees)
    }

    fn facts(&self) -> MutexGuard<'_, Bounded<(String, i64), ActionFacts>> {
        self.facts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The name of the action tree remembered for `action` in `form`, if it is good at `now`.
    fn remembered_tree(
        &self,
        action: &kbf_types::Digest,
        form: InputTree,
        now: SystemTime,
    ) -> Option<String> {
        let (tree, expires) = self.trees().get(&(*action, form))?;
        (now < expires).then_some(tree)
    }

    /// The message `digest` is, read from the CAS.
    async fn read<T: prost::Message + Default>(
        &self,
        what: &str,
        digest: &kbf_types::Digest,
    ) -> Result<T, InputsError> {
        let bytes =
            self.backend.read_blob(digest).await.map_err(|e| {
                InputsError::Unavailable(format!("the {what} could not be read: {e}"))
            })?;
        T::decode(bytes)
            .map_err(|e| InputsError::Invalid(format!("the {what} does not decode: {e}")))
    }
}

/// `digest` as the CAS names it.
fn cas_digest(digest: &Digest) -> Result<kbf_types::Digest, String> {
    let size = u64::try_from(digest.size_bytes)
        .map_err(|_| format!("the digest {} has a negative size", digest.hash))?;
    kbf_types::Digest::parse(DigestFunction::Sha256, &format!("{}/{size}", digest.hash))
        .map_err(|e| e.to_string())
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[tonic::async_trait]
impl Ingest for Front {
    async fn mint(
        &self,
        run: RunId,
        action: &Digest,
        wall: Duration,
        tree: InputTree,
    ) -> Result<PackKey, MintError> {
        let action = cas_digest(action).map_err(MintError::Refused)?;
        let now = (self.now)();
        self.mint_put_dir(run, &action, (now, wall), tree).await
    }

    async fn stored(&self, run: RunId) -> Result<(), MintError> {
        let pending = self.pending().remove(&run);
        let Some(pending) = pending else {
            return Ok(());
        };
        pending.writes.await.unwrap_or_else(|_| Err(broke()))
    }

    async fn ingest(
        &self,
        run: RunId,
        key: &PackKey,
        action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested {
        // The key is read first: a key that is not one issued is the wrong key, whatever the
        // action's digest.
        let Some(dir) = put_dir_of(key) else {
            return Ingested::Reject(RejectReason::WrongKey);
        };
        let action = match cas_digest(action) {
            Ok(action) => action,
            Err(why) => return Ingested::Unavailable(why),
        };
        let limits = kbf_front::UntrustedLimits {
            max_pack_bytes: limits.max_pack_bytes,
            max_blob_bytes: limits.max_blob_bytes,
            max_object_entries: limits.max_object_entries,
            max_object_manifest_bytes: limits.max_object_manifest_bytes,
            object_bytes_per_entry: limits.object_bytes_per_entry,
            ..kbf_front::UntrustedLimits::default()
        };
        let (packs, backend, mint) = (&self.packs, &self.backend, &self.mint);
        let writer = Some(&*self.writer);
        let ingest = kbf_front::ingest_put_dir_with;
        let build = build_id(run);
        let done = ingest(packs, backend, mint, &build, &dir, &action, &limits, writer).await;
        match done {
            kbf_front::Ingested::Ran { result, cached } => Ingested::Ran { result, cached },
            kbf_front::Ingested::Reject(why) => Ingested::Reject(reason(why)),
            kbf_front::Ingested::Unavailable(why) => Ingested::Unavailable(why.to_string()),
        }
    }
}

/// The put directory the frontend issued, from its text `cas/b/<build>/<nonce>`.
fn put_dir_of(key: &PackKey) -> Option<IssuedPutDir> {
    let (build, nonce) = key.as_str().strip_prefix("cas/b/")?.split_once('/')?;
    IssuedPutDir::new(build, nonce).ok()
}

/// Refuses a put directory whose keys end before the run's wall time (`wall` from `now`) does.
fn lasts(not_after: u64, now: SystemTime, wall: Duration) -> Result<(), MintError> {
    let needed = seconds(now).saturating_add(wall.as_secs());
    match not_after < needed {
        true => Err(MintError::Refused(
            "the put directory's keys end before the run's wall time does".to_owned(),
        )),
        false => Ok(()),
    }
}

impl Front {
    /// [`Ingest::mint`]: the put directory of the run, and the action tree over the base tree as
    /// `form` says (its manifests, or its tree list), named and parked for [`Inputs::tree`]; the
    /// tree's writes go on after the mint answers, parked for [`Ingest::stored`]. An action tree
    /// remembered for the action in this form is not written again: only the directory is
    /// minted.
    async fn mint_put_dir(
        &self,
        run: RunId,
        action: &kbf_types::Digest,
        (now, wall): (SystemTime, Duration),
        form: InputTree,
    ) -> Result<PackKey, MintError> {
        // Before anything is read or minted: without the base no tree can be written.
        if self.base.get().is_none() {
            return Err(MintError::Unavailable(
                "the kbf base tree is not published: no action tree can be written".to_owned(),
            ));
        }
        let build = build_id(run);
        let prepare = &self.prepare;
        let started = Instant::now();
        let (dir, not_after, tree, writes) = match self.remembered_tree(action, form, now) {
            Some(tree) => {
                let minted = prepare.mint_put_dir(&build, action, now, wall).await;
                self.metrics.step_took(Step::PutDir, started.elapsed());
                let minted = minted.map_err(MintError::from)?;
                lasts(minted.not_after, now, wall)?;
                (minted.dir, minted.not_after, tree, None)
            }
            None => {
                let prepared = prepare.prepare_put_dir(&build, action, now, wall).await;
                self.metrics.step_took(Step::PutDir, started.elapsed());
                let prepared = prepared.map_err(MintError::from)?;
                let took = &prepared.took;
                for (step, took) in [
                    (Step::Action, took.action),
                    (Step::Walk, took.walk),
                    (Step::Held, took.held),
                ] {
                    self.metrics.step_took(step, took);
                }
                lasts(prepared.not_after, now, wall)?;
                let (tree, writes) = self.write_tree(prepared.kept, *action, form, now).await?;
                (prepared.dir, prepared.not_after, tree, Some(writes))
            }
        };
        let live = |held: u64| held >= seconds(now);
        let mut parked = self.parked();
        parked.retain(|_, run| live(run.not_after));
        parked.insert(run, Parked { tree, not_after });
        drop(parked);
        let mut pending = self.pending();
        pending.retain(|_, run| live(run.not_after));
        if let Some(writes) = writes {
            pending.insert(run, Pending { writes, not_after });
        }
        Ok(PackKey::new(dir))
    }

    /// The action tree of `kept`, the closure of `action`, written over the base tree as `form`
    /// says at `now`: its name (its root manifest's digest in hex, or its tree list's key), as
    /// soon as the writer has it, and the task that writes it. The task remembers the tree for
    /// the action once every write of it succeeded, never before. A tree the writer refuses, or
    /// whose lookups fail, has no name: the answer is the writer's.
    async fn write_tree(
        &self,
        kept: KeptClosure,
        action: kbf_types::Digest,
        form: InputTree,
        now: SystemTime,
    ) -> Result<(String, JoinHandle<Result<(), MintError>>), MintError> {
        let started = Instant::now();
        let (writer, base, trees) = (
            Arc::clone(&self.writer),
            Arc::clone(&self.base),
            Arc::clone(&self.trees),
        );
        let (named, name) = oneshot::channel::<String>();
        let writes = tokio::spawn(async move {
            let tree = write_named(&writer, &base, &kept, form, named).await?;
            let expires = now.checked_add(MEMO_TTL).unwrap_or(now);
            lock_trees(&trees).put((action, form), (tree, expires));
            Ok(())
        });
        let Ok(tree) = name.await else {
            // The task ended before it named the tree: its answer is why.
            return Err(match writes.await {
                Ok(Err(why)) => why,
                Ok(Ok(())) | Err(_) => broke(),
            });
        };
        self.metrics.step_took(Step::Tree, started.elapsed());
        Ok((tree, writes))
    }
}

/// The trees remembered behind `trees`' lock.
fn lock_trees(trees: &Mutex<Trees>) -> MutexGuard<'_, Trees> {
    trees.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answer for a task writing an action tree that ended with no answer of its own.
fn broke() -> MintError {
    MintError::Unavailable("the task writing the action tree ended without an answer".to_owned())
}

/// Writes the action tree of `kept` over the base tree `base` holds as `form` says, and sends
/// its name (its root manifest's digest in hex, or its tree list's key) to `named` as soon as
/// the writer has it, before any write. Returns the name once every write succeeded.
async fn write_named(
    writer: &TreeWriter,
    base: &OnceLock<BaseTree>,
    kept: &KeptClosure,
    form: InputTree,
    named: oneshot::Sender<String>,
) -> Result<String, MintError> {
    // The mint checked the base before it collected the closure, and a base once set stays.
    let Some(base) = base.get() else {
        return Err(broke());
    };
    let name = |tree: String| {
        let _ = named.send(tree);
    };
    let tree = match form {
        InputTree::Manifests => (writer
            .write_named(kept, base, |tree| name(tree.digest().to_string()))
            .await)
            .map(|tree| tree.digest().to_string()),
        InputTree::Provider => (writer
            .write_listing_named(kept, base, |listing| name(listing.key().to_owned()))
            .await)
            .map(|listing| listing.key().to_owned()),
    };
    tree.map_err(|no| {
        let why = format!("no action tree: {no}");
        match no.why() {
            "store" => MintError::Unavailable(why),
            _ => MintError::Refused(why),
        }
    })
}

fn reason(why: kbf_front::RejectReason) -> RejectReason {
    use kbf_front::RejectReason as Front;
    match why {
        Front::Missing => RejectReason::Missing,
        Front::WrongKey => RejectReason::WrongKey,
        Front::TooLarge => RejectReason::TooLarge,
        Front::Malformed => RejectReason::Malformed,
        Front::HashMismatch => RejectReason::HashMismatch,
        Front::BadManifest => RejectReason::BadManifest,
        Front::Incomplete => RejectReason::Incomplete,
        Front::Expired => RejectReason::Expired,
        Front::Claimed => RejectReason::Claimed,
    }
}

/// The name of the platform property that names the image.
const IMAGE_PROPERTY: &str = "container-image";

#[tonic::async_trait]
impl Inputs for Front {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        let key = (action.hash.clone(), action.size_bytes);
        let remembered = self.facts().get(&key);
        if let Some(facts) = remembered {
            return Ok(facts);
        }
        let facts = self.read_facts(action).await?;
        self.facts().put(key, facts.clone());
        Ok(facts)
    }

    async fn tree(&self, run: RunId) -> Result<String, InputsError> {
        let parked = self.parked().remove(&run);
        parked
            .map(|parked| parked.tree)
            .ok_or_else(|| InputsError::Unavailable(format!("no tree was written for {run}")))
    }
}

impl Front {
    /// The facts of `action`, read from the CAS.
    async fn read_facts(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        let digest = cas_digest(action).map_err(InputsError::Invalid)?;
        let action: Action = self.read("action", &digest).await?;
        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| InputsError::Invalid("the action names no command".to_owned()))?;
        let command = cas_digest(command).map_err(InputsError::Invalid)?;
        let command: Command = self.read("command", &command).await?;
        let platform = platform_of(&action, &command);
        check_base(platform, &self.base_image, &self.kbf_image)?;
        let timeout = timeout_of(&action)?;
        let system = System::of(platform).map_err(InputsError::Invalid)?;
        // The target and kind of the action, read from its argv and output paths.
        let described = Described::of(command);
        let shape = Shape::of(&described.arguments, &described.outputs);
        Ok(ActionFacts {
            image: self.kbf_image.to_string(),
            timeout,
            system,
            size_class: SizeClass::of_action(&shape),
        })
    }
}

/// Refuses an action whose `container-image` is not the base environment `base` this executor
/// serves. The property is read from the Action's platform or else the Command's; one that
/// names none means the base. Only the manifest digest is compared: two spellings of one digest
/// are one image. The refusal names both images, and the kbf image the run would use.
fn check_base(
    platform: Option<&reapi::Platform>,
    base: &ImageRef,
    kbf: &ImageRef,
) -> Result<(), InputsError> {
    let named = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == IMAGE_PROPERTY))
        .map(|p| p.value.as_str());
    let Some(value) = named else {
        return Ok(());
    };
    let image = ImageRef::parse(value).map_err(|e| InputsError::Invalid(e.to_string()))?;
    match image.digest() == base.digest() {
        true => Ok(()),
        false => Err(InputsError::Invalid(format!(
            "the action's {IMAGE_PROPERTY} {image} is not the base environment this executor \
             serves, {base} (runs are launched from the kbf image {kbf}, which is built from \
             it): name the served base, or none"
        ))),
    }
}

/// The platform of `action` if it names any property, or else of its `command`: as `kbf-front`
/// reads it (`kbf-front`'s `execution.rs`), so the system the executor runs an action on is the
/// one `kbf-front` read for it.
// The Command's platform is deprecated in REAPI 2.2 in favour of the Action's, and clients that
// predate it still send only the Command's: both are read, the Action's first.
#[allow(deprecated)]
fn platform_of<'a>(action: &'a Action, command: &'a Command) -> Option<&'a reapi::Platform> {
    action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref())
}

/// The action's timeout; none if it names none or zero.
fn timeout_of(action: &Action) -> Result<Option<Duration>, InputsError> {
    let Some(timeout) = &action.timeout else {
        return Ok(None);
    };
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(timeout.seconds), u32::try_from(timeout.nanos))
    else {
        return Err(InputsError::Invalid(
            "the action's timeout is negative".to_owned(),
        ));
    };
    let timeout = Duration::new(seconds, nanos);
    Ok((!timeout.is_zero()).then_some(timeout))
}

#[cfg(test)]
#[path = "front_tests.rs"]
mod tests;
