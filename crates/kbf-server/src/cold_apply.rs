//! A storage node applies the cold-store registration at its start
//! (`docs/design/object-store-registration.md`): after it registers, and before it
//! opens its cache, it asks the frontend for the registered store and its keys, probes
//! them, and opens the cache on them, or keeps the store its own flags name and says why.
//!
//! **What the node does**, for an `s3://` `--cold-store` with `--frontend` (and, below, for
//! `registered`):
//! - The frontend's answer to the registration names a generation
//!   ([`Joined::objstore_generation`]); 0 is "nothing registered", and the node starts on
//!   the store its flags and its `AWS_*` key pair name, as it always did.
//! - Otherwise it calls `ColdStore.GetStoreConfig` over the channel it registered on (its
//!   storage certificate), backing off while the frontend cannot answer
//!   ([`fetch_with`]). With a key pair of its own it gives up after
//!   [`FALLBACK_ATTEMPTS`] tries and starts on that (`pending`); without one it cannot
//!   open a cache at all, so it waits for ever, as it already does for the registration.
//! - The registered store must be the store `--cold-store` names: endpoint, bucket and
//!   prefix ([`same_store`]). If it is not, the node keeps its flag store and reports
//!   `identity_mismatch`; the registration supplies keys, never a different store. A node
//!   with a `file://` cold store is always in that case.
//! - The registered keys are probed ([`kbf_objstore::probe`]: one object written, read
//!   and deleted under `.kbf-probe/`). A key pair that fails keeps the node on its own
//!   store, reporting `probe_failed`; the cache is opened on the registered keys only
//!   after the probe passed.
//! - What the node reports ([`Applied`]) is decided here, before the cache opens, and the
//!   heartbeats carry it from the first one on: a node never claims a generation it has
//!   not opened a store on. It reports `pending` for any generation the frontend
//!   announces that it did not apply, higher or lower (a restored state store goes back),
//!   so a lowered generation is never ignored: the node's next start fetches whatever the
//!   frontend holds.
//!
//! **`--cold-store registered`.** The word names no store: the node holds no URL and no key
//! of its own, and the registration is the only place its store comes from.
//! - Before it opens its metadata log, which keeps the objects under the registered
//!   prefix, the node asks for the registration over its storage certificate
//!   ([`registration_with`]) and waits, with the backoff of a fetch, until a generation
//!   above 0 names a store. It is no member of the farm, and touches no store, while it
//!   waits. A refusal that waiting does not change (the node is denied) ends the start.
//! - It runs on the registered endpoint, bucket, prefix and keys. There is no
//!   [`same_store`] comparison: there is no flag store. The registered keys are probed as
//!   above; a failed probe stops the start with [`ColdApplyError::Probe`] (exit 75, the
//!   operator registers a working pair and the restart reads it), and the node never falls
//!   back to a key pair from the environment or a file: neither is read, and
//!   `--cold-store-credentials-file` is refused.
//! - It reports [`Applied::running`] for the generation it fetched, so the frontend shows
//!   the node `ok` at that generation in `kbf cluster object-store status`. A registration
//!   that changes while it runs is `pending`, as for any node, and the node takes it at its
//!   next start: that restart is how a farm moves to another store. A log keeps the key
//!   prefix it was created with ([`crate::RaftMetaLog`]), so a registration that changes
//!   the prefix is refused by the log at that start; it needs an empty `--meta-dir`.
//!
//! **What it does not do.** There is no hot swap: a running node keeps the store it
//! opened. A rotation reaches it at its next start, and until then the frontend shows it
//! `pending`. A node with a key pair of its own and no registration fails nothing: a
//! frontend that is down at the start costs it [`FALLBACK_ATTEMPTS`] short waits.
//!
//! **Where the keys live.** In this process's memory only: the proto message that carries
//! them prints the secret as `Secret(..)` and overwrites it when dropped; the store
//! client keeps its own copy for the life of the process. They are never written, passed
//! as an argument or an environment variable, or logged. [`harden_process`] turns off core
//! dumps for the process before the node can hold them, and makes it not dumpable.

use std::error::Error;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kbf_mapsync::MapHolder;
use kbf_objstore::probe::{ProbeStep, probe};
use kbf_objstore::s3::{Credentials, S3Store};
use kbf_objstore::{KeyPrefix, ObjectStore};
use kbf_proto::kbf::enroll::v1::{GetStoreConfigResponse, StoreConfig};
use kbf_proto::kbf::map::v1::ApplyStatus;
use tonic::{Code, Status};

use crate::config::{ConfigError, s3_store_keyed};
use crate::join::{Joined, Joiner};
use crate::node_map::NodeMap;
use crate::storage_node::StorageFlags;
use crate::stores::{DEFAULT_MIN_PUT_RATE, DEFAULT_REGION, DEFAULT_S3_PREFIX, S3Uri};

/// How many times a node that has a key pair of its own asks for the registration before
/// it starts on that key pair.
pub const FALLBACK_ATTEMPTS: u32 = 4;

/// What a node tells the frontend about the registration, in every heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The generation the open store runs on; 0: none.
    applied: u64,
    /// The generation `status` is about.
    about: u64,
    status: ApplyStatus,
}

impl Applied {
    /// Nothing registered, so nothing to apply: the node runs on its flags.
    #[must_use]
    pub fn nothing() -> Self {
        Self {
            applied: 0,
            about: 0,
            status: ApplyStatus::Ok,
        }
    }

    /// A node that does not apply registrations at all: its heartbeats carry no status,
    /// which the frontend shows as `unsupported`.
    #[must_use]
    pub fn unreported() -> Self {
        Self {
            applied: 0,
            about: 0,
            status: ApplyStatus::Unspecified,
        }
    }

    /// The node opened its store on `generation`.
    #[must_use]
    pub fn running(generation: u64) -> Self {
        Self {
            applied: generation,
            about: generation,
            status: ApplyStatus::Ok,
        }
    }

    /// The node runs on its own store, and `status` says what became of `generation`.
    #[must_use]
    pub fn kept(generation: u64, status: ApplyStatus) -> Self {
        Self {
            applied: 0,
            about: generation,
            status,
        }
    }

    /// The two heartbeat fields, when the frontend last announced `announced`: what the
    /// node decided at its start, unless the frontend has moved on since, in which case
    /// the node has applied nothing of the new generation yet (`pending`). 0 is "the
    /// frontend cannot say now", which changes nothing.
    #[must_use]
    pub fn for_beat(self, announced: u64) -> (u64, ApplyStatus) {
        let stale =
            announced != 0 && announced != self.about && self.status != ApplyStatus::Unspecified;
        (
            self.applied,
            if stale {
                ApplyStatus::Pending
            } else {
                self.status
            },
        )
    }
}

/// Why a node could not start on the store the registration and its flags leave it.
/// The texts are fixed: none carries a key, a bucket or a message of the store.
#[derive(Debug, thiserror::Error)]
pub enum ColdApplyError {
    /// An s3:// store with no key pair of its own, and no registration to give one.
    #[error(
        "{0} must be set for an s3:// store (or the frontend must have a cold store registered)"
    )]
    MissingKeys(&'static str),
    /// The registered store is not the one `--cold-store` names, and the node has no key
    /// pair of its own to run that one with.
    #[error(
        "the registered cold store is not the one --cold-store names, and this node has no \
         key pair of its own (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)"
    )]
    MismatchNoKeys,
    /// The registered keys failed the probe, and the node has no key pair of its own.
    #[error(
        "the registered cold store's keys failed the node's probe: {0}; this node has no key pair of its own"
    )]
    Probe(ProbeStep),
    /// The registered store cannot be used (its configuration is refused).
    #[error("the registered cold store cannot be used; this node has no key pair of its own")]
    Unusable,
    /// The frontend refused to give the registered store.
    #[error(
        "the frontend refused the registered cold store ({0:?}); this node has no key pair of its own"
    )]
    Refused(Code),
    /// The frontend refused a node on `--cold-store registered`, which has no other store to
    /// run on.
    #[error(
        "the frontend refused the registered cold store ({0:?}); a node on --cold-store \
         registered has no other store to run on"
    )]
    RegisteredRefused(Code),
    /// A node on `--cold-store registered` whose metadata log was made under another prefix
    /// than the registration names now.
    #[error(
        "the registered cold store's prefix is {registered:?}, but this node's metadata log \
         was made under {logged:?}: a log keeps the objects under the prefix it was made \
         with, so a changed prefix needs an empty --meta-dir (and the objects copied)"
    )]
    PrefixChanged {
        /// The prefix the registration names.
        registered: String,
        /// The prefix the log was made under.
        logged: String,
    },
    /// `--cold-store registered` without `--frontend`: nothing to ask for the store.
    #[error(
        "--cold-store registered needs --frontend: the store is the one the frontend registers"
    )]
    NoFrontend,
    /// Core dumps could not be turned off, and the node may hold keys.
    #[error("turning off core dumps: {0}")]
    Harden(#[from] io::Error),
}

/// Whether `code` is worth another try: the frontend or the path to it, not the node's
/// standing.
fn retriable(code: Code) -> bool {
    matches!(
        code,
        Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Unknown
            | Code::Internal
            | Code::Aborted
            | Code::ResourceExhausted
            | Code::Cancelled
    )
}

/// One call for the registered store.
pub type Fetch<'a> = Pin<Box<dyn Future<Output = Result<GetStoreConfigResponse, Status>> + 'a>>;

/// Calls `call` until it answers, waiting `pause(failures)` after each failure that is
/// worth another try. `attempts` bounds the calls; `None` goes on for ever. A refusal
/// that waiting does not change (the node is denied) ends it at once. Not generic, so
/// that the lines of the loop are one copy in every test binary.
///
/// # Errors
/// The last status, when it is not worth another try or the attempts are spent.
pub async fn fetch_with<'a>(
    call: &mut dyn FnMut() -> Fetch<'a>,
    pause: &dyn Fn(u32) -> Duration,
    attempts: Option<u32>,
) -> Result<GetStoreConfigResponse, Status> {
    fetch_while(call, pause, attempts, retriable).await
}

/// [`fetch_with`], trying again after a failure when `retry` says its code is worth it.
async fn fetch_while<'a>(
    call: &mut dyn FnMut() -> Fetch<'a>,
    pause: &dyn Fn(u32) -> Duration,
    attempts: Option<u32>,
    retry: fn(Code) -> bool,
) -> Result<GetStoreConfigResponse, Status> {
    let mut failures = 0_u32;
    loop {
        let status = match call().await {
            Ok(answer) => return Ok(answer),
            Err(status) if !retry(status.code()) => return Err(status),
            Err(status) => status,
        };
        failures = failures.saturating_add(1);
        if attempts.is_some_and(|spent| failures >= spent) {
            return Err(status);
        }
        tracing::warn!(
            attempt = failures,
            code = ?status.code(),
            error = %reason(&status),
            "fetching the registered cold store failed; trying again"
        );
        tokio::time::sleep(pause(failures - 1)).await;
    }
}

/// What went wrong in `status`: its message and the chain of errors under it (a transport
/// failure keeps the connect, TLS or DNS error there), so a down frontend, a refused
/// certificate and a wrong address read differently. Holds no key: a status of the
/// frontend's is fixed text, and a transport error names an address and a cause.
fn reason(status: &Status) -> String {
    let mut text = status.message().to_owned();
    let mut source = std::error::Error::source(status);
    while let Some(error) = source {
        text.push_str(": ");
        text.push_str(&error.to_string());
        source = error.source();
    }
    text
}

/// Whether a node with no store of its own tries again after `code`: what [`retriable`] says,
/// and `Unimplemented`, a frontend too old to serve `ColdStore` (the frontend is updated
/// first, so a rolling upgrade meets one). Waiting is what `registered` means. The codes
/// that stay fatal are the node's own standing, which a later ask does not change:
/// `PermissionDenied` and `Unauthenticated` (its certificate is denied or not accepted),
/// `InvalidArgument` and `FailedPrecondition` (a request this build makes wrongly), `NotFound`.
fn retriable_registered(code: Code) -> bool {
    retriable(code) || code == Code::Unimplemented
}

/// Waits for the store a node started with `--cold-store registered` runs on: calls `call`
/// until an answer names a store, waiting `pause(n)` after each failure the frontend or the
/// path to it is to blame for ([`fetch_with`]) and after each answer that names none
/// (nothing registered, or taken back since the generation was announced). Goes on for
/// ever: a node with no store of its own has nothing else to do. A failure is warned with its
/// attempt, code and reason ([`fetch_while`]). Not generic, as [`fetch_with`] is not. Retries
/// what [`retriable_registered`] says.
///
/// # Errors
/// The status of a refusal that waiting does not change (the node is denied).
pub async fn registration_with<'a>(
    call: &mut dyn FnMut() -> Fetch<'a>,
    pause: &dyn Fn(u32) -> Duration,
) -> Result<(u64, StoreConfig), Status> {
    let mut empty = 0_u32;
    loop {
        let reply = fetch_while(call, pause, None, retriable_registered).await?;
        let generation = reply.generation;
        if let Some(config) = reply.config {
            return Ok((generation, config));
        }
        empty = empty.saturating_add(1);
        tracing::warn!(
            attempt = empty,
            generation,
            "no cold store is registered yet; waiting for one"
        );
        tokio::time::sleep(pause(empty - 1)).await;
    }
}

/// Whether `config` names the store `flag` does: the same endpoint, bucket and prefix.
/// Keys, region and the put settings are not identity.
#[must_use]
pub fn same_store(flag: &S3Uri, config: &StoreConfig) -> bool {
    let endpoint = |text: &str| text.trim_end_matches('/').to_ascii_lowercase();
    flag.bucket == config.bucket
        && flag.prefix == prefix_of(&config.prefix)
        && endpoint(&flag.endpoint) == endpoint(&config.endpoint)
}

/// A registered prefix as a store URI spells its own: ending in `/`, `kbf/` when empty.
fn prefix_of(prefix: &str) -> String {
    match prefix {
        "" => DEFAULT_S3_PREFIX.to_owned(),
        p if p.ends_with('/') => p.to_owned(),
        p => format!("{p}/"),
    }
}

/// Why registered keys were not used: the closed reasons a node reports as `probe_failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unusable {
    Probe(ProbeStep),
    Config,
}

/// What a node with no key pair of its own is told when the registered keys were not used.
fn refusal(why: Unusable) -> ColdApplyError {
    match why {
        Unusable::Probe(step) => ColdApplyError::Probe(step),
        Unusable::Config => ColdApplyError::Unusable,
    }
}

/// The address and put settings of the store `config` registers: its own, and the flag's
/// region or minimum put rate where it names none.
fn registered_uri(flag: &S3Uri, config: &StoreConfig) -> S3Uri {
    S3Uri {
        bucket: config.bucket.clone(),
        prefix: prefix_of(&config.prefix),
        endpoint: config.endpoint.clone(),
        region: if config.region.is_empty() {
            flag.region.clone()
        } else {
            config.region.clone()
        },
        conditional_put: config.conditional_put,
        min_put_rate: NonZeroU64::new(config.min_put_rate).unwrap_or(flag.min_put_rate),
    }
}

/// The store `config` names with its keys, probed as `nonce`.
async fn registered_store(
    flag: &S3Uri,
    config: &StoreConfig,
    nonce: &str,
) -> Result<S3Store, Unusable> {
    let uri = registered_uri(flag, config);
    let root = KeyPrefix::new(uri.prefix.clone()).map_err(|_| Unusable::Config)?;
    let credentials = Credentials::new(&config.access_key_id, config.secret_access_key.clone());
    let store = s3_store_keyed(&uri, credentials).map_err(|_| Unusable::Config)?;
    probe(&store, &root, nonce).await.map_err(Unusable::Probe)?;
    Ok(store)
}

/// The cold store a storage node opens its cache on, chosen after it registers.
pub(crate) trait ColdPick {
    /// The store type the cache is built on.
    type Store: ObjectStore;

    /// The store for a node with no `--frontend`: its flags' own.
    ///
    /// # Errors
    /// The flags' store cannot be built.
    fn alone(self) -> Result<Self::Store, Box<dyn Error>>;

    /// The store for a node registered as `joined` with `joiner`'s frontend, and what to
    /// report about the registration.
    ///
    /// # Errors
    /// The node can run on neither the registered store nor its own.
    fn registered(
        self,
        joiner: &Joiner,
        joined: &Joined,
    ) -> impl Future<Output = Result<(Self::Store, Applied), Box<dyn Error>>>;
}

/// A store the flags name and no registration can replace (a `file://` store).
pub(crate) struct Fixed<O>(pub O);

impl<O: ObjectStore> ColdPick for Fixed<O> {
    type Store = O;

    fn alone(self) -> Result<O, Box<dyn Error>> {
        Ok(self.0)
    }

    async fn registered(self, _: &Joiner, joined: &Joined) -> Result<(O, Applied), Box<dyn Error>> {
        let announced = joined.objstore_generation;
        let applied = if announced == 0 {
            Applied::nothing()
        } else {
            // An S3 registration is never the identity of a local directory.
            Applied::kept(announced, ApplyStatus::IdentityMismatch)
        };
        Ok((self.0, applied))
    }
}

/// An `s3://` `--cold-store`, whose keys a registration may supply.
pub(crate) struct Registrable {
    flag: S3Uri,
    /// The store on the node's own key pair, or the name of the variable missing.
    own: Result<S3Store, &'static str>,
}

impl Registrable {
    /// The flag's store, with the key pair `env` gives. Not generic over `env`, so that the
    /// three ways it ends are one copy in every test binary.
    ///
    /// # Errors
    /// The flag's configuration is refused (a missing key pair is not an error yet: the
    /// registration may supply it).
    pub(crate) fn new(
        uri: &S3Uri,
        file: Option<&std::path::Path>,
        env: fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let own = match crate::key_file::cold_s3_store(uri, file, env) {
            Ok(store) => Ok(store),
            Err(ConfigError::MissingEnv(name)) => Err(name),
            Err(other) => return Err(other),
        };
        Ok(Self {
            flag: uri.clone(),
            own,
        })
    }

    /// The node's own store with `applied`, or the error that its missing keys are.
    fn keep(self, applied: Applied) -> Result<(S3Store, Applied), Box<dyn Error>> {
        let store = self.own.map_err(ColdApplyError::MissingKeys)?;
        Ok((store, applied))
    }

    /// The node's own store with `applied`, or `failed` when it has none.
    fn keep_or(
        self,
        applied: Applied,
        failed: ColdApplyError,
    ) -> Result<(S3Store, Applied), Box<dyn Error>> {
        match self.own {
            Ok(store) => Ok((store, applied)),
            Err(_) => Err(failed.into()),
        }
    }
}

impl ColdPick for Registrable {
    type Store = S3Store;

    fn alone(self) -> Result<S3Store, Box<dyn Error>> {
        Ok(self.own.map_err(ColdApplyError::MissingKeys)?)
    }

    async fn registered(
        self,
        joiner: &Joiner,
        joined: &Joined,
    ) -> Result<(S3Store, Applied), Box<dyn Error>> {
        let announced = joined.objstore_generation;
        if announced == 0 {
            return self.keep(Applied::nothing());
        }
        let attempts = self.own.is_ok().then_some(FALLBACK_ATTEMPTS);
        let fetched = fetch_with(
            &mut || Box::pin(joiner.fetch_store_config(0)),
            &|failures| joiner.pause(failures),
            attempts,
        )
        .await;
        let reply = match fetched {
            Ok(reply) => reply,
            Err(status) => {
                tracing::warn!("the registered cold store was not fetched");
                let refused = ColdApplyError::Refused(status.code());
                return self.keep_or(Applied::kept(announced, ApplyStatus::Pending), refused);
            }
        };
        let generation = reply.generation;
        let Some(config) = reply.config.as_ref() else {
            // Taken back between the registration and the fetch.
            return self.keep(Applied::nothing());
        };
        if !same_store(&self.flag, config) {
            tracing::warn!("the registered cold store differs from --cold-store");
            let mismatch = Applied::kept(generation, ApplyStatus::IdentityMismatch);
            return self.keep_or(mismatch, ColdApplyError::MismatchNoKeys);
        }
        let nonce = format!("{}-{}", joined.member_id, joined.boot);
        match registered_store(&self.flag, config, &nonce).await {
            Ok(store) => Ok((store, Applied::running(generation))),
            Err(why) => {
                tracing::warn!("the registered cold store's keys were not used");
                let failed = Applied::kept(generation, ApplyStatus::ProbeFailed);
                self.keep_or(failed, refusal(why))
            }
        }
    }
}

/// The address and put settings a store starts from when nothing else names them: those of a
/// store URI that gives none.
fn defaults() -> S3Uri {
    S3Uri {
        bucket: String::new(),
        prefix: DEFAULT_S3_PREFIX.to_owned(),
        endpoint: String::new(),
        region: DEFAULT_REGION.to_owned(),
        conditional_put: false,
        min_put_rate: DEFAULT_MIN_PUT_RATE,
    }
}

/// `--cold-store registered`: the registration, fetched before the node's log opens, and the
/// only store the node can run on.
pub(crate) struct RegisteredOnly {
    generation: u64,
    config: StoreConfig,
}

impl RegisteredOnly {
    /// Asks `flags`' frontend for the registration until it names a store
    /// ([`registration_with`]).
    ///
    /// # Errors
    /// [`ColdApplyError::NoFrontend`]; the joiner cannot be made (a TLS file is refused);
    /// [`ColdApplyError::Refused`] when the frontend denies the node.
    pub(crate) async fn wait(flags: &StorageFlags) -> Result<Self, Box<dyn Error>> {
        let node = Arc::new(NodeMap::new(Arc::new(MapHolder::new())));
        let joiner = flags
            .joiner(flags.listen, node)?
            .ok_or(ColdApplyError::NoFrontend)?;
        tracing::info!(
            frontend = flags.frontend.as_deref().unwrap_or_default(),
            "waiting for the registered cold store"
        );
        let (generation, config) = registration_with(
            &mut || Box::pin(joiner.fetch_store_config(0)),
            &|failures| joiner.pause(failures),
        )
        .await
        .map_err(|status| ColdApplyError::RegisteredRefused(status.code()))?;
        Ok(Self { generation, config })
    }

    /// The key prefix the node's log keeps its objects under: the registered prefix, as a
    /// store URI spells its own.
    pub(crate) fn prefix_root(&self) -> String {
        prefix_of(&self.config.prefix)
    }
}

/// `error`, the failure of a start of a node on `--cold-store registered`, told as
/// [`ColdApplyError::PrefixChanged`] when it is the metadata log refusing the prefix
/// `registered` names (the log's `Bootstrap` holds the prefix it was made under).
pub(crate) fn explain_prefix(error: Box<dyn Error>, registered: &str) -> Box<dyn Error> {
    let Some(crate::RaftLogError::Mismatch {
        committed: Some(committed),
        ..
    }) = error.downcast_ref::<crate::RaftLogError>()
    else {
        return error;
    };
    // The log's own prefix is `<root><log id>/`; the registration names the root.
    let own = committed.store_prefix.as_str();
    let logged = own
        .strip_suffix(&format!("{}/", committed.log_id))
        .unwrap_or(own);
    if logged == registered {
        return error;
    }
    ColdApplyError::PrefixChanged {
        registered: registered.to_owned(),
        logged: logged.to_owned(),
    }
    .into()
}

impl ColdPick for RegisteredOnly {
    type Store = S3Store;

    fn alone(self) -> Result<S3Store, Box<dyn Error>> {
        Err(ColdApplyError::NoFrontend.into())
    }

    async fn registered(
        self,
        _: &Joiner,
        joined: &Joined,
    ) -> Result<(S3Store, Applied), Box<dyn Error>> {
        let nonce = format!("{}-{}", joined.member_id, joined.boot);
        match registered_store(&defaults(), &self.config, &nonce).await {
            Ok(store) => Ok((store, Applied::running(self.generation))),
            Err(why) => {
                tracing::warn!("the registered cold store's keys were not used");
                Err(refusal(why).into())
            }
        }
    }
}

/// Makes the process refuse to leave its memory in a file: no core dump (`RLIMIT_CORE` 0),
/// and where the system has the control, not
/// dumpable (`PR_SET_DUMPABLE` 0: no other process of the same user can `ptrace` it or
/// read `/proc/self/mem`). A node that registers with a frontend may hold the cold store's
/// keys, so it calls this before it starts.
///
/// Not dumpable makes a process's `/proc/self` entries root-owned, but the kernel lets a
/// process open its own `/proc/self/fd` and follow its entries whatever their owner
/// (`proc_fd_permission`, `proc_fd_access_allowed`). The worker daemon of
/// `kbf storage-and-daemon` shares the process and needs exactly that to chmod a read-only
/// directory before it removes the tree (`kbf_outputs::remove_tree`); a test of that
/// mode pins that it still works.
///
/// # Errors
/// The limit cannot be set.
pub fn harden_process() -> Result<(), ColdApplyError> {
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(io::Error::from)?;
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(io::Error::from)?;
    Ok(())
}

#[cfg(test)]
#[path = "cold_apply_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "cold_registered_tests.rs"]
mod registered_tests;
