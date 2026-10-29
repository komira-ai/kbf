//! The hand-over of the body of a put to the backend.

use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};

use crate::PartBody;

use super::{PIECES_QUEUED, hand_over};

/// Asserts that the future that carries the pieces ends, without a hang and without taking more
/// pieces from the body, when the backend's end of the queue is dropped while it waits to pass a
/// piece on: the queue holds `PIECES_QUEUED` pieces and the carrier holds one more, blocked; when
/// the receiver goes, that send fails and the carrier returns, leaving the rest of the body
/// unread and sending no end of body. It catches a carrier that goes on reading a body nobody
/// takes. The test counts the pieces taken from the body; it waits for nothing.
#[test]
fn the_carrier_ends_when_the_stream_is_dropped_while_it_waits_to_send() {
    let taken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&taken);
    let total = PIECES_QUEUED + 3;
    let pieces: PartBody<'static> = stream::iter((0..total).map(|_| Ok(Bytes::from_static(b"x"))))
        .inspect(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .boxed();
    let (carry, body) = hand_over(pieces);
    let mut carry = pin!(carry);
    assert!(
        carry.as_mut().now_or_never().is_none(),
        "blocked on a full queue"
    );
    assert_eq!(taken.load(Ordering::SeqCst), PIECES_QUEUED + 1);
    drop(body);
    assert!(
        carry.as_mut().now_or_never().is_some(),
        "ends once the stream is gone"
    );
    assert_eq!(taken.load(Ordering::SeqCst), PIECES_QUEUED + 1);
}
