//! The reads of the cache as streams: the chunks of a read are handed on as they are read from a
//! file or from the upstream, so that a read holds one chunk (`fill_chunk` bytes) in memory
//! whatever the size of the object.
//!
//! - A hit is read from the object's file a chunk at a time.
//! - A miss whose fill runs is read from the file the fill stages, up to the bytes the fill has
//!   written, while the fill goes on: the readers of one key each read the file with a handle of
//!   their own and share no bytes in memory. The last `fill_chunk` bytes of the window a reader
//!   asked for are held back until the fill has ended and said whether the object is good (the
//!   hash of a key named by its content, the second HEAD of any other key, the link into
//!   `obj/`), so a body that is not good never ends with all of its bytes: it ends with the
//!   error of the fill, which the answer turns into a broken response, never a clean end.
//! - A read that goes to the upstream (a key that is not kept, a range of a large object, a miss
//!   that finds every fill running) is read a chunk at a time by ranged reads, each made when
//!   the one before has been taken.

use std::fs::File;
use std::future;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use tokio::sync::watch;

use crate::{ByteRange, Error, ReadBody};

use super::fill::{Filled, Live, Pending};
use super::layout::{Name, data_path};
use super::metrics::{add, bump};
use super::{Inner, gone, locked, window};

/// Where the bytes of a read came from, for the counters.
#[derive(Clone, Copy)]
enum Served {
    /// The cache answered on its own.
    Local,
    /// The upstream's bytes (read on to the client, whether or not they were kept).
    Upstream,
}

/// `len` bytes of `file` from `offset`, read on a thread that may block.
async fn read_piece(file: Arc<File>, offset: u64, len: u64) -> io::Result<Bytes> {
    let reading = tokio::task::spawn_blocking(move || {
        // `len` is at most a chunk, which memory holds.
        let mut bytes = vec![0; len as usize];
        file.read_exact_at(&mut bytes, offset)?;
        Ok(Bytes::from(bytes))
    });
    reading.await.expect("a read does not panic")
}

/// The file at `path`, opened, if it is a regular file of at least `need` bytes.
fn open_whole(path: &std::path::Path, need: u64) -> io::Result<File> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("the object's name is not a file"));
    }
    if meta.len() < need {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(file)
}

/// `body` with its first chunk taken, so that a read that fails at once (the object is not
/// there, the range is past its end) is the error of the call and not of the body.
pub(super) async fn eager(mut body: ReadBody) -> Result<ReadBody, Error> {
    match body.next().await {
        None => Ok(stream::empty().boxed()),
        Some(Err(error)) => Err(error),
        Some(Ok(first)) => Ok(stream::once(future::ready(Ok(first))).chain(body).boxed()),
    }
}

impl Inner {
    /// The bytes a read holds at a time.
    fn chunk(&self) -> u64 {
        self.config.fill_chunk.max(1)
    }

    fn served(&self, from: Served, len: u64) {
        let counters = &self.counters;
        match from {
            Served::Local => add(&counters.bytes_served_local, len),
            Served::Upstream => add(&counters.bytes_served_upstream, len),
        }
    }

    /// `range` of the object from the upstream, a chunk at a time, keeping nothing: `len` bytes
    /// from `offset`, or to the end of the object for `None`. A chunk shorter than asked is the
    /// end of the object.
    pub(super) fn chunked(self: &Arc<Self>, key: &str, offset: u64, len: Option<u64>) -> ReadBody {
        let start = (Arc::clone(self), key.to_owned(), offset, len);
        stream::try_unfold(start, |(inner, key, offset, left)| async move {
            if left == Some(0) {
                return Ok(None);
            }
            let chunk = inner.chunk();
            let want = left.map_or(chunk, |left| left.min(chunk));
            let bytes = inner
                .upstream
                .get(&key, Some(ByteRange::new(offset, want)))
                .await?;
            let got = bytes.len() as u64;
            inner.served(Served::Upstream, got);
            // A short chunk is the last: nothing is asked beyond it.
            let left = if got < want {
                Some(0)
            } else {
                left.map(|left| left - got)
            };
            if got == 0 {
                return Ok(None);
            }
            Ok(Some((bytes, (inner, key, offset + got, left))))
        })
        .boxed()
    }

    /// The bytes of `range` of the object as a stream, if it is in the cache: `hit` says whether
    /// this is a read the cache answers on its own (a hit) or the read that follows a fill.
    pub(super) async fn open_kept(
        self: &Arc<Self>,
        name: &Name,
        range: Option<ByteRange>,
        hit: bool,
    ) -> Result<Option<ReadBody>, Error> {
        let used = locked(&self.index).used(&name.hex, self.clock.now());
        let Some(size) = used else {
            return Ok(None);
        };
        let (offset, len) = window(range, size)?;
        let path = data_path(&self.config.path, &name.hex);
        let opening = tokio::task::spawn_blocking(move || open_whole(&path, offset + len));
        match opening.await.expect("an open does not panic") {
            Ok(file) => {
                if hit {
                    bump(&self.counters.hits);
                }
                let from = if hit { Served::Local } else { Served::Upstream };
                Ok(Some(self.from_file(Arc::new(file), (offset, len), from)))
            }
            Err(error) if gone(&error) => {
                // The file is gone or short: the entry is a lie, so it is dropped.
                self.drop_entry(&name.hex);
                Ok(None)
            }
            // Out of descriptors, a failing disk: the object may be fine, so it stays.
            Err(_) => Ok(None),
        }
    }

    /// `len` bytes of `file` from `offset`, a chunk at a time.
    fn from_file(self: &Arc<Self>, file: Arc<File>, window: (u64, u64), from: Served) -> ReadBody {
        let (offset, len) = window;
        let start = (Arc::clone(self), file, offset, offset + len);
        stream::try_unfold(start, move |(inner, file, at, end)| async move {
            if at >= end {
                return Ok(None);
            }
            let len = (end - at).min(inner.chunk());
            let bytes = read_piece(Arc::clone(&file), at, len)
                .await
                .map_err(|error| Error::Other(format!("reading the cached object: {error}")))?;
            inner.served(from, len);
            Ok(Some((bytes, (inner, file, at + len, end))))
        })
        .boxed()
    }

    /// A read of `range` of `key` that follows the fill `pending`.
    pub(super) fn follow(
        self: &Arc<Self>,
        key: &str,
        range: Option<ByteRange>,
        pending: Pending,
    ) -> ReadBody {
        let follower = Follower {
            inner: Arc::clone(self),
            key: key.to_owned(),
            range,
            live: pending.live.clone(),
            live_open: true,
            done: pending,
            verdict: None,
            sent: range.map_or(0, ByteRange::offset),
            end: None,
            tail: None,
            finished: false,
        };
        stream::unfold(follower, |mut follower| async move {
            let item = follower.next().await?;
            Some((item, follower))
        })
        .boxed()
    }
}

/// A reader of a miss whose fill runs.
struct Follower {
    inner: Arc<Inner>,
    key: String,
    range: Option<ByteRange>,
    /// The end of the fill.
    done: Pending,
    /// How far the fill has written.
    live: watch::Receiver<Live>,
    /// Whether the fill can still change `live` (its task has not ended).
    live_open: bool,
    /// How the fill ended, once it has.
    verdict: Option<Result<Filled, Error>>,
    /// The offset in the object of the next byte to give.
    sent: u64,
    /// The offset in the object where the window ends, once the size is known.
    end: Option<u64>,
    /// The rest of the read from the upstream, once the fill ended without an object.
    tail: Option<ReadBody>,
    finished: bool,
}

impl Follower {
    /// Ends the body with `error`.
    fn fail(&mut self, error: Error) -> Option<Result<Bytes, Error>> {
        self.finished = true;
        Some(Err(error))
    }

    /// Reads the rest from the upstream: the fill ended without an object (no room, every fill
    /// running, the cache directory failed), which is not the reader's error.
    fn resume(&mut self) {
        let limit = self
            .end
            .or_else(|| self.range.map(|r| r.offset().saturating_add(r.size())));
        let left = limit.map(|limit| limit.saturating_sub(self.sent));
        self.tail = Some(self.inner.chunked(&self.key, self.sent, left));
    }

    /// The next chunk, or `None` at the end of the window.
    async fn next(&mut self) -> Option<Result<Bytes, Error>> {
        loop {
            if self.finished {
                return None;
            }
            if let Some(tail) = &mut self.tail {
                return tail.next().await;
            }
            let staged = match &*self.live.borrow_and_update() {
                Live::Starting => None,
                Live::Staging {
                    size,
                    file,
                    written,
                } => Some((*size, Arc::clone(file), *written)),
            };
            if let (Some((size, _, _)), None) = (&staged, self.end) {
                match window(self.range, *size) {
                    Ok((offset, len)) => self.end = Some(offset + len),
                    Err(error) => return self.fail(error),
                }
            }
            // The end of the window, once the fill has said the object is good (a window of no
            // bytes has nothing held back, and still waits for the verdict).
            let whole = matches!(self.verdict, Some(Ok(Filled::Kept)));
            if whole && self.end.is_some_and(|end| self.sent >= end) {
                return None;
            }
            // How far this reader may read: up to what is written and no further than the last
            // chunk of its window until the fill says the object is good, then to the end.
            let hold = self.inner.chunk();
            let ready = match (&self.verdict, &staged, self.end) {
                (None, Some((_, file, written)), Some(end)) => {
                    Some((file, (*written).min(end.saturating_sub(hold))))
                }
                (Some(Ok(Filled::Kept)), Some((_, file, _)), Some(end)) => Some((file, end)),
                _ => None,
            };
            if let Some((file, upto)) = ready
                && upto > self.sent
            {
                let len = (upto - self.sent).min(hold);
                let piece = read_piece(Arc::clone(file), self.sent, len).await;
                return match piece {
                    Ok(bytes) => {
                        self.sent += len;
                        self.inner.served(Served::Upstream, len);
                        Some(Ok(bytes))
                    }
                    Err(error) => {
                        let text = format!("reading the staged object: {error}");
                        self.fail(Error::Other(text))
                    }
                };
            }
            match &self.verdict {
                Some(Err(error)) => {
                    let error = error.clone();
                    return self.fail(error);
                }
                Some(Ok(_)) => self.resume(),
                None => {
                    tokio::select! {
                        changed = self.live.changed(), if self.live_open => {
                            self.live_open = changed.is_ok();
                        }
                        ended = &mut self.done => self.verdict = Some(ended),
                    }
                }
            }
        }
    }
}
