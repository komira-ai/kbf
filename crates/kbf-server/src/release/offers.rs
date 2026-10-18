//! The offers a frontend sends over its nodes' sessions (`docs/design/pull-updates.md`
//! sections 5 and 6.4): for each platform, the `UpdateOffer` that names the desired release
//! and its components, built from the `update/desired` row and the release store.
//!
//! - **One offer per complete platform.** A platform is offered only when the store holds
//!   every component it needs, attested ([`kbf_types::ReleasePlatform::components`]), for
//!   the desired commit. An offer carries those components and no others (a Linux host is
//!   not told about `kbf-daemon`), each with its digest, its size and the attestation bytes
//!   as stored.
//! - **Level-triggered.** [`Offers::start`] looks at the row and the store every `every`
//!   and publishes a new [`Table`] when it differs from the last. A session holds a
//!   receiver ([`Offers::subscribe`]) and sends its platform's offer when the table gives it
//!   one it has not sent ([`push`]): after `Welcome`, and again at each change of the
//!   desired release or of a platform's completeness. A platform that stops being complete
//!   is sent nothing (the node keeps the offer it holds).
//! - **A look that fails changes nothing.** The row or the store could not be read: the
//!   table stays as it was, and the first failure after a good look is logged.
//!
//! A change reaches the nodes within one `every` of the write, which is the farm's tick.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_proto::update::{Component as WireComponent, UpdateOffer};
use kbf_proto::worker::server_message;
use kbf_types::ReleasePlatform;
use tokio::sync::watch;

use super::desired::blocking;
use super::key::{Commit, Key};
use super::store::ReleaseStore;
use crate::farm::Outbound;
use crate::update::{RowError, Rows};

/// The offer of each platform that has a complete release.
pub type Table = BTreeMap<ReleasePlatform, UpdateOffer>;

/// Why a look failed.
#[derive(Debug, thiserror::Error)]
enum LookError {
    #[error("the update rows: {0}")]
    Rows(#[from] RowError),
    #[error("the release store: {0}")]
    Store(#[from] io::Error),
}

/// The offers of one frontend.
#[derive(Debug)]
pub struct Offers {
    store: Arc<ReleaseStore>,
    rows: Rows,
    table: watch::Sender<Arc<Table>>,
    /// The last look failed (so a run of failures is logged once).
    failing: AtomicBool,
}

/// The offers over `store` and `rows`, if the server has both; `None` otherwise (no
/// release store or no state store: nothing to offer). See [`Offers::start`].
#[must_use]
pub fn offers_of(
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    every: Duration,
) -> Option<Arc<Offers>> {
    store
        .zip(rows)
        .map(|(store, rows)| Offers::start(store, rows, every))
}

impl Offers {
    /// The offers over `store` and `rows`, looked at every `every` on a task of its own
    /// that ends once the last holder of the returned value is gone. The first look is at
    /// once; until it is done the table is empty.
    ///
    /// Call it on a tokio runtime.
    #[must_use]
    pub fn start(store: Arc<ReleaseStore>, rows: Rows, every: Duration) -> Arc<Self> {
        let (table, _) = watch::channel(Arc::new(Table::new()));
        let offers = Arc::new(Self {
            store,
            rows,
            table,
            failing: AtomicBool::new(false),
        });
        tokio::spawn(poll(Arc::downgrade(&offers), every));
        offers
    }

    /// A receiver of the table, holding the current one.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<Table>> {
        self.table.subscribe()
    }

    /// Looks at the row and the store now, and publishes the table if it changed.
    pub async fn refresh(&self) {
        match self.look().await {
            Ok(table) => {
                self.failing.store(false, Ordering::Relaxed);
                self.table.send_if_modified(|held| {
                    let changed = **held != table;
                    if changed {
                        *held = Arc::new(table);
                    }
                    changed
                });
            }
            Err(e) => {
                if !self.failing.swap(true, Ordering::Relaxed) {
                    tracing::warn!("offers: the look failed, the table is kept: {e}");
                }
            }
        }
    }

    /// The table the row and the store give now.
    async fn look(&self) -> Result<Table, LookError> {
        let rows = self.rows.clone();
        let desired = blocking(move || rows.desired()).await?.value;
        // A row that names no commit the store could hold offers nothing.
        let Some((desired, commit)) =
            desired.and_then(|d| Commit::parse(d.commit.as_str()).map(|c| (d, c)))
        else {
            return Ok(Table::new());
        };
        let Some(listing) = self.store.commit(commit.clone()).await? else {
            return Ok(Table::new());
        };
        let mut table = Table::new();
        for held in listing.platforms.iter().filter(|p| p.complete) {
            let platform = held.platform;
            let mut components = Vec::new();
            for info in held
                .components
                .iter()
                .filter(|c| platform.components().contains(&c.component.as_str()))
            {
                let key = Key {
                    commit: commit.clone(),
                    platform,
                    component: info.component,
                };
                // Complete means attested, and a key never changes: a bundle that is
                // missing now is a store that fails, and the table is kept.
                let attestation = self
                    .store
                    .attestation(&key)
                    .await?
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
                components.push(WireComponent {
                    name: info.component.as_str().to_owned(),
                    sha256: info.sha256.to_string(),
                    size: info.size,
                    attestation,
                });
            }
            let offer = UpdateOffer {
                generation: desired.generation.0,
                commit: commit.to_string(),
                platform: platform.as_str().to_owned(),
                components,
            };
            table.insert(platform, offer);
        }
        Ok(table)
    }
}

/// Looks every `every` until `offers` has no holder left.
async fn poll(offers: Weak<Offers>, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(offers) = offers.upgrade() else {
            return;
        };
        offers.refresh().await;
    }
}

/// The platform a node's report names, if a release is built for it
/// ([`ReleasePlatform::from_report`] over its `os` and `arch`).
#[must_use]
pub fn platform_of(caps: &NodeCaps) -> Option<ReleasePlatform> {
    let os = caps.exact.get("os").map_or("", String::as_str);
    ReleasePlatform::from_report(os, caps.cpu.arch().name())
}

/// What a session sends its node of the offers: nothing for a server without offers or a
/// node on a platform no release is built for.
#[derive(Clone, Debug)]
pub(crate) struct OfferFeed(Option<Arc<Offers>>);

impl OfferFeed {
    /// No offers: a server without a release store or a state store.
    pub(crate) const NONE: Self = Self(None);

    pub(crate) const fn new(offers: Arc<Offers>) -> Self {
        Self(Some(offers))
    }

    /// Starts sending `outbound` the offers for `platform` as the table gives them, until
    /// the stream is gone. Called after the `Welcome` was queued on `outbound`, so the
    /// first offer follows it.
    pub(crate) fn push(&self, platform: Option<ReleasePlatform>, outbound: &Outbound) {
        if let Some((offers, platform)) = self.0.as_ref().zip(platform) {
            tokio::spawn(push(offers.subscribe(), platform, outbound.clone()));
        }
    }
}

/// Sends `outbound` the offer of `platform` each time the table gives one it has not
/// sent, the table it holds now first, until the stream is gone or the offers are.
async fn push(
    mut table: watch::Receiver<Arc<Table>>,
    platform: ReleasePlatform,
    outbound: Outbound,
) {
    let mut sent: Option<UpdateOffer> = None;
    let sending = async {
        loop {
            let offer = table.borrow_and_update().get(&platform).cloned();
            if let Some(offer) = offer.filter(|o| sent.as_ref() != Some(o)) {
                let message = server_message::Message::UpdateOffer(offer.clone());
                // A send fails only for a stream that is gone, which `closed` ends.
                let _ = outbound.send(Ok(crate::worker::message(message)));
                sent = Some(offer);
            }
            if table.changed().await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        () = outbound.closed() => {}
        () = sending => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kbf_state::ManualClock;
    use kbf_state::Store;
    use kbf_state::fake::{Fake, Flaw};

    use super::super::store::Limits;
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kbf-offers-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn rows() -> Rows {
        let clock = Arc::new(ManualClock::default());
        Rows::new(Arc::new(Fake::with_flaw(clock, Flaw::None)) as Arc<dyn Store>)
    }

    fn store(name: &str) -> Arc<ReleaseStore> {
        Arc::new(ReleaseStore::open(&scratch(name), Limits::default()).expect("a store"))
    }

    /// Catches a platform read from the wrong report entry, a pair no release is built for
    /// taken as one, and an `os` of another spelling accepted.
    #[test]
    fn a_nodes_platform_is_its_os_and_arch() {
        let table = [
            ("linux", "x86_64", Some(ReleasePlatform::LinuxX86_64)),
            ("linux", "arm64", Some(ReleasePlatform::LinuxArm64)),
            ("macos", "arm64", Some(ReleasePlatform::DarwinArm64)),
            ("macos", "x86_64", None),
            ("freebsd", "x86_64", None),
        ];
        for (os, arch, want) in table {
            let caps = NodeCaps::from_report([("os", os), ("arch", arch)]).expect("a report");
            assert_eq!(platform_of(&caps), want, "{os}/{arch}");
        }
        let no_os = NodeCaps::from_report([("arch", "x86_64")]).expect("a report");
        assert_eq!(platform_of(&no_os), None, "a report with no os");
    }

    /// Catches offers built from half of what they need: with no release store or no state
    /// store there are none.
    #[tokio::test]
    async fn offers_need_both_a_release_store_and_a_state_store() {
        let every = Duration::from_secs(1);
        assert!(offers_of(None, None, every).is_none());
        assert!(offers_of(Some(store("half-a")), None, every).is_none());
        assert!(offers_of(None, Some(rows()), every).is_none());
        let both = offers_of(Some(store("both")), Some(rows()), every).expect("offers");
        assert!(both.subscribe().borrow().is_empty());
    }

    /// Catches a look that never ends once the offers are gone (a task per frontend left
    /// running for the life of the process).
    #[tokio::test(start_paused = true)]
    async fn the_poll_ends_when_the_offers_are_dropped() {
        let offers = Offers::start(store("poll"), rows(), Duration::from_secs(1));
        let held = Arc::downgrade(&offers);
        drop(offers);
        tokio::time::timeout(Duration::from_secs(60), poll(held, Duration::from_secs(1)))
            .await
            .expect("the poll ends");
    }
}
