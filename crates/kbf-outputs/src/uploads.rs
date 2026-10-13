//! [`Uploads`]: the blobs a walk of the outputs stores, sent a bounded number at a time
//! while the walk goes on, each distinct digest once.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _, TryStreamExt as _, future};
use kbf_proto::reapi::Digest;

/// One blob's upload, not started until the drain polls it.
type Upload<'a, E> = Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>;

/// The uploads a walk queues. Each one is a future that is not polled until its turn;
/// the drain [`Uploads::new`] returns runs them, `in_flight` at a time, and must be
/// polled alongside the walk (`futures::try_join!`). A queued upload waits for a free
/// slot, so a walk holds at most `in_flight` + 1 blobs (and the descriptors of the
/// files among them) at a time, not the whole tree. One at a time spent a lease on
/// round trips: a tree of thousands of small outputs took minutes to store while the
/// CAS sat idle. The store's own limit ([`crate::Store::in_flight`], the transfer
/// engine's for the whole process) bounds the requests; this bounds what one walk holds.
/// Dropping the `Uploads` once the walk is done lets the drain finish.
pub struct Uploads<'a, E> {
    sender: mpsc::Sender<Upload<'a, E>>,
    queued: HashSet<String>,
}

impl<'a, E: 'a> Uploads<'a, E> {
    /// An empty queue, and the drain that runs what is queued, `in_flight` (at least 1)
    /// at a time: it finishes with the first upload that fails, or once every upload
    /// succeeded and the `Uploads` is dropped.
    pub fn new(in_flight: usize) -> (Self, impl Future<Output = Result<(), E>> + Send + 'a)
    where
        E: Send,
    {
        // No slack: a queued upload waits until the drain takes it.
        let (sender, receiver) = mpsc::channel::<Upload<'a, E>>(0);
        let drain = receiver
            .buffer_unordered(in_flight.max(1))
            .try_for_each(|()| future::ready(Ok(())));
        let queue = Self {
            sender,
            queued: HashSet::new(),
        };
        (queue, drain)
    }

    /// Queues `upload`, which stores the blob `digest` names, unless a blob with that
    /// digest was queued already; waits until the drain has room for it.
    pub async fn queue(
        &mut self,
        digest: &Digest,
        upload: impl Future<Output = Result<(), E>> + Send + 'a,
    ) {
        if !self
            .queued
            .insert(format!("{}/{}", digest.hash, digest.size_bytes))
        {
            return;
        }
        // The drain stops taking uploads only once one failed; the drain returns that
        // error, so this one is not needed.
        let _ = self.sender.send(Box::pin(upload)).await;
    }
}
