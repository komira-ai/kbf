//! The executor's seams, over what the frontend owns: [`Front`] implements [`Ingest`],
//! [`Presign`] and [`Inputs`].
//!
//! - **Ingest** is [`kbf_front::ingest`] over the frontend's [`Packs`], a [`Backend`] built over
//!   the same packs (the Action it reads to decide on caching may be a blob the frontend keeps
//!   in a pack), and the [`PackKeyMint`] that issued the key. The lease it is given is the farm
//!   lease's build id ([`build_id`]), the same string the task tool is told with `--lease`.
//! - **The key, the write and the grant** come from one call of the frontend's run preparation
//!   ([`RunPrepare`], which the frontend implements over `prepare_run_for` of
//!   `kbf_front::prepare_run`): the pack key, one presigned `PUT` of exactly that key, and the
//!   read grant. The executor asks for them in three steps ([`Ingest::mint`], then
//!   [`Presign::put_url`], then [`Inputs::read_grant`]), so what the one call returned is parked
//!   under the farm lease and handed out once. The write host and the size cap are the
//!   preparation's: it signs for one store endpoint and the signer limits the object. A run
//!   that cannot be prepared is the caller's ([`MintError::Refused`]) unless the store could
//!   not be read ([`MintError::Unavailable`]): see [`RunPrep`]'s [`PrepareFault`].
//! - **A put directory** (`--task-out-mode put-dir`, [`FrontParts::put_dir`]): the preparation
//!   mints the run's put directory and collects its closure ([`RunPrepare::prepare_put_dir`]),
//!   and [`Ingest::mint`] writes the action tree over the published base tree ([`TreeWriter`])
//!   and parks its digest, which [`Inputs::tree`] hands out once. No write is signed and no
//!   grant issued. The ingest is [`kbf_front::ingest_put_dir`] of the directory.
//! - **The action's facts** are read from the Action and the Command in the CAS: the
//!   `container-image` platform property, which names the BASE environment the Command sees and
//!   must be, by digest, the base the executor serves (an action that names none uses it); and
//!   the timeout. The image a run is launched from is the kbf image, built from that base and
//!   set by the operator: it keeps its `ENTRYPOINT` (the task tool), so no action's image is
//!   launched and no entrypoint is looked for.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::action_tree::{BaseTree, TreeWriter};
use kbf_front::prepare_run::{KeptClosure, PrepareError as RunPrepError, PrepareFault, RunPrep};
use kbf_front::{Backend, IssuedPackKey, IssuedPutDir, PackKeyMint, Packs};
use kbf_proto::reapi::{self, Action, Command, Digest};
use kbf_types::{DigestFunction, ImageRef, LeaseId};
use std::sync::Arc;

use crate::executor::task::{PackKey, Secret, build_id};
use crate::executor::traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, Presign, PresignError,
    ReadGrant, RejectReason, UntrustedLimits,
};

/// What the frontend's run preparation gives one run (`RunInputs` of
/// `kbf_front::prepare_run`, as values).
#[derive(Clone)]
pub struct PreparedRun {
    /// The pack key the task may write: `cas/b/<build>/<nonce>-0`.
    pub key: String,
    /// One presigned `PUT` of exactly [`PreparedRun::key`].
    pub put_url: Secret,
    /// The base url of the read listener.
    pub grant_url: String,
    /// The bearer token of the read grant.
    pub grant_token: Secret,
    /// The Unix second the write and the grant stop working at.
    pub not_after: u64,
}

/// Why a run could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// The inputs are not all held, are over budget, or the wall is over the presign ceiling.
    #[error("{0}")]
    Refused(String),
    /// The store could not answer.
    #[error("{0}")]
    Unavailable(String),
}

/// The frontend's preparation of a run: the key, the write and the grant.
#[tonic::async_trait]
pub trait RunPrepare: Send + Sync {
    /// The inputs of the run of `action` on `lease` (a build id), started at `now`, which may
    /// take `wall`: the write and the grant are good for it.
    ///
    /// # Errors
    /// [`PrepareError`].
    async fn prepare_run(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedRun, PrepareError>;

    /// The run of `action` on `lease`, started at `now`, which may take `wall`, that commits
    /// into a put directory: the directory and the closure its action tree is written from.
    ///
    /// # Errors
    /// [`PrepareError`]; by default [`PrepareError::Refused`], for a preparation that makes no
    /// put directory.
    async fn prepare_put_dir(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        let _ = (lease, action, now, wall);
        Err(PrepareError::Refused(
            "this preparation makes no put directory".to_owned(),
        ))
    }
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
}

impl From<PrepareError> for MintError {
    fn from(error: PrepareError) -> Self {
        match error {
            PrepareError::Refused(why) => Self::Refused(why),
            PrepareError::Unavailable(why) => Self::Unavailable(why),
        }
    }
}

/// The frontend's real preparation, [`RunPrep`], as the executor asks for it: its own wall per
/// run (`prepare_run_for`), the values of its [`RunInputs`](kbf_front::prepare_run::RunInputs)
/// with the two secrets wrapped, and its refusal by whose it is to fix.
///
/// No error text carries the presigned url or the token: they exist only in a success.
#[tonic::async_trait]
impl RunPrepare for RunPrep {
    async fn prepare_run(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedRun, PrepareError> {
        let run = self
            .prepare_run_for(lease, action, now, wall)
            .await
            .map_err(prepare_error)?;
        Ok(PreparedRun {
            key: run.key.key().to_owned(),
            put_url: Secret::new(run.put_url),
            grant_url: run.read_grant.url,
            grant_token: Secret::new(run.read_grant.token.expose()),
            not_after: run.read_grant.not_after,
        })
    }

    async fn prepare_put_dir(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        let run = RunPrep::prepare_put_dir(self, lease, action, now, wall)
            .await
            .map_err(prepare_error)?;
        Ok(PreparedPutDir {
            dir: run.dir.dir().to_owned(),
            kept: run.kept,
            not_after: run.not_after,
        })
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
/// [`RunPrep`] ([`RunSource::of`]); a test may start one with another preparation
/// ([`crate::frontend::start_frontend_with`]).
pub struct RunSource {
    /// The mint that issues the keys and verifies them in the ingest.
    pub mint: PackKeyMint,
    /// The preparation of a run's write and read grant.
    pub prepare: Arc<dyn RunPrepare>,
}

impl RunSource {
    /// The source over `prep`: its own mint, which signs the keys it prepares, so the ingest
    /// verifies the keys that were issued.
    #[must_use]
    pub fn of(prep: &Arc<RunPrep>) -> Self {
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
    /// The mint that issued the keys and verifies them.
    pub mint: PackKeyMint,
    /// The preparation of a run.
    pub prepare: Arc<dyn RunPrepare>,
    /// The base environment this executor serves, pinned by digest: the one image an action's
    /// `container-image` may name (an action that names none gets it).
    pub base_image: ImageRef,
    /// The kbf image, pinned by digest, built from [`FrontParts::base_image`]: the image every
    /// run of presigned writes is launched from.
    pub kbf_image: ImageRef,
    /// With `--task-out-mode put-dir`, how the action trees are written; `None` for presigned
    /// writes.
    pub put_dir: Option<PutDirParts>,
}

/// What a frontend whose tasks commit into put directories writes their action trees with.
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
}

/// What [`Front`] holds of one run between the three steps.
struct Parked {
    key: String,
    put_url: Option<Secret>,
    grant: Option<ReadGrant>,
    /// The digest of the action tree of a put-directory run, in hex.
    tree: Option<String>,
    /// When the write and the grant stop working: the entry is dropped after.
    not_after: u64,
}

impl Parked {
    /// Whether something of the run is still to be handed out.
    fn holds_any(&self) -> bool {
        self.put_url.is_some() || self.grant.is_some() || self.tree.is_some()
    }
}

/// The executor's seams over the frontend.
pub struct Front {
    parts: FrontParts,
    now: fn() -> SystemTime,
    parked: Mutex<BTreeMap<LeaseId, Parked>>,
}

impl Front {
    /// The seams over `parts`.
    #[must_use]
    pub fn new(parts: FrontParts) -> Self {
        Self::with_clock(parts, SystemTime::now)
    }

    /// As [`Front::new`], reading the time from `now`.
    #[must_use]
    pub fn with_clock(parts: FrontParts, now: fn() -> SystemTime) -> Self {
        // The ingest judges the age of a key by the clock the keys are minted by.
        let parts = FrontParts {
            packs: parts.packs.with_clock(Arc::new(now)),
            ..parts
        };
        Self {
            parts,
            now,
            parked: Mutex::new(BTreeMap::new()),
        }
    }

    fn parked(&self) -> MutexGuard<'_, BTreeMap<LeaseId, Parked>> {
        self.parked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The message `digest` is, read from the CAS.
    async fn read<T: prost::Message + Default>(
        &self,
        what: &str,
        digest: &kbf_types::Digest,
    ) -> Result<T, InputsError> {
        let bytes =
            self.parts.backend.read_blob(digest).await.map_err(|e| {
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
        lease: LeaseId,
        action: &Digest,
        wall: Duration,
    ) -> Result<PackKey, MintError> {
        let action = cas_digest(action).map_err(MintError::Refused)?;
        let now = (self.now)();
        if let Some(put_dir) = &self.parts.put_dir {
            return self.mint_put_dir(put_dir, lease, &action, now, wall).await;
        }
        let prepared = self
            .parts
            .prepare
            .prepare_run(&build_id(lease), &action, now, wall)
            .await
            .map_err(MintError::from)?;
        let needed = seconds(now).saturating_add(wall.as_secs());
        if prepared.not_after < needed {
            return Err(MintError::Refused(
                "the write and the grant end before the run's wall time does".to_owned(),
            ));
        }
        let key = PackKey::new(prepared.key.clone());
        let mut parked = self.parked();
        // What a start that ran out of its window left behind.
        parked.retain(|_, run| run.not_after >= seconds(now));
        parked.insert(
            lease,
            Parked {
                key: prepared.key,
                put_url: Some(prepared.put_url),
                grant: Some(ReadGrant {
                    url: prepared.grant_url,
                    token: prepared.grant_token,
                }),
                tree: None,
                not_after: prepared.not_after,
            },
        );
        Ok(key)
    }

    async fn ingest(
        &self,
        lease: LeaseId,
        key: &PackKey,
        action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested {
        let action = match cas_digest(action) {
            Ok(action) => action,
            Err(why) => return Ingested::Unavailable(why),
        };
        let limits = kbf_front::UntrustedLimits {
            max_pack_bytes: limits.max_pack_bytes,
            max_blob_bytes: limits.max_blob_bytes,
            ..kbf_front::UntrustedLimits::default()
        };
        let (packs, backend, mint) = (&self.parts.packs, &self.parts.backend, &self.parts.mint);
        let build = build_id(lease);
        let done = if self.parts.put_dir.is_some() {
            let Some(dir) = put_dir_of(key) else {
                return Ingested::Reject(RejectReason::WrongKey);
            };
            kbf_front::ingest_put_dir(packs, backend, mint, &build, &dir, &action, &limits).await
        } else {
            let Some(issued) = issued_of(key) else {
                return Ingested::Reject(RejectReason::WrongKey);
            };
            kbf_front::ingest(packs, backend, mint, &build, &issued, &action, &limits).await
        };
        match done {
            kbf_front::Ingested::Ran { result, cached } => Ingested::Ran { result, cached },
            kbf_front::Ingested::Reject(why) => Ingested::Reject(reason(why)),
            kbf_front::Ingested::Unavailable(why) => Ingested::Unavailable(why.to_string()),
        }
    }
}

/// The key the frontend issued, from its text `cas/b/<build>/<nonce>`.
fn issued_of(key: &PackKey) -> Option<IssuedPackKey> {
    let (build, nonce) = key.as_str().strip_prefix("cas/b/")?.split_once('/')?;
    IssuedPackKey::new(build, nonce).ok()
}

/// The put directory the frontend issued, from its text `cas/b/<build>/<nonce>`.
fn put_dir_of(key: &PackKey) -> Option<IssuedPutDir> {
    let (build, nonce) = key.as_str().strip_prefix("cas/b/")?.split_once('/')?;
    IssuedPutDir::new(build, nonce).ok()
}

impl Front {
    /// [`Ingest::mint`] of a run that commits into a put directory: the directory, and the
    /// action tree written over the base tree and parked for [`Inputs::tree`].
    async fn mint_put_dir(
        &self,
        put_dir: &PutDirParts,
        lease: LeaseId,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PackKey, MintError> {
        let prepared = self
            .parts
            .prepare
            .prepare_put_dir(&build_id(lease), action, now, wall)
            .await
            .map_err(MintError::from)?;
        let needed = seconds(now).saturating_add(wall.as_secs());
        if prepared.not_after < needed {
            return Err(MintError::Refused(
                "the put directory's keys end before the run's wall time does".to_owned(),
            ));
        }
        let Some(base) = put_dir.base.get() else {
            return Err(MintError::Unavailable(
                "the kbf base tree is not published: no action tree can be written".to_owned(),
            ));
        };
        let tree = put_dir.writer.write(&prepared.kept, base).await;
        let tree = tree.map_err(|no| {
            let why = format!("no action tree: {no}");
            match no.why() {
                "store" => MintError::Unavailable(why),
                _ => MintError::Refused(why),
            }
        })?;
        let mut parked = self.parked();
        parked.retain(|_, run| run.not_after >= seconds(now));
        parked.insert(
            lease,
            Parked {
                key: prepared.dir.clone(),
                put_url: None,
                grant: None,
                tree: Some(tree.digest().to_string()),
                not_after: prepared.not_after,
            },
        );
        Ok(PackKey::new(prepared.dir))
    }
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

#[tonic::async_trait]
impl Presign for Front {
    async fn put_url(
        &self,
        key: &PackKey,
        _expires: Duration,
        _max_bytes: u64,
    ) -> Result<Secret, PresignError> {
        let mut parked = self.parked();
        let url = parked
            .values_mut()
            .find(|run| run.key == key.as_str())
            .and_then(|run| run.put_url.take());
        parked.retain(|_, run| run.holds_any());
        url.ok_or_else(|| PresignError::Refused(format!("no write was prepared for {key}")))
    }
}

/// The name of the platform property that names the image.
const IMAGE_PROPERTY: &str = "container-image";

#[tonic::async_trait]
impl Inputs for Front {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        let digest = cas_digest(action).map_err(InputsError::Invalid)?;
        let action: Action = self.read("action", &digest).await?;
        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| InputsError::Invalid("the action names no command".to_owned()))?;
        let command = cas_digest(command).map_err(InputsError::Invalid)?;
        let command: Command = self.read("command", &command).await?;
        check_base(
            &action,
            &command,
            &self.parts.base_image,
            &self.parts.kbf_image,
        )?;
        Ok(ActionFacts {
            image: self.parts.kbf_image.to_string(),
            timeout: timeout_of(&action)?,
        })
    }

    async fn read_grant(
        &self,
        lease: LeaseId,
        _action: &Digest,
        _expires: Duration,
    ) -> Result<ReadGrant, InputsError> {
        let mut parked = self.parked();
        let grant = parked.get_mut(&lease).and_then(|run| run.grant.take());
        parked.retain(|_, run| run.holds_any());
        grant.ok_or_else(|| InputsError::Unavailable(format!("no grant was prepared for {lease}")))
    }

    async fn tree(&self, lease: LeaseId) -> Result<String, InputsError> {
        let mut parked = self.parked();
        let tree = parked.get_mut(&lease).and_then(|run| run.tree.take());
        parked.retain(|_, run| run.holds_any());
        tree.ok_or_else(|| InputsError::Unavailable(format!("no tree was written for {lease}")))
    }
}

/// Refuses an action whose `container-image` is not the base environment `base` this executor
/// serves. The property is read from the Action's platform or else the Command's; one that
/// names none means the base. Only the manifest digest is compared: two spellings of one digest
/// are one image. The refusal names both images, and the kbf image the run would use.
#[allow(deprecated)]
fn check_base(
    action: &Action,
    command: &Command,
    base: &ImageRef,
    kbf: &ImageRef,
) -> Result<(), InputsError> {
    let platform: Option<&reapi::Platform> = action.platform.as_ref().or(command.platform.as_ref());
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
