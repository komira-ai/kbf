//! What a frontend with no `--storage` is made of beyond the flags' own rules: what it needs
//! instead of a node ([`without_storage`]), the blobs it keeps ([`stored_of`]) and what its
//! readiness asks ([`stored_checks`]). `frontend.rs` has the whole picture.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Packs, Stored};
use kbf_pack_index::action::ActionStore;
use kbf_storage::service::RemoteStorage;
use tonic::transport::Channel;

use super::{Args, ExecutorFlags, PackFlags};
use crate::health::{Checks, Readiness, StateProbe};

/// What a frontend with no `--storage` needs instead: the blobs it keeps itself and the one
/// executor that does not need a node. The daemons' blob traffic (reads, uploads, the segments)
/// is a storage node's, so `--executor kbf-daemon` is refused by name; the packs and the pack
/// index are where the blobs and the action cache are.
pub(super) fn without_storage(
    server: &Args,
    executor: &ExecutorFlags,
    packs: Option<&PackFlags>,
) -> Result<(), String> {
    if executor.js.is_none() {
        return Err(
            "no --storage needs --executor js-agent: with --executor kbf-daemon (the default) the \
             daemons read and write blobs on a storage node, so give --storage"
                .to_owned(),
        );
    }
    if !server.pack_index {
        return Err(
            "no --storage needs --pack-index: the blobs are found, and the action cache is kept, \
             in the Postgres state store"
                .to_owned(),
        );
    }
    if packs.is_none() {
        return Err(
            "no --storage needs --pack-store-endpoint (and --pack-store-bucket and \
             --pack-store-credentials-file): the blobs are kept in packs in that object store"
                .to_owned(),
        );
    }
    Ok(())
}

/// What `/readyz` and the health service ask of a frontend with no storage node: the stop signal
/// and the scheduler role ([`Readiness`]), the state store (where the pack index and the action
/// cache are) and, with an executor, the job service. No probe of the object store the packs are
/// in or of any storage node: a store that is down is found by the call that needs it.
#[must_use]
pub(super) fn stored_checks(
    readiness: Arc<Readiness>,
    state_store: Option<Arc<dyn kbf_state::Store>>,
    executor: Option<Arc<dyn crate::health::Probe>>,
    timeout: Duration,
) -> Checks {
    let mut checks = Checks::new(readiness);
    if let Some(store) = state_store {
        checks = checks.probing("state_store", Arc::new(StateProbe::new(store, timeout)));
    }
    if let Some(executor) = executor {
        checks = checks.probing("executor", executor);
    }
    checks
}

/// Where a frontend's blobs are: a storage node it dials, or the packs and the state store.
pub(super) enum Plane {
    /// The node `--storage` names: the lazy connection, and the storage calls over it.
    Node {
        /// The storage calls.
        storage: Arc<RemoteStorage>,
        /// The connection, which the proxy's raw ByteStream calls use too.
        channel: Channel,
    },
    /// No node: [`Stored`].
    Stored(Arc<Stored>),
}

impl Plane {
    /// The plane of a frontend whose connection to a node is `channel` (none without
    /// `--storage`), over `packs` and the action cache `actions`.
    ///
    /// # Errors
    /// With no node: see [`stored_of`].
    pub(super) fn of(
        channel: Option<Channel>,
        packs: Option<&Packs>,
        actions: Option<Arc<dyn ActionStore>>,
    ) -> Result<Self, Box<dyn Error>> {
        match channel {
            Some(channel) => Ok(Self::Node {
                storage: Arc::new(RemoteStorage::new(channel.clone(), 0)),
                channel,
            }),
            None => Ok(Self::Stored(Arc::new(stored_of(packs, actions)?))),
        }
    }
}

/// The blobs a frontend with no storage node serves: `packs` over the pack index, and `actions`,
/// the action cache table of the same handle.
///
/// # Errors
/// There are no packs or no action cache: the frontend was started without `--pack-index` or
/// `--pack-store-endpoint`, which [`parse`] refuses for a frontend with no `--storage`.
pub(super) fn stored_of(
    packs: Option<&Packs>,
    actions: Option<Arc<dyn ActionStore>>,
) -> Result<Stored, Box<dyn Error>> {
    let packs = packs.ok_or("no --storage needs --pack-store-endpoint")?;
    let actions = actions.ok_or("no --storage needs --pack-index")?;
    Ok(Stored::over(packs.clone(), actions))
}
