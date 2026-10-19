//! The bodies of a proxied stream: the answer the client gets, the request bytes the proxy
//! keeps for a re-dispatch, and the pump that carries them upstream.
//!
//! Flow control is plain here. A request body is read frame by frame, and hyper returns the
//! client its credit only as each frame is read: a held stream never reads, so it returns
//! nothing and the client stops at one window. A response body returns credit upstream as
//! each frame is handed to hyper.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use h2::{Reason, RecvStream, SendStream};
use http::HeaderMap;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::core::StreamCount;
use crate::identity::strip_reserved;
use crate::pool::Reservation;
use crate::replay::{Failure, REPLAY_BYTES, ReplayBuffer, may_retry};
use crate::reply::unavailable_trailers;
use crate::route::Idle;
use crate::slot::Permit;

/// A response body that failed after its headers went out.
#[derive(Debug, thiserror::Error)]
#[error("the upstream stream failed")]
pub(crate) struct BodyError;

/// What a stream holds for its whole life: its slot permit and its upstream stream. It drops
/// when the last of its two halves (the response body, the request pump) is done, never at
/// the downstream END_STREAM.
pub(crate) struct Lifetime {
    _permit: Permit,
    _reservation: Reservation,
    /// Set by the response half when it gives up: the request pump resets the upstream stream.
    cancel: Notify,
    /// The stream's place under `max_streams`.
    _count: Arc<StreamCount>,
}

impl Lifetime {
    pub(crate) fn new(
        permit: Permit,
        reservation: Reservation,
        count: Arc<StreamCount>,
    ) -> Arc<Self> {
        Arc::new(Self {
            _permit: permit,
            _reservation: reservation,
            cancel: Notify::new(),
            _count: count,
        })
    }
}

/// When a stream last moved a byte either way, for the idle timeout.
#[derive(Debug)]
pub(crate) struct Activity {
    start: Instant,
    last_ms: AtomicU64,
}

impl Activity {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
        })
    }

    pub(crate) fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(ms, Ordering::AcqRel);
    }

    fn last(&self) -> Instant {
        self.start + std::time::Duration::from_millis(self.last_ms.load(Ordering::Acquire))
    }
}

/// Resolves when the stream has been silent for `idle`; never for [`Idle::Never`].
pub(crate) async fn idle_expired(activity: Arc<Activity>, idle: Idle) {
    let Idle::After(after) = idle else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep_until(activity.last() + after).await;
        if Instant::now() >= activity.last() + after {
            return;
        }
    }
}

type Idling = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The upstream response, handed to hyper frame by frame.
pub(crate) struct Down {
    recv: RecvStream,
    life: Arc<Lifetime>,
    activity: Arc<Activity>,
    idle: Option<Idling>,
    grpc: bool,
}

impl Down {
    pub(crate) fn new(
        recv: RecvStream,
        life: Arc<Lifetime>,
        activity: Arc<Activity>,
        idle: Idle,
        grpc: bool,
    ) -> Self {
        let idling = matches!(idle, Idle::After(_))
            .then(|| Box::pin(idle_expired(Arc::clone(&activity), idle)) as Idling);
        Self {
            recv,
            life,
            activity,
            idle: idling,
            grpc,
        }
    }

    /// The stream failed or went idle after its headers went out: a gRPC client is told by a
    /// non-OK trailer (never by a short OK message), anything else by a reset. The request
    /// half is cancelled too.
    fn fail(&mut self) -> Option<Result<Frame<Bytes>, BodyError>> {
        self.life.cancel.notify_one();
        Some(if self.grpc {
            Ok(Frame::trailers(unavailable_trailers(
                "the upstream stream failed",
            )))
        } else {
            Err(BodyError)
        })
    }

    fn poll_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        if let Some(idle) = self.idle.as_mut()
            && idle.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(self.fail());
        }
        // Data while there is data, then the trailers if there are any. One error arm serves
        // both reads: a failure at either is a failed stream.
        let next = match ready!(self.recv.poll_data(cx)) {
            Some(data) => data.map(|d| Some(Item::Data(d))),
            None => ready!(self.recv.poll_trailers(cx)).map(|t| t.map(Item::Trailers)),
        };
        // `Ok(None)` is the end of the stream: no data and no trailers left. (When the last DATA
        // frame carries END_STREAM, `is_end_stream` ends the body before this is polled again.)
        Poll::Ready(match next {
            Ok(item) => item.map(|item| {
                Ok(match item {
                    Item::Data(data) => {
                        let _ = self.recv.flow_control().release_capacity(data.len());
                        self.activity.touch();
                        Frame::data(data)
                    }
                    Item::Trailers(mut trailers) => {
                        strip_reserved(&mut trailers);
                        Frame::trailers(trailers)
                    }
                })
            }),
            Err(_) => self.fail(),
        })
    }
}

/// What the upstream response gave next.
enum Item {
    Data(Bytes),
    Trailers(HeaderMap),
}

/// A body of the proxy's own answers, or an upstream response being proxied.
pub(crate) enum ProxyBody {
    Full(Option<Bytes>),
    Up(Box<Down>),
}

impl ProxyBody {
    pub(crate) const fn empty() -> Self {
        Self::Full(None)
    }

    pub(crate) const fn bytes(bytes: Bytes) -> Self {
        Self::Full(Some(bytes))
    }
}

impl Body for ProxyBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        match &mut *self {
            Self::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            Self::Up(down) => down.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Full(bytes) => bytes.is_none(),
            // True for a trailers-only answer, so its HEADERS carry END_STREAM and a client
            // that reads `grpc-status` from the last HEADERS frame finds it.
            Self::Up(down) => down.recv.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(Some(bytes)) => SizeHint::with_exact(bytes.len() as u64),
            _ => SizeHint::default(),
        }
    }
}

/// One frame of the request, as kept for a re-dispatch.
#[derive(Clone, Debug)]
enum Kept {
    Data(Bytes),
    Trailers(HeaderMap),
}

/// The most frames kept for a replay. A request that has more is not replayed.
const MAX_KEPT_FRAMES: usize = 64;

/// What the pump does next.
enum Next {
    Frame(Kept),
    End,
    /// The client's request body failed (a reset): the upstream stream is reset too.
    Failed,
}

/// What the attempts of a stream share about the request, behind a lock that is never held
/// across an await.
struct Replay {
    kept: Vec<Kept>,
    buffer: ReplayBuffer,
    ended: bool,
}

/// The client's request body, shared by the attempts of one stream. The first
/// [`REPLAY_BYTES`] are kept as `Bytes` clones, so a re-dispatch replays them and then goes on
/// reading the client. Only one attempt's pump runs at a time: a retry waits for the old one.
///
/// The body itself is behind an async lock that only the pump takes, and holds while it waits
/// for the client: everything else (the retry decision, closing the replay window at the
/// response) reads the small state under a plain lock, so a pump parked on a client that is
/// slow to send never blocks the response half.
pub(crate) struct Source {
    body: Mutex<Incoming>,
    state: StdMutex<Replay>,
}

impl Source {
    pub(crate) fn new(body: Incoming) -> Arc<Self> {
        let ended = body.is_end_stream();
        let mut buffer = ReplayBuffer::new(REPLAY_BYTES);
        if ended {
            buffer.end_request();
        }
        Arc::new(Self {
            body: Mutex::new(body),
            state: StdMutex::new(Replay {
                kept: Vec::new(),
                buffer,
                ended,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Replay> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the request has no body at all (headers only): the one case that is sent as
    /// END_STREAM on the headers. A request that was read to its end and has frames to
    /// replay is not empty.
    pub(crate) fn bodiless(&self) -> bool {
        let state = self.state();
        state.ended && state.kept.is_empty()
    }

    /// Whether a failed dispatch may be repeated (see [`may_retry`]).
    pub(crate) fn may_retry(&self, path: &str, failure: Failure, attempts: u8) -> bool {
        may_retry(path, failure, &self.state().buffer, attempts)
    }

    /// Reads the upstream's first response head: true if it is the fenced answer (the window
    /// stays open for [`Source::may_retry`]), else the request may have had an effect and the
    /// window is closed for good. See [`ReplayBuffer::on_first_response`].
    pub(crate) fn on_first_response(&self, headers: &HeaderMap, end_stream: bool) -> bool {
        self.state().buffer.on_first_response(headers, end_stream)
    }

    /// Closes the window: the request is never replayed again.
    pub(crate) fn close_window(&self) {
        self.state().buffer.close();
    }

    /// The next frame for an attempt whose position is `cursor`: a kept one first, then the
    /// client's own.
    async fn next(&self, cursor: &mut usize) -> Next {
        {
            let state = self.state();
            if let Some(kept) = state.kept.get(*cursor) {
                *cursor += 1;
                return Next::Frame(kept.clone());
            }
            if state.ended {
                return Next::End;
            }
        }
        let frame = self.body.lock().await.frame().await;
        let mut state = self.state();
        match frame {
            None => {
                state.ended = true;
                state.buffer.end_request();
                Next::End
            }
            Some(Err(_)) => Next::Failed,
            Some(Ok(frame)) => {
                let kept = match frame.into_data() {
                    Ok(data) => Kept::Data(data),
                    Err(frame) => {
                        // Trailers end the request.
                        state.ended = true;
                        state.buffer.end_request();
                        Kept::Trailers(frame.into_trailers().unwrap_or_default())
                    }
                };
                let len = match &kept {
                    Kept::Data(data) => data.len(),
                    Kept::Trailers(_) => 0,
                };
                // The window is closed by bytes (`REPLAY_BYTES`) and by frames: a flood of
                // tiny frames would otherwise grow the list, and pin the read buffers the
                // frames were cut from, far past the byte bound. (h2 drops an empty DATA
                // frame that does not end the stream before the body sees it.)
                if state.kept.len() >= MAX_KEPT_FRAMES {
                    state.buffer.close();
                } else if state.buffer.push(len) {
                    state.kept.push(kept.clone());
                    *cursor += 1;
                }
                Next::Frame(kept)
            }
        }
    }
}

/// Sends `data` upstream within the stream's flow-control window.
async fn send_chunk(send: &mut SendStream<Bytes>, mut data: Bytes) -> Result<(), h2::Error> {
    while !data.is_empty() {
        send.reserve_capacity(data.len());
        let granted = std::future::poll_fn(|cx| send.poll_capacity(cx)).await;
        // `None`: the stream is no longer in a state that can send (the peer reset it).
        let granted = granted.ok_or(h2::Error::from(Reason::CANCEL))??;
        let chunk = data.split_to(granted.min(data.len()));
        send.send_data(chunk, false)?;
    }
    Ok(())
}

/// Carries the client's request upstream, frame by frame, until it ends, either side resets,
/// or the response half cancels. Holds the stream's [`Lifetime`] until then, so the permit
/// outlives the response when the request is still open.
pub(crate) async fn pump(
    source: Arc<Source>,
    mut send: SendStream<Bytes>,
    activity: Arc<Activity>,
    life: Arc<Lifetime>,
) {
    let mut cursor = 0;
    loop {
        let next = tokio::select! {
            () = life.cancel.notified() => Next::Failed,
            next = source.next(&mut cursor) => next,
        };
        match next {
            Next::Frame(Kept::Data(data)) => {
                // The response half's `cancel` also ends a pump parked on a full window.
                let cancelled = tokio::select! {
                    () = life.cancel.notified() => true,
                    sent = send_chunk(&mut send, data) => {
                        if sent.is_err() {
                            return;
                        }
                        false
                    }
                };
                if cancelled {
                    send.send_reset(Reason::CANCEL);
                    return;
                }
            }
            Next::Frame(Kept::Trailers(trailers)) => {
                let _ = send.send_trailers(trailers);
                return;
            }
            Next::End => {
                let _ = send.send_data(Bytes::new(), true);
                return;
            }
            Next::Failed => {
                send.send_reset(Reason::CANCEL);
                return;
            }
        }
        activity.touch();
    }
}
