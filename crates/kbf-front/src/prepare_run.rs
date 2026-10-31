//! Preparing one run: what the frontend hands the executor for a task before it starts.
//!
//! [`RunPrep::prepare_run`] turns a lease and the Action it runs into [`RunInputs`], values
//! the executor delivers to `kbf-task` as it chooses (this module writes no file and reads no
//! environment):
//!
//! - **The key set** ([`RunInputs::packs`] and [`RunInputs::large`], each a [`PutGrant`]):
//!   `N` pack keys (`cas/b/<lease>/<nonce>-<k>`, slots `0..N`) and `M` large-object keys
//!   (`...-L<i>`, slots `L0..L<M>`), all minted by [`PackKeyMint::mint_set`] for this lease
//!   and this Action at one time. `N` and `M` are ceilings the preparer is configured with
//!   ([`RunPrepConfig::max_packs`], [`LargeTarget::max_objects`]), not predictions: the
//!   executor cannot know the sizes of the outputs before the run, and an unused key is never
//!   written. The executor passes [`RunInputs::keys`] to [`crate::ingest`] when the run ends.
//!   The lease string is the one [`kbf_pack::check_build`] accepts, and the one the executor
//!   passes to `kbf-task --lease`. [`RunInputs::key`] is the key of pack 0, for a caller that
//!   writes one pack.
//! - **The writes** (`put_url` of each grant): one presigned `PUT` of exactly its key to the
//!   tier store's proxy. A pack is signed with the presign-only pack credential and a large
//!   object with the large credential (`tierstore::server::presign_put`); without a large
//!   credential `M` is 0.
//! - **The read grant** ([`ReadGrant`]): the base URL of the job-facing read listener, a
//!   bearer token ([`crate::grants`]) and the second it stops working at, for reading the
//!   Action, its Command and the input tree.
//!
//! **One lifetime.** Every write and the grant are good for the run's wall limit plus a slack
//! (the time a task takes to start and to put its pack), and never for more than
//! [`MAX_EXPIRY`], the longest a presigning key may sign a URL for. A wall that would make
//! them longer is refused ([`ExpiryError`]), at start for the configured wall and per run
//! for a wall the caller gives ([`RunPrep::prepare_run_for`]).
//!
//! **The input closure is collected first.** A grant names an Action; the listener works out
//! what that Action may read from the store when the token is used. So that a run is never
//! started for inputs that cannot be read, [`RunPrep::prepare_run`] collects the closure
//! itself ([`collect_closure`]) and fails if a blob of it is not held or the tree is over the
//! configured budget, and returns the closure it found ([`RunInputs::closure`]) for the
//! executor's accounting.
//!
//! **Secrets.** The key of the mint, the key of the grants and the signing key of the write
//! are values the caller loads from files named by flags (`kbf-server`'s `task_flags`); no
//! type here prints one, and the URL and the token are redacted from [`RunInputs`]'s `Debug`.

mod closure;

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_pack::{MAX_LARGE, MAX_PACKS, MAX_RECORD_BYTES, TaskKeyError, check_build};
use kbf_types::Digest;
use tierstore::server::presign::PresignError;
use tierstore::server::{PresignRequest, presign_put};

pub use self::closure::{ClosureError, ClosureLimits, InputClosure, collect_closure};
pub use tierstore::server::AccessKey;

use crate::backend::Backend;
use crate::grants::{ClaimsError, GrantKey, Grants, Token};
use crate::proxy::{IssuedPackKey, MintError, PackKeyMint};

/// The longest a presigned write and a read grant are good for: 4 hours, the ceiling of the
/// presigning key (`max_expires_secs` of its `--presign-key-file`, and the tier store's
/// `--presign-max-expires`, must be at least this).
pub const MAX_EXPIRY: Duration = kbf_pack::MAX_KEY_LIFE;

/// Why a run wall and a slack cannot give an expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExpiryError {
    /// The wall and the slack are both nothing.
    #[error("the run wall and the slack add up to no time at all")]
    Zero,
    /// The wall and the slack add up to more than [`MAX_EXPIRY`].
    #[error("the run wall and the slack add up to {seconds} s, over the {ceiling} s ceiling")]
    OverCeiling {
        /// The seconds they add up to (rounded up).
        seconds: u64,
        /// The ceiling, in seconds.
        ceiling: u64,
    },
}

/// The seconds a write and a grant are good for when the run may take `wall` and the tasks
/// need `slack` more (a fraction of a second counts as a second).
fn expiry_secs(wall: Duration, slack: Duration) -> Result<u64, ExpiryError> {
    let total = wall.saturating_add(slack);
    let seconds = total
        .as_secs()
        .saturating_add(u64::from(total.subsec_nanos() > 0));
    if seconds == 0 {
        return Err(ExpiryError::Zero);
    }
    let ceiling = MAX_EXPIRY.as_secs();
    if seconds > ceiling {
        return Err(ExpiryError::OverCeiling { seconds, ceiling });
    }
    Ok(seconds)
}

/// Why a [`RunPrep`] cannot be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The URL of the read listener is not `https://host[:port]`.
    #[error("the read URL {url:?} {why}")]
    ReadUrl {
        /// The URL.
        url: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// The endpoint of the tier store is not `http(s)://host[:port]`.
    #[error("the put endpoint {url:?} {why}")]
    PutEndpoint {
        /// The URL.
        url: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// The bucket or the region is empty.
    #[error("the put {0} is empty")]
    Empty(&'static str),
    /// The configured wall and slack give no usable expiry.
    #[error(transparent)]
    Expiry(#[from] ExpiryError),
    /// The number of pack keys a run gets is not 1 to [`MAX_PACKS`].
    #[error("{0} packs per run is not 1 to {MAX_PACKS}")]
    Packs(u8),
    /// The number of large-object keys a run gets is more than [`MAX_LARGE`].
    #[error("{0} large objects per run is more than {MAX_LARGE}")]
    LargeObjects(u8),
    /// The cap of a large object is not above what a pack record may hold, or it would not be
    /// a large object.
    #[error("a large object cap of {0} bytes is not above {MAX_RECORD_BYTES}")]
    LargeCap(u64),
}

/// `(scheme, authority)` of `url` if it is `scheme://host[:port]` and nothing more: no path (not
/// even `/`), query, fragment or user information, a host that is not empty, and a port, when
/// there is one, from 1 to 65535.
fn base_of(url: &str) -> Result<(&str, &str), &'static str> {
    let (scheme, authority) = url
        .split_once("://")
        .ok_or("is not of the form scheme://host[:port]")?;
    if authority.is_empty() {
        return Err("has no host");
    }
    let stray = |c: char| c.is_whitespace() || c.is_control() || "/?#@\\".contains(c);
    if authority.chars().any(stray) {
        return Err("has more than a host and a port");
    }
    // A bracketed IPv6 address holds colons of its own: its port, if any, follows the `]`.
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (inside, after) = rest
                .split_once(']')
                .ok_or("has more than a host and a port")?;
            let port = match after {
                "" => None,
                _ => Some(
                    after
                        .strip_prefix(':')
                        .ok_or("has more than a host and a port")?,
                ),
            };
            (inside, port)
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if host.is_empty() {
        return Err("has no host");
    }
    let valid = |port: &str| {
        port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|n| n != 0)
    };
    if port.is_some_and(|port| !valid(port)) {
        return Err("has a port that is not a number from 1 to 65535");
    }
    Ok((scheme, authority))
}

/// Checks that `url` is the base URL of the job-facing read listener: `https://host[:port]`,
/// with no path prefix.
///
/// # Errors
/// [`ConfigError::ReadUrl`] for a URL that is not, `http://` ones included: a grant's token is
/// a bearer secret and crosses only a TLS connection unless the operator said otherwise
/// ([`check_read_url_allowing`]).
pub fn check_read_url(url: &str) -> Result<(), ConfigError> {
    check_read_url_allowing(url, false)
}

/// As [`check_read_url`], and `http://host[:port]` too when `allow_plain` is set.
///
/// # Errors
/// [`ConfigError::ReadUrl`] for a URL that is neither `https://host[:port]` nor, with
/// `allow_plain`, `http://host[:port]`.
pub fn check_read_url_allowing(url: &str, allow_plain: bool) -> Result<(), ConfigError> {
    let refuse = |why| ConfigError::ReadUrl {
        url: url.to_owned(),
        why,
    };
    match base_of(url) {
        Ok(("https", _)) => Ok(()),
        Ok(("http", _)) if allow_plain => Ok(()),
        Ok(_) => Err(refuse("is not https")),
        Err(why) => Err(refuse(why)),
    }
}

/// Where a task writes its pack, and the credential that signs the write.
#[derive(Clone, Debug)]
pub struct PutTarget {
    /// The tier store's proxy, `http(s)://host[:port]`: its authority is the host the URL is
    /// signed for.
    pub endpoint: String,
    /// The region of the credential scope.
    pub region: String,
    /// The bucket.
    pub bucket: String,
    /// The presign-only credential (a key of the tier store's `--presign-key-file`). It can
    /// sign a `PUT` under its prefix and do nothing else.
    pub key: AccessKey,
}

/// Where a task writes its large objects, and the credential that signs those writes.
///
/// A large object is a raw output blob over [`MAX_RECORD_BYTES`], which a pack cannot hold. It
/// is signed with a credential of its own (a key of the tier store whose prefix allows objects
/// up to [`LargeTarget::max_bytes`]); the endpoint, the region and the bucket are the pack
/// target's.
#[derive(Clone, Debug)]
pub struct LargeTarget {
    /// The presign-only credential for large objects.
    pub key: AccessKey,
    /// The large-object keys each run gets: 0 to [`MAX_LARGE`].
    pub max_objects: u8,
    /// The most bytes of one large object, above [`MAX_RECORD_BYTES`]: the cap the task and
    /// [`crate::ingest`] hold an object to.
    pub max_bytes: u64,
}

/// What a [`RunPrep`] is built from.
#[derive(Debug)]
pub struct RunPrepConfig {
    /// Mints the pack keys. Its secret is the one the executor's [`crate::ingest`] verifies
    /// with.
    pub mint: PackKeyMint,
    /// The key the read grants are made under: the one the job-facing listener verifies with.
    pub grant_key: GrantKey,
    /// Where a task writes its packs.
    pub put: PutTarget,
    /// The pack keys each run gets, 1 to [`MAX_PACKS`].
    pub max_packs: u8,
    /// Where a task writes its large objects; `None` gives every run no large key, and a run
    /// the size of one pack set.
    pub large: Option<LargeTarget>,
    /// The base URL of the job-facing read listener, `https://host[:port]` (`http://` only with
    /// `allow_plain_read_url`).
    pub read_url: String,
    /// Whether `read_url` may be `http://host[:port]`: a farm on its own network that serves
    /// the read listener without TLS. Off, only `https://` is accepted.
    pub allow_plain_read_url: bool,
    /// The run wall limit every run is prepared for by [`RunPrep::prepare_run`].
    pub run_wall: Duration,
    /// The time past the wall a write and a grant stay good.
    pub slack: Duration,
    /// How large an input closure may be.
    pub limits: ClosureLimits,
}

/// Why a run could not be prepared. Nothing was written.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    /// The lease is not a build id [`kbf_pack::check_build`] accepts.
    #[error("the lease: {0}")]
    Lease(#[from] TaskKeyError),
    /// The wall and the slack give no usable expiry.
    #[error(transparent)]
    Expiry(#[from] ExpiryError),
    /// The time is before the Unix epoch.
    #[error("the time is before the Unix epoch")]
    Clock,
    /// The inputs cannot be read or are over budget.
    #[error(transparent)]
    Closure(#[from] ClosureError),
    /// The key could not be minted.
    #[error(transparent)]
    Mint(#[from] MintError),
    /// The write could not be presigned.
    #[error(transparent)]
    Presign(#[from] PresignError),
    /// The grant could not be issued: the lease is not a build id a grant carries (at most 64
    /// characters).
    #[error(transparent)]
    Grant(#[from] ClaimsError),
}

/// Whose a refused preparation is to fix, for the caller that settles the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareFault {
    /// The store could not be read: the same run may be prepared later, so it is the farm's
    /// to retry.
    Unavailable,
    /// The run cannot be prepared as asked (an input missing, malformed or over budget, a
    /// lease or a time no key can carry, a configuration that does not sign): asking again
    /// gives the same answer, so it is the caller's to fix.
    Refused,
}

impl PrepareError {
    /// Whose this refusal is to fix. Only a store that could not be read is
    /// [`PrepareFault::Unavailable`]; minting, presigning and the grant are local work and
    /// fail the same way every time.
    #[must_use]
    pub fn fault(&self) -> PrepareFault {
        match self {
            Self::Closure(ClosureError::Unavailable { .. }) => PrepareFault::Unavailable,
            _ => PrepareFault::Refused,
        }
    }
}

/// The read grant of a run.
#[derive(Clone, Debug)]
pub struct ReadGrant {
    /// The base URL of the job-facing read listener (`https://host[:port]`); a blob is
    /// `GET <url>/<hex>/<size>`.
    pub url: String,
    /// The bearer token. A secret: never printed.
    pub token: Token,
    /// The Unix second the grant stops working at.
    pub not_after: u64,
}

/// One object a task may write: its key and the presigned `PUT` of exactly that key.
#[derive(Clone)]
pub struct PutGrant {
    /// The key, to pass to [`crate::ingest`] when the run ends.
    pub key: IssuedPackKey,
    /// A presigned `PUT` of exactly [`PutGrant::key`]. Its query carries a signature: a secret
    /// until it expires.
    pub put_url: String,
}

impl fmt::Debug for PutGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PutGrant")
            .field("key", &self.key)
            .field("put_url", &"<redacted>")
            .finish()
    }
}

/// What the executor delivers to the task of one run.
///
/// The run's key set is [`RunInputs::packs`] (slots `0..N`) then [`RunInputs::large`] (slots
/// `L0..L<M>`), each with its write; [`RunInputs::keys`] lists the keys in that order for
/// [`crate::ingest`]. `key` and `put_url` are `packs[0]`, for a caller that writes one pack.
pub struct RunInputs {
    /// The key of pack 0, which holds the manifest: `packs[0].key`.
    pub key: IssuedPackKey,
    /// The write of pack 0: `packs[0].put_url`.
    pub put_url: String,
    /// The packs the task may write, in slot order: 1 to [`MAX_PACKS`] of them, never empty.
    pub packs: Vec<PutGrant>,
    /// The large objects the task may write, in slot order `L0..`: none unless the preparer
    /// has a large credential.
    pub large: Vec<PutGrant>,
    /// How the task reads the Action, its Command and its inputs.
    pub read_grant: ReadGrant,
    /// What the task may read.
    pub closure: InputClosure,
}

impl RunInputs {
    /// Every key of the run, packs then large objects, each in slot order: the set
    /// [`crate::ingest`] verifies.
    #[must_use]
    pub fn keys(&self) -> Vec<IssuedPackKey> {
        let grants = self.packs.iter().chain(&self.large);
        grants.map(|grant| grant.key.clone()).collect()
    }
}

impl fmt::Debug for RunInputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunInputs")
            .field("key", &self.key)
            .field("put_url", &"<redacted>")
            .field("packs", &self.packs)
            .field("large", &self.large)
            .field("read_grant", &self.read_grant)
            .field("inputs", &self.closure.input_count())
            .finish_non_exhaustive()
    }
}

/// Prepares runs: the minter, the signer, the grant key and the listener's URL, built once at
/// start.
#[derive(Debug)]
pub struct RunPrep {
    backend: Backend,
    mint: PackKeyMint,
    grants: Grants,
    put: PutTarget,
    max_packs: u8,
    large: Option<LargeTarget>,
    read_url: String,
    run_wall: Duration,
    slack: Duration,
    limits: ClosureLimits,
}

impl RunPrep {
    /// The most bytes of one large object, if runs get large keys: what a task and
    /// [`crate::ingest`] hold an object to.
    #[must_use]
    pub fn large_max_bytes(&self) -> Option<u64> {
        self.large.as_ref().map(|large| large.max_bytes)
    }

    /// The mint that signs this preparer's keys: the one [`crate::ingest`] must verify a
    /// run's pack key with. It can mint as well as verify, so only the frontend's own code
    /// holds it (the executor beside it); nothing a task or a client reaches is given it.
    #[must_use]
    pub fn mint(&self) -> &PackKeyMint {
        &self.mint
    }

    /// A preparer that reads the store through `backend` and is set up by `config`.
    ///
    /// # Errors
    /// [`ConfigError`]: the read URL is not `https://host[:port]`, the put endpoint is not
    /// `http(s)://host[:port]`, the bucket or region is empty, or the wall and slack give no
    /// usable expiry.
    pub fn new(backend: Backend, config: RunPrepConfig) -> Result<Self, ConfigError> {
        let RunPrepConfig {
            mint,
            grant_key,
            put,
            max_packs,
            large,
            read_url,
            allow_plain_read_url,
            run_wall,
            slack,
            limits,
        } = config;
        check_read_url_allowing(&read_url, allow_plain_read_url)?;
        match base_of(&put.endpoint) {
            Ok(("http" | "https", _)) => {}
            Ok(_) => return Err(put_endpoint(&put, "is not http or https")),
            Err(why) => return Err(put_endpoint(&put, why)),
        }
        if put.bucket.is_empty() {
            return Err(ConfigError::Empty("bucket"));
        }
        if put.region.is_empty() {
            return Err(ConfigError::Empty("region"));
        }
        expiry_secs(run_wall, slack)?;
        if !(1..=MAX_PACKS).contains(&max_packs) {
            return Err(ConfigError::Packs(max_packs));
        }
        if let Some(large) = &large {
            if large.max_objects > MAX_LARGE {
                return Err(ConfigError::LargeObjects(large.max_objects));
            }
            if large.max_bytes <= MAX_RECORD_BYTES {
                return Err(ConfigError::LargeCap(large.max_bytes));
            }
        }
        Ok(Self {
            backend,
            mint,
            grants: Grants::system(grant_key),
            put,
            max_packs,
            large,
            read_url,
            run_wall,
            slack,
            limits,
        })
    }

    /// The input closure of `action`, as [`RunPrep::prepare_run`] collects it.
    ///
    /// # Errors
    /// [`ClosureError`].
    pub async fn closure(&self, action: &Digest) -> Result<InputClosure, ClosureError> {
        collect_closure(&self.backend, action, self.limits).await
    }

    /// The inputs of the run of `action` on the lease `lease`, started at `now`, which may
    /// take the configured wall.
    ///
    /// # Errors
    /// [`PrepareError`], as [`RunPrep::prepare_run_for`].
    pub async fn prepare_run(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
    ) -> Result<RunInputs, PrepareError> {
        self.prepare_run_for(lease, action, now, self.run_wall)
            .await
    }

    /// [`RunPrep::prepare_run`] for a run that may take `wall`: the write and the grant are
    /// good from `now` for `wall` plus the slack.
    ///
    /// # Errors
    /// [`PrepareError`]: the lease is not a build id, the wall and slack are over
    /// [`MAX_EXPIRY`], `now` is before the epoch, the inputs are missing or over budget, or the
    /// key, the write or the grant cannot be made.
    pub async fn prepare_run_for(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<RunInputs, PrepareError> {
        check_build(lease)?;
        let expires_secs = expiry_secs(wall, self.slack)?;
        let since = now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PrepareError::Clock)?;
        let closure = self.closure(action).await?;
        // One key per object the task may write: packs `0..N`, then large objects `L0..L<M>`.
        let n_packs = self.max_packs;
        let n_large = self.large.as_ref().map_or(0, |large| large.max_objects);
        let keys = self.mint.mint_set(lease, action, now, n_packs, n_large)?;
        let now_ms = u64::try_from(since.as_millis()).unwrap_or(u64::MAX);
        let mut grants = Vec::with_capacity(keys.len());
        for (at, key) in keys.into_iter().enumerate() {
            let signer = match self.large.as_ref() {
                Some(large) if at >= usize::from(n_packs) => &large.key,
                _ => &self.put.key,
            };
            let put_url = presign_put(
                signer,
                &PresignRequest {
                    endpoint: &self.put.endpoint,
                    region: &self.put.region,
                    bucket: &self.put.bucket,
                    key: key.key(),
                    expires_secs,
                    now: now_ms,
                },
            )?;
            grants.push(PutGrant { key, put_url });
        }
        let large = grants.split_off(usize::from(n_packs));
        let not_after = since.as_secs().saturating_add(expires_secs);
        let token = self.grants.issue(lease, *action, not_after)?;
        let first = grants.first().cloned().ok_or(MintError::SetSize {
            packs: n_packs,
            large: n_large,
        })?;
        Ok(RunInputs {
            key: first.key,
            put_url: first.put_url,
            packs: grants,
            large,
            read_grant: ReadGrant {
                url: self.read_url.clone(),
                token,
                not_after,
            },
            closure,
        })
    }
}

fn put_endpoint(put: &PutTarget, why: &'static str) -> ConfigError {
    ConfigError::PutEndpoint {
        url: put.endpoint.clone(),
        why,
    }
}
