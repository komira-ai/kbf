//! [`Backend`]: what Execute and the scheduler need of the store, and nothing more. Three
//! implementations: a [`Cache`] in this process (what `kbf-server` and `kbf all-in-one`
//! have always run), a [`Storage`] reached over gRPC (`kbf frontend` without members,
//! which holds no blobs of its own: `docs/design/architecture-v2.md`, sections 2 and 7),
//! and the storage nodes of a map ([`Backend::routed`], `kbf frontend` with members),
//! which is the second until the map has two members.
//!
//! The calls are the ones the scheduler side makes: the action-cache lookup before a
//! run, the Action and Command blobs, the check that every input is held, and the two
//! halves of the action-cache write after one. Client CAS, ByteStream and action-cache
//! calls are not here: they are the cache's services, or the proxy's.
//!
//! **The input check over a [`Storage`]** ([`walk`], in `input_walk`) reads each Directory of the tree
//! from the node and asks it which files are missing. Over one node it remembers nothing
//! between Executes. Over a map the same walk reads each Directory from its owners, and
//! remembers the subtrees it found held for a short time, under the map's epoch, so the
//! actions of one build, which share most of their inputs, read a shared subtree once
//! (`input_walk` says what bounds the memory and how a loss invalidates it).
//!
//! **The action-cache write over a map.** A node checks an entry against the blobs it
//! holds itself, and a result's blobs are on the owners of their own digests. The first
//! half ([`Backend::prepare_action_result`]) therefore asks the farm: the closure is
//! worked out (the trees read from their owners) and every blob in it must be held
//! somewhere, and the result is stored where the map puts it. The second half copies the
//! entry's blobs to the action's owner before it commits there
//! (`kbf_route::Router::commit_with_closure`), until av2-12 gives an entry the
//! acknowledgements of its writers.
//!
//! **Packs.** A frontend given [`Packs`] ([`Backend::remote_over`], [`Backend::routed_over`])
//! has the blobs clients upload in packs, not on its nodes, so the Action and Command blobs, the
//! input check and the walk of the input tree look in the pack index first and ask the nodes
//! about what it lacks (`layered`). The action cache and the result blobs are the nodes', but
//! the closure check is not: a blob a daemon wrote into a pack is not on a node, so the check
//! of an action-cache write and of every hit asks the pack index first and the nodes for the
//! rest (`closed`), and the entry handed to the nodes names only the blobs they hold.
//! Without packs the nodes check the entry alone, as they always did.

mod closed;
mod layered;

use std::fmt;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_proto::reapi::ActionResult;
use kbf_route::Router;
use kbf_storage::{OpenPieces, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;

pub(crate) use self::closed::{checked, on_nodes, served};
pub(crate) use self::layered::{Layered, Primary};
use crate::cache::Cache;
use crate::closure;
use crate::input_tree::Walked;
use crate::input_walk::{Blobs, Trees, walk};
use crate::meta_log::{Leadership, MetaLog};
use crate::metrics::CacheMetrics;
use crate::proxy::Packs;
use crate::routed::Routed;

/// The store behind Execute and the scheduler. Cheap to clone.
#[derive(Clone)]
pub struct Backend(Arc<dyn Plane>);

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Backend(..)")
    }
}

/// What a store does for [`Backend`]: object safe, so Execute and the farm are not
/// generic over the metadata log and object store of a cache they may not have.
trait Plane: Send + Sync {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>>;
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>>;
    fn read_stream<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<ReadStream>>;
    /// Each blob of `digests` read as [`Plane::read_blob`] reads one; a plane with packs
    /// reads the packed ones together.
    fn read_many<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Vec<Answer<bytes::Bytes>>> {
        Box::pin(futures::future::join_all(
            digests.iter().map(|digest| self.read_blob(digest)),
        ))
    }
    /// The packs a plane keeps its client uploads in, if it has any.
    fn packs(&self) -> Option<&Packs>;
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>>;
    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>>;
    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>>;
    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>>;
    fn put_streamed<'a>(
        &'a self,
        digest: &'a Digest,
        open: &'a OpenPieces<'a>,
    ) -> BoxFuture<'a, Answer<()>>;
    fn metrics(&self) -> &CacheMetrics;
    fn render_metrics(&self) -> String;
    fn leadership(&self) -> Option<Leadership>;
}

type Answer<T> = Result<T, StorageError>;

impl Backend {
    /// A store of a storage node (or any [`Storage`]) reached through `storage`, with
    /// metrics of its own: the frontend holds no cache to keep them.
    #[must_use]
    pub fn remote(storage: Arc<dyn Storage>) -> Self {
        Self::remote_over(storage, None)
    }

    /// [`Backend::remote`], with the blobs a frontend keeps in `packs` looked for first (a
    /// frontend without packs passes `None`, and the store is the one `remote` makes).
    #[must_use]
    pub fn remote_over(storage: Arc<dyn Storage>, packs: Option<Packs>) -> Self {
        Self(Arc::new(Remote::new(storage, packs)))
    }

    /// A store of the storage nodes the map `routed` names, once it has two members or
    /// more ([`Routed::active`]); until then, the node `storage` reaches, as
    /// [`Backend::remote`] does.
    ///
    /// The action-cache write is the router's ([`Router::commit_with_closure`]): the
    /// result and the closure are checked against the owners of every blob in them, the
    /// result blob is stored where the map puts it, and the entry is committed at the
    /// action's owner with its blobs copied there first.
    #[must_use]
    pub fn routed(routed: Arc<Routed>, storage: Arc<dyn Storage>) -> Self {
        Self::routed_over(routed, storage, None)
    }

    /// [`Backend::routed`], with the blobs a frontend keeps in `packs` looked for first (a
    /// frontend without packs passes `None`, and the store is the one `routed` makes).
    #[must_use]
    pub fn routed_over(
        routed: Arc<Routed>,
        storage: Arc<dyn Storage>,
        packs: Option<Packs>,
    ) -> Self {
        Self(Arc::new(RoutedPlane {
            routed,
            bootstrap: Remote::new(storage, packs.clone()),
            trees: Trees::default(),
            packs,
        }))
    }

    /// The digests among `digests` the store does not hold ([`Storage::find_missing`]).
    ///
    /// # Errors
    /// The store could not answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        self.0.find_missing(digests).await
    }

    /// The blob `digest`, read whole and verified against its digest.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if it is not held, or as the store answers.
    pub async fn read_blob(&self, digest: &Digest) -> Result<bytes::Bytes, StorageError> {
        self.0.read_blob(digest).await
    }

    /// The blob `digest` as a stream of chunks, read in windows and never held whole: for a
    /// blob too large for [`Backend::read_blob`]. The store checks the blob as a whole
    /// against its digest ([`Storage::read_range`]); a router does not, so the caller that
    /// holds the digest hashes what it receives.
    ///
    /// The packs are not looked in: what a frontend packs is at most
    /// [`crate::MAX_PACKED_BLOB_BYTES`], which is what a caller reads whole.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if it is not held, or as the store answers; a failure after
    /// the first chunk is an error item of the stream.
    pub async fn read_stream(&self, digest: &Digest) -> Result<ReadStream, StorageError> {
        self.0.read_stream(digest).await
    }

    /// The blobs `digests`, one answer each in the order asked, each as
    /// [`Backend::read_blob`] answers one. With packs, the blobs in them are read together
    /// (the index is asked once, and blobs that lie close in a pack are one ranged read of the
    /// store) and only the rest are read from the storage nodes, one by one.
    pub async fn read_many(&self, digests: &[Digest]) -> Vec<Result<bytes::Bytes, StorageError>> {
        self.0.read_many(digests).await
    }

    /// Where the blob of each of `digests` lies in a pack, in the order asked, `None` for one that
    /// is in none; all `None` for a store without packs. One batched ask of the pack index.
    ///
    /// # Errors
    /// As the pack index answers.
    pub async fn locate_packed(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Option<kbf_pack_index::Location>>, StorageError> {
        let Some(packs) = self.0.packs() else {
            return Ok(vec![None; digests.len()]);
        };
        packs.locate(digests).await.map_err(layered::unavailable)
    }

    /// The length of the object `key` in the object store the packs live in (a HEAD), `None` if
    /// there is none or the store has no packs.
    ///
    /// # Errors
    /// As the object store answers.
    pub async fn object_len(&self, key: &str) -> Result<Option<u64>, StorageError> {
        let Some(packs) = self.0.packs() else {
            return Ok(None);
        };
        packs.object_len(key).await.map_err(layered::unavailable)
    }

    pub(crate) async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<(Digest, ActionResult)>, StorageError> {
        self.0.action_result(action).await
    }

    /// The first half of an action-cache write ([`Storage::prepare_action_result`]).
    ///
    /// # Errors
    /// As the store answers.
    pub async fn prepare_action_result(
        &self,
        result: &ActionResult,
    ) -> Result<ActionRecord, StorageError> {
        self.0.prepare_action_result(result).await
    }

    /// The second half, as a daemon's result ([`Storage::commit_action_record`]).
    ///
    /// # Errors
    /// As the store answers.
    pub async fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.0.commit_action_record(action, record).await
    }

    pub(crate) async fn walk_inputs(&self, root: Digest) -> Result<Walked, StorageError> {
        self.0.walk_inputs(root).await
    }

    /// Stores the blob `digest` from the pieces `open` gives, a piece at a time and never held
    /// whole, on the node (or the first owner of the blob, when the farm has a map) that
    /// stores it; the node checks that the bytes hash to `digest` and stores nothing if they do
    /// not. The blob is a large one: a blob that fits in memory is [`Storage::put`]'s.
    ///
    /// `open` is called for each attempt and must give the blob from its first byte: a write
    /// that fails over to another owner sends it again.
    ///
    /// # Errors
    /// [`StorageError::DigestMismatch`] or [`StorageError::Invalid`] for bytes that are not
    /// `digest`'s, an error of `open`'s pieces, or as the store answers.
    pub async fn put_streamed(
        &self,
        digest: &Digest,
        open: &OpenPieces<'_>,
    ) -> Result<(), StorageError> {
        self.0.put_streamed(digest, open).await
    }

    /// What the service counts: the cache's own metrics, or this frontend's.
    #[must_use]
    pub fn metrics(&self) -> &CacheMetrics {
        self.0.metrics()
    }

    /// [`Backend::metrics`] in the Prometheus text format (the cache adds its store's).
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.0.render_metrics()
    }

    /// The id and term of a durable metadata log the store has ([`MetaLog::leadership`]);
    /// none for a store without one.
    #[must_use]
    pub fn leadership(&self) -> Option<Leadership> {
        self.0.leadership()
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> From<Arc<Cache<M, O>>> for Backend {
    fn from(cache: Arc<Cache<M, O>>) -> Self {
        Self(Arc::new(Local(cache)))
    }
}

/// A cache in this process.
struct Local<M, O>(Arc<Cache<M, O>>);

impl<M: MetaLog, O: ObjectStore + 'static> Plane for Local<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            self.0
                .find_missing(digests)
                .await
                .map_err(StorageError::from)
        })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        Box::pin(async move { self.0.read_blob(digest).await.map_err(StorageError::from) })
    }

    fn read_stream<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<ReadStream>> {
        Box::pin(async move {
            let read = self.0.read_windows(digest, 0, digest.size_bytes).await?;
            Ok(crate::storage::chunks(read))
        })
    }

    fn packs(&self) -> Option<&Packs> {
        None
    }
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        Box::pin(async move {
            self.0
                .action_result(action)
                .await
                .map_err(StorageError::from)
        })
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        Box::pin(async move {
            self.0
                .prepare_action_result(result)
                .await
                .map_err(StorageError::from)
        })
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        Box::pin(async move {
            self.0
                .commit_action_record(Role::Daemon, action, record)
                .await
                .map_err(StorageError::from)
        })
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        Box::pin(async move { self.0.walk_inputs(root).await.map_err(StorageError::from) })
    }

    fn put_streamed<'a>(
        &'a self,
        digest: &'a Digest,
        open: &'a OpenPieces<'a>,
    ) -> BoxFuture<'a, Answer<()>> {
        Box::pin(async move {
            let body = open()
                .map(|piece| piece.map_err(|e| ObjectStoreError::Body(e.to_string())))
                .boxed();
            let claim = Box::pin(futures::future::ready(Some(*digest)));
            let stored = self
                .0
                .store_streamed_claimed(digest.size_bytes, body, claim)
                .await;
            stored_as(stored.map_err(StorageError::from), digest)
        })
    }

    fn metrics(&self) -> &CacheMetrics {
        self.0.metrics()
    }

    fn render_metrics(&self) -> String {
        self.0.render_metrics()
    }

    fn leadership(&self) -> Option<Leadership> {
        self.0.meta().leadership()
    }
}

/// A [`Storage`], and the metrics of the service that calls it.
struct Remote {
    storage: Arc<dyn Storage>,
    metrics: CacheMetrics,
    /// Of one node, whose [`Held`] gives no epoch, only the content cache of decoded
    /// Directories is used (every Directory it holds is still asked about); nothing is
    /// remembered as verified.
    trees: Trees,
    /// The blobs the frontend keeps in packs, looked for before the node.
    packs: Option<Packs>,
}

impl Remote {
    fn new(storage: Arc<dyn Storage>, packs: Option<Packs>) -> Self {
        Self {
            storage,
            metrics: CacheMetrics::default(),
            trees: Trees::default(),
            packs,
        }
    }
}

impl Plane for Remote {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        match &self.packs {
            None => self.storage.find_missing(digests),
            Some(packs) => Box::pin(async move {
                let held = Held(&*self.storage);
                let layered = Layered { packs, rest: &held };
                layered.find_missing(digests).await
            }),
        }
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        match &self.packs {
            None => self.storage.read_blob(digest),
            Some(packs) => Box::pin(async move {
                let held = Held(&*self.storage);
                let layered = Layered { packs, rest: &held };
                layered.read_blob(digest).await
            }),
        }
    }

    fn read_stream<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<ReadStream>> {
        self.storage.read_range(digest, 0, 0)
    }

    fn read_many<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Vec<Answer<bytes::Bytes>>> {
        match &self.packs {
            None => Box::pin(futures::future::join_all(
                digests.iter().map(|digest| self.storage.read_blob(digest)),
            )),
            Some(packs) => Box::pin(async move {
                let held = Held(&*self.storage);
                Layered { packs, rest: &held }.read_many(digests).await
            }),
        }
    }

    fn packs(&self) -> Option<&Packs> {
        self.packs.as_ref()
    }
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        let storage = &self.storage;
        match &self.packs {
            None => storage.action_result(action),
            Some(packs) => Box::pin(async move {
                let hit = storage.action_result(action).await?;
                let held = Held(&**storage);
                let layered = Layered { packs, rest: &held };
                served(hit, &layered).await
            }),
        }
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        let storage = &self.storage;
        match &self.packs {
            None => storage.prepare_action_result(result),
            Some(packs) => Box::pin(async move {
                let held = Held(&**storage);
                let layered = Layered { packs, rest: &held };
                let closure = checked(result, &layered).await?;
                let encoded = bytes::Bytes::from(result.encode_to_vec());
                let stored = storage.put(encoded).await?;
                Ok(ActionRecord {
                    result: stored,
                    closure,
                })
            }),
        }
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        let storage = &self.storage;
        match &self.packs {
            None => storage.commit_action_record(Role::Daemon, action, record),
            Some(packs) => Box::pin(async move {
                let record = on_nodes(packs, record).await?;
                let committed = storage.commit_action_record(Role::Daemon, action, record);
                committed.await
            }),
        }
    }

    fn put_streamed<'a>(
        &'a self,
        digest: &'a Digest,
        open: &'a OpenPieces<'a>,
    ) -> BoxFuture<'a, Answer<()>> {
        Box::pin(async move {
            let claim = Box::pin(futures::future::ready(Some(*digest)));
            let stored = self
                .storage
                .put_streamed(digest.size_bytes, open(), claim)
                .await;
            stored_as(stored, digest)
        })
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        Box::pin(async move {
            let held = Held(&*self.storage);
            match &self.packs {
                None => walk(&held, root, &self.trees).await,
                Some(packs) => walk(&Layered { packs, rest: &held }, root, &self.trees).await,
            }
        })
    }

    fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    fn render_metrics(&self) -> String {
        let mut out = self.metrics.render();
        // The process's, as `Cache::render_metrics` adds it: this frontend buffers
        // bodies for its requests (batch blobs, Segments uploads, whole-blob reads) too.
        kbf_storage::in_flight::render(&mut out);
        out
    }

    fn leadership(&self) -> Option<Leadership> {
        None
    }
}

/// The storage nodes of a map, or the one node `--storage` names until the map has two
/// members: [`Remote`] when [`Routed::active`] is false.
struct RoutedPlane {
    routed: Arc<Routed>,
    bootstrap: Remote,
    /// The Directories verified held on the farm, shared by every Execute.
    trees: Trees,
    /// The blobs the frontend keeps in packs, looked for before the owners.
    packs: Option<Packs>,
}

impl RoutedPlane {
    /// The router, when the map is to be routed by.
    fn routing(&self) -> Option<&Router> {
        if self.routed.active() {
            return Some(self.routed.router());
        }
        self.routed.tag_bootstrap();
        None
    }
}

impl Plane for RoutedPlane {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(router.find_missing(digests)),
            (Some(router), Some(packs)) => Box::pin(async move {
                let primary = Primary(router);
                let layered = Layered {
                    packs,
                    rest: &primary,
                };
                layered.find_missing(digests).await
            }),
            (None, _) => self.bootstrap.find_missing(digests),
        }
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(router.read_blob(digest)),
            (Some(router), Some(packs)) => Box::pin(async move {
                let primary = Primary(router);
                let layered = Layered {
                    packs,
                    rest: &primary,
                };
                layered.read_blob(digest).await
            }),
            (None, _) => self.bootstrap.read_blob(digest),
        }
    }

    fn read_stream<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<ReadStream>> {
        match self.routing() {
            Some(router) => Box::pin(router.read_range(digest, 0, 0)),
            None => self.bootstrap.read_stream(digest),
        }
    }

    fn read_many<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Vec<Answer<bytes::Bytes>>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(futures::future::join_all(
                digests.iter().map(|digest| router.read_blob(digest)),
            )),
            (Some(router), Some(packs)) => Box::pin(async move {
                let primary = Primary(router);
                let layered = Layered {
                    packs,
                    rest: &primary,
                };
                layered.read_many(digests).await
            }),
            (None, _) => self.bootstrap.read_many(digests),
        }
    }

    fn packs(&self) -> Option<&Packs> {
        self.packs.as_ref()
    }
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(router.action_result(action)),
            (Some(router), Some(packs)) => Box::pin(async move {
                let hit = router.action_result(action).await?;
                let layered = Layered {
                    packs,
                    rest: router,
                };
                served(hit, &layered).await
            }),
            (None, _) => self.bootstrap.action_result(action),
        }
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(prepare(router, result)),
            (Some(router), Some(packs)) => Box::pin(async move {
                let layered = Layered {
                    packs,
                    rest: router,
                };
                let closure = checked(result, &layered).await?;
                let encoded = bytes::Bytes::from(result.encode_to_vec());
                let stored = router.put(encoded).await?;
                Ok(ActionRecord {
                    result: stored,
                    closure,
                })
            }),
            (None, _) => self.bootstrap.prepare_action_result(result),
        }
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => {
                Box::pin(router.commit_with_closure(Role::Daemon, action, record))
            }
            (Some(router), Some(packs)) => Box::pin(async move {
                let record = on_nodes(packs, record).await?;
                let committed = router.commit_with_closure(Role::Daemon, action, record);
                committed.await
            }),
            (None, _) => self.bootstrap.commit_action_record(action, record),
        }
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        match (self.routing(), &self.packs) {
            (Some(router), None) => Box::pin(async move { walk(router, root, &self.trees).await }),
            (Some(router), Some(packs)) => Box::pin(async move {
                let layered = Layered {
                    packs,
                    rest: router,
                };
                walk(&layered, root, &self.trees).await
            }),
            (None, _) => self.bootstrap.walk_inputs(root),
        }
    }

    fn put_streamed<'a>(
        &'a self,
        digest: &'a Digest,
        open: &'a OpenPieces<'a>,
    ) -> BoxFuture<'a, Answer<()>> {
        match self.routing() {
            Some(router) => Box::pin(router.put_streamed(digest, open)),
            None => self.bootstrap.put_streamed(digest, open),
        }
    }

    fn metrics(&self) -> &CacheMetrics {
        self.bootstrap.metrics()
    }

    fn render_metrics(&self) -> String {
        self.bootstrap.render_metrics()
    }

    fn leadership(&self) -> Option<Leadership> {
        None
    }
}

/// A streamed put's answer as the write's: the blob is stored when the store says it stored
/// `digest`; anything else a store answers (a withdrawn claim, another digest) is not.
fn stored_as(answer: Answer<Option<Digest>>, digest: &Digest) -> Answer<()> {
    match answer? {
        Some(stored) if stored == *digest => Ok(()),
        other => Err(StorageError::Internal(format!(
            "a store answered {other:?} for the streamed blob {digest}"
        ))),
    }
}

/// The first half of an action-cache write over the owners: works out the closure of
/// `result` (reading its output trees from wherever the map has them), asks which of its
/// blobs the farm does not hold, and stores `result` as a blob where the map puts it. As
/// [`Cache::prepare_action_result`] does for one node's blobs.
///
/// # Errors
/// [`ActionWriteError::Absent`] naming every blob of the closure no owner holds; a
/// missing tree, a malformed result, or owners that could not answer.
async fn prepare(router: &Router, result: &ActionResult) -> Answer<ActionRecord> {
    let (mut closure, trees) = closure::direct(result)?;
    for tree in trees {
        closure::add_tree(&mut closure, &tree, router.read_blob(&tree).await?)?;
    }
    let needed: Vec<Digest> = closure.iter().copied().collect();
    let missing = router.find_missing(&needed).await?;
    let absent = held_by_none(router, &missing).await?;
    if !absent.is_empty() {
        return Err(ActionWriteError::Absent(absent).into());
    }
    let stored = router
        .put(bytes::Bytes::from(result.encode_to_vec()))
        .await?;
    Ok(ActionRecord {
        result: stored,
        closure,
    })
}

/// Those of `missing`, which the digests' primaries lack, that no owner holds. A
/// map change moves a blob's primary before the blob is copied there, so a primary that
/// lacks it is not the farm lacking it: the read order, which ends with the previous
/// generation's owners, is asked for the first byte. (Unlike a missing input, which the
/// client uploads again, a missing output would fail the action.)
///
/// # Errors
/// An owner of a digest could not say, and none had it.
async fn held_by_none(router: &Router, missing: &[Digest]) -> Answer<Vec<Digest>> {
    let mut absent = Vec::new();
    for digest in missing {
        match router.read_range(digest, 0, 1).await {
            Ok(_) => {}
            Err(StorageError::NotFound(_)) => absent.push(*digest),
            Err(e) => return Err(e),
        }
    }
    Ok(absent)
}

/// One storage node's blobs.
struct Held<'a>(&'a dyn Storage);

impl Blobs for Held<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        self.0.read_blob(digest)
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        self.0.find_missing(digests)
    }
}

/// The blobs of a farm, routed by its map. A file is missing when no owner a read would
/// try holds it ([`Router::find_missing_anywhere`]), not when its primary lacks it: a
/// primary that has just joined holds nothing of what it now owns, and refusing the run
/// for that would refuse every action whose inputs the client no longer uploads.
impl Blobs for Router {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        Box::pin(Router::read_blob(self, digest))
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(Router::find_missing_anywhere(self, digests))
    }

    fn epoch(&self) -> Option<u64> {
        Router::epoch(self)
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
