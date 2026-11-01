//! The idle limit of a body that is being sent.
//!
//! A response whose bytes are charged to the read budget keeps the charge while the connection
//! has the rest of them to send, and a client that opens a response and does not read it would
//! keep it for ever (enough of them stop every read of the listener). The connection stops
//! asking for the next piece of a body whose client is not reading, so the body cannot notice:
//! a watcher task does. When no piece has been taken for the idle limit, the watcher drops the
//! body, which drops its bytes and the budget they are charged to, and the next piece asked for
//! is [`StreamError::Idle`]. The limit is a tokio timer, so a test on a paused clock does not
//! wait for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt as _};
use tokio::sync::{Mutex, Notify};

use super::StreamError;

/// A body of chunks, as the listener sends it.
pub(super) type Chunks = BoxStream<'static, Result<Bytes, StreamError>>;

/// Tells the watcher that the body is gone.
struct Gone(Arc<Notify>);

impl Drop for Gone {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// `inner`, ended by [`StreamError::Idle`], and dropped, when no chunk of it is taken for `idle`.
pub(super) fn bounded(inner: Chunks, idle: Duration) -> Chunks {
    let body = Arc::new(Mutex::new(Some(inner)));
    let expired = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Notify::new());
    let gone = Arc::new(Notify::new());
    tokio::spawn(watch(
        Arc::clone(&body),
        Arc::clone(&expired),
        Arc::clone(&progress),
        Arc::clone(&gone),
        idle,
    ));
    stream::unfold(
        (body, expired, progress, Gone(gone)),
        |(body, expired, progress, gone)| async move {
            let item = match body.lock().await.as_mut() {
                Some(inner) => inner.next().await,
                None => expired
                    .swap(false, Ordering::SeqCst)
                    .then_some(Err(StreamError::Idle)),
            }?;
            progress.notify_one();
            Some((item, (body, expired, progress, gone)))
        },
    )
    .boxed()
}

/// Drops the body when `idle` passes with no progress, until the body is gone.
async fn watch(
    body: Arc<Mutex<Option<Chunks>>>,
    expired: Arc<AtomicBool>,
    progress: Arc<Notify>,
    gone: Arc<Notify>,
    idle: Duration,
) {
    loop {
        tokio::select! {
            biased;
            () = gone.notified() => return,
            () = progress.notified() => {}
            () = tokio::time::sleep(idle) => {
                // A chunk being waited for is waited for first: the body is dropped between
                // two chunks, not in the middle of one.
                body.lock().await.take();
                expired.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
}
