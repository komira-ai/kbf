//! The hand-over of the body of a put to the call of the backend that stores it, which runs on
//! a task of its own.
//!
//! The request reads its body a piece at a time, checks the pieces against the hashes the
//! request claims, and passes them on through a queue of [`PIECES_QUEUED`] pieces; the backend
//! takes them from the other end as a stream. The queue is what holds the memory of a put to a
//! few pieces: when the backend is slow the request stops reading, whatever the length of the
//! object.
//!
//! The stream ends cleanly only if the request says so, after it has read the whole body and
//! every hash came out right. A stream whose request is gone (the client went away, so the
//! request was dropped) without saying so ends with an error, so a backend never takes a cut
//! body for a whole one.

use std::future::Future;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tokio::sync::mpsc;

use crate::{Error, PartBody};

/// How many pieces of a body wait for the backend to take them. With the piece the request has
/// in hand and the one the backend has, that is all a streamed put holds between the client and
/// the backend; a piece is what the connection delivers in one frame, so the memory of a put does
/// not grow with the length of the object.
pub(crate) const PIECES_QUEUED: usize = 4;

/// What crosses the queue: a piece, the end of the body, or the error that ended it.
type Item = Result<Option<Bytes>, Error>;

/// The error of a body whose request went away before it ended.
fn cut() -> Error {
    Error::Other("the request body ended before it was complete".to_owned())
}

/// Splits `pieces` into the future that carries them, which the request runs, and the stream
/// they arrive on, which the backend takes. The future ends when all the pieces were passed on
/// (and the end of the body with them), when a piece that is an error was passed on, or when the
/// stream was dropped.
pub(crate) fn hand_over<'a>(
    pieces: PartBody<'a>,
) -> (impl Future<Output = ()> + Send + 'a, PartBody<'static>) {
    let (sender, receiver) = mpsc::channel(PIECES_QUEUED);
    (carry(pieces, sender), received(receiver))
}

/// Passes the pieces to the queue, then the end of the body, unless a piece is an error, which
/// ends the body in its place.
async fn carry(mut pieces: PartBody<'_>, queue: mpsc::Sender<Item>) {
    while let Some(piece) = pieces.next().await {
        let failed = piece.is_err();
        if queue.send(piece.map(Some)).await.is_err() || failed {
            return;
        }
    }
    let _ = queue.send(Ok(None)).await;
}

/// The pieces of the queue as a stream: it ends after the end of the body, ends with an error
/// after an error of the body, and ends with an error if the queue is closed before either.
fn received(queue: mpsc::Receiver<Item>) -> PartBody<'static> {
    stream::unfold(Some(queue), |state| async move {
        let mut queue = state?;
        match queue.recv().await {
            Some(Ok(Some(piece))) => Some((Ok(piece), Some(queue))),
            Some(Ok(None)) => None,
            Some(Err(error)) => Some((Err(error), None)),
            None => Some((Err(cut()), None)),
        }
    })
    .fuse()
    .boxed()
}
