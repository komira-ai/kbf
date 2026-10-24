//! The lease that makes one reconciler the runner.

use std::sync::Arc;

use bytes::Bytes;

use crate::journal::LEASE_KEY;
use crate::{Backend, Clock, Error};

/// A lease on the object `_tiering/lease` of the hot tier.
///
/// It is taken by a create-only put, and renewed by a put that replaces the object only if it
/// still has the content tag the last write gave (`If-Match`), which also moves the object's
/// time of last write on. A lease whose object was last written `ttl` ago or longer is
/// expired, and another holder may take it over, again with `If-Match`, so of two that try
/// at once one fails. A holder whose renewal fails (it lost the lease, or the hot tier did
/// not answer) stops holding it at once, and does not use it again before it takes it anew.
/// A holder that stops deletes the lease object only if it still has the content tag the
/// holder wrote.
///
/// The lease keeps two reconcilers from doing the same work. The copies are create-only and
/// every other step can be done twice, but a holder only learns that it lost the lease when
/// it renews it, once for each pass, so a pass longer than `ttl` can run beside another
/// holder's.
pub(crate) struct Lease {
    hot: Arc<dyn Backend>,
    clock: Arc<dyn Clock>,
    holder: String,
    ttl: u64,
    /// Counts the bodies written, so that every body differs from the one before and no
    /// holder is mistaken for another by the content tag.
    generation: u64,
    /// The content tag of the lease object as this holder last wrote it, while it holds it.
    etag: Option<String>,
}

impl Lease {
    /// A lease for `holder`, expired when its object is `ttl_millis` old.
    pub(crate) fn new(
        hot: Arc<dyn Backend>,
        clock: Arc<dyn Clock>,
        holder: &str,
        ttl_millis: u64,
    ) -> Self {
        Self {
            hot,
            clock,
            holder: holder.to_owned(),
            ttl: ttl_millis,
            generation: 0,
            etag: None,
        }
    }

    /// Holds the lease after this call: renews it if it is held, else takes it if it is free
    /// or expired.
    pub(crate) async fn ensure(&mut self) -> bool {
        match self.etag.clone() {
            Some(etag) => self.renew(&etag).await,
            None => self.acquire().await,
        }
    }

    /// Gives the lease up, if it is held and the lease object is still the one this holder
    /// wrote: a holder that lost the lease without knowing leaves the new holder's alone.
    pub(crate) async fn release(&mut self) {
        if let Some(etag) = self.etag.take() {
            let ours = match self.hot.head(LEASE_KEY).await {
                Ok(meta) => meta.tag == etag,
                Err(_) => false,
            };
            if ours {
                let _ = self.hot.delete(LEASE_KEY).await;
            }
        }
    }

    fn body(&mut self) -> Bytes {
        self.generation += 1;
        Bytes::from(format!("{}\n{}", self.holder, self.generation))
    }

    async fn renew(&mut self, etag: &str) -> bool {
        let body = self.body();
        self.etag = self.hot.put_if_match(LEASE_KEY, body, etag).await.ok();
        self.etag.is_some()
    }

    async fn acquire(&mut self) -> bool {
        let body = self.body();
        match self.hot.put(LEASE_KEY, body).await {
            Ok(()) => self.learn_tag().await,
            Err(Error::AlreadyExists(_)) => self.take_over().await,
            Err(_) => false,
        }
    }

    /// Reads the content tag of the lease object this holder just created.
    async fn learn_tag(&mut self) -> bool {
        self.etag = self.hot.head(LEASE_KEY).await.ok().map(|meta| meta.tag);
        self.etag.is_some()
    }

    /// Takes the lease over if its object has expired, by replacing the object that was seen.
    async fn take_over(&mut self) -> bool {
        let Ok(meta) = self.hot.head(LEASE_KEY).await else {
            return false;
        };
        if meta.modified.saturating_add(self.ttl) > self.clock.now() {
            return false;
        }
        self.renew(&meta.tag).await
    }
}
