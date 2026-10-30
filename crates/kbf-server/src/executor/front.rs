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
//!   under the lease and handed out once. The write host and the size cap are the
//!   preparation's: it signs for one store endpoint and the signer limits the object. A run
//!   that cannot be prepared is the caller's ([`MintError::Refused`]) unless the store could
//!   not be read ([`MintError::Unavailable`]): see [`RunPrep`]'s [`PrepareFault`].
//! - **The action's facts** are read from the Action and the Command in the CAS: the image from
//!   the `container-image` platform property, which must name the image by digest (an action
//!   that names none is run in the operator's default image, if there is one), and the
//!   timeout. Whether the image sets an `ENTRYPOINT` is in the image's config, which the
//!   frontend never reads, so it is not known: every action is refused as such unless the
//!   operator said no image of the farm sets one.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::prepare_run::{PrepareError as RunPrepError, PrepareFault, RunPrep};
use kbf_front::{Backend, IssuedPackKey, PackKeyMint, Packs};
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
    /// Whether the operator says no image of the farm sets an `ENTRYPOINT`.
    pub images_have_no_entrypoint: bool,
    /// The image, pinned by digest, of an action that names none.
    pub default_image: Option<ImageRef>,
}

/// What [`Front`] holds of one run between the three steps.
struct Parked {
    key: String,
    put_url: Option<Secret>,
    grant: Option<ReadGrant>,
    /// When the write and the grant stop working: the entry is dropped after.
    not_after: u64,
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
        let Some(issued) = issued_of(key) else {
            return Ingested::Reject(RejectReason::WrongKey);
        };
        let action = match cas_digest(action) {
            Ok(action) => action,
            Err(why) => return Ingested::Unavailable(why),
        };
        let limits = kbf_front::UntrustedLimits {
            max_pack_bytes: limits.max_pack_bytes,
            max_blob_bytes: limits.max_blob_bytes,
            ..kbf_front::UntrustedLimits::default()
        };
        let done = kbf_front::ingest(
            &self.parts.packs,
            &self.parts.backend,
            &self.parts.mint,
            &build_id(lease),
            &issued,
            &action,
            &limits,
        )
        .await;
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
    }
}

#[tonic::async_trait]
impl Presign for Front {
    async fn put_url(
        &self,
        _host: &str,
        key: &PackKey,
        _expires: Duration,
        _max_bytes: u64,
    ) -> Result<Secret, PresignError> {
        let mut parked = self.parked();
        let url = parked
            .values_mut()
            .find(|run| run.key == key.as_str())
            .and_then(|run| run.put_url.take());
        parked.retain(|_, run| run.put_url.is_some() || run.grant.is_some());
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
        let image = image_of(&action, &command, self.parts.default_image.as_ref())?;
        if !self.parts.images_have_no_entrypoint {
            return Err(InputsError::Invalid(
                "whether the action's image sets an ENTRYPOINT is in the image's config, which \
                 the frontend does not read, and the task tool cannot run under one: set \
                 --executor-images-have-no-entrypoint when no image of the farm sets one"
                    .to_owned(),
            ));
        }
        Ok(ActionFacts {
            image,
            timeout: timeout_of(&action)?,
            has_entrypoint: false,
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
        parked.retain(|_, run| run.put_url.is_some() || run.grant.is_some());
        grant.ok_or_else(|| InputsError::Unavailable(format!("no grant was prepared for {lease}")))
    }
}

/// The action's image, by digest, from the platform of the Action or else the Command's; the
/// operator's `default` if the action names none.
#[allow(deprecated)]
fn image_of(
    action: &Action,
    command: &Command,
    default: Option<&ImageRef>,
) -> Result<String, InputsError> {
    let platform: Option<&reapi::Platform> = action.platform.as_ref().or(command.platform.as_ref());
    let named = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == IMAGE_PROPERTY))
        .map(|p| p.value.as_str());
    let Some(value) = named else {
        return default.map(ImageRef::to_string).ok_or_else(|| {
            InputsError::Invalid(format!(
                "the action names no {IMAGE_PROPERTY} property, and the executor has no \
                 --executor-default-image"
            ))
        });
    };
    ImageRef::parse(value)
        .map(|image| image.to_string())
        .map_err(|e| InputsError::Invalid(e.to_string()))
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
