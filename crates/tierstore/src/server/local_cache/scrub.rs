//! The scrub: a background walk of the cache that hashes each object again and drops the ones
//! whose bytes are not what they were when they were kept.
//!
//! A fill checks the bytes once, as they arrive; a hit is never hashed again, so an object that
//! rots on the disk would be served until it is evicted. The scrub reads each object at a bounded
//! rate and compares the SHA-256 of what the file holds now with the digest the object must have:
//!
//! - a key named by its content (`cas/l/<sha256>`) must hash to its name, whatever the sidecar
//!   says;
//! - any other key must hash to the `sha256` its sidecar recorded at the fill. Every sidecar the
//!   cache writes has that field (a sidecar without it does not parse, and the scan at start
//!   already deletes such an object), so no object is exempt.
//!
//! An object that does not match, whose sidecar cannot be read any more, is dropped from the index
//! and the disk, counted in `scrub_mismatches` and named on the standard error stream; the next
//! read of its key is a miss and fills it again. An object whose file or sidecar cannot be read for
//! another reason than being absent (out of descriptors, a failing disk) is left alone and counted
//! in `scrub_errors`: the scrub says nothing it does not know. Only a sidecar that is not there or
//! does not parse, with the data file there, makes an object that cannot be vouched for, and it is
//! dropped.
//!
//! # The rate
//!
//! A [`Ticker`] says when to work. Each tick adds `per_tick` bytes of credit (at most one tick's
//! worth is kept), and an object is scrubbed while there is credit, which its size then spends: an
//! object bigger than the credit is read whole and leaves a debt that the next ticks pay before
//! anything else is read. The average is therefore `per_tick` bytes a tick and the largest burst
//! is one object: inside one object the read is not paced (it runs at the speed of the disk), in
//! chunks of 1 MiB, and the stop flag is looked at before each chunk, so a halt ends it at the
//! next chunk and shutdown does not wait out a big object. The walk takes the names in the index
//! at its start, in order, and starts over when it is done, so each object is visited once a round
//! whatever eviction and fills do between. At least `min_round` ticks (an hour by default) lie
//! between the starts of two rounds, so a small cache is not read over and over at the rate.
//!
//! A cache that is read-only (a second process over the directory, see the module `hold`) runs no
//! pass: its ticks do nothing until it takes the lock, and a cache that loses the lock stops
//! dropping at once.
//!
//! An object whose check panics is counted in `scrub_errors`, skipped for the round, and the walk
//! goes on past it. A failure to read the identity of the open file is not counted: the object
//! is then never dropped (its identity cannot be matched), only passed over.
//!
//! # Races
//!
//! The bytes are hashed from one open descriptor, with no lock held, and the descriptor stays open
//! until the drop is decided (the inode number of a file that is open cannot be given to another
//! file). To drop the object the scrub then takes the table of the fills that run, which no fill
//! can start or end a flight in while it is held, and drops only if no fill of the name runs and
//! the path still names the file that was hashed. Whatever puts a file at `data_path` (a fill, and
//! so a write that a fill follows) does it inside a flight, which the scrub leaves alone; a
//! delete or a write-through put that marks a running fill ([`Inner::forget`]) finds that flight
//! still in the table, because the scrub never takes one out. A reader therefore sees the object
//! whole (before the drop) or absent (after it), a file linked by a fill that ran between the hash
//! and the drop is never removed for the old file's bytes, and an object whose fill is still ending is left for the next round.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::time::{Interval, MissedTickBehavior};

use super::layout::{Sidecar, data_path, meta_path, name_of};
use super::metrics::{add, bump};
use super::{Inner, LocalCache, locked};

/// The bytes a second the binary scrubs at unless the settings say otherwise.
pub const SCRUB_RATE: u64 = 16 << 20;

/// The rounds of the binary's scrub start at least this far apart, unless the settings say
/// otherwise.
pub const SCRUB_MIN_ROUND: Duration = Duration::from_secs(3600);

/// The time between two ticks of the binary's scrub.
pub const SCRUB_TICK: Duration = Duration::from_secs(1);

/// What wakes the scrub: the binary's is a timer, a test's is a hand.
#[async_trait]
pub trait Ticker: Send {
    /// Waits for the next tick. `false` says there will be none, and the scrub ends.
    async fn tick(&mut self) -> bool;
}

/// A [`Ticker`] that ticks every `period` of the runtime's clock.
pub struct Every(Interval);

impl Every {
    /// The first tick is one `period` from now. A tick that is late is not made up.
    #[must_use]
    pub fn new(period: Duration) -> Self {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self(interval)
    }
}

#[async_trait]
impl Ticker for Every {
    async fn tick(&mut self) -> bool {
        let _ = self.0.tick().await;
        true
    }
}

/// What one tick of the scrub did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrubPass {
    /// The objects that were hashed again, those that matched and those that did not.
    pub scrubbed: usize,
    /// The bytes that were read for them.
    pub bytes: u64,
    /// The objects that did not match and were dropped.
    pub dropped: usize,
}

/// The bytes read between two looks at the stop flag.
const CHUNK: usize = 1 << 20;

/// Where the walk is and what credit it has.
pub struct Scrubber {
    per_tick: u64,
    /// The names still to visit in this round, in order.
    queue: VecDeque<String>,
    /// The bytes that may be read now; negative while an object bigger than a tick's worth is paid
    /// for.
    credit: i128,
    /// The ticks seen, and the tick at which the round that is running (or ran last) started.
    ticks: u64,
    round_at: Option<u64>,
    /// The fewest ticks between the starts of two rounds.
    min_round: u64,
    /// Set to end the walk: looked at before each chunk of an object and each object.
    pub(super) stop: Arc<AtomicBool>,
    /// Runs after each chunk of an object is hashed.
    #[cfg(test)]
    pub(super) on_chunk: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Scrubber {
    /// A walk that reads `per_tick` bytes a tick on average (at least one).
    #[must_use]
    pub fn new(per_tick: u64) -> Self {
        Self {
            per_tick: per_tick.max(1),
            queue: VecDeque::new(),
            credit: 0,
            ticks: 0,
            round_at: None,
            min_round: 0,
            stop: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            on_chunk: None,
        }
    }

    /// At least `ticks` ticks between the starts of two rounds: a cache that is small is not read
    /// again at once, whatever the rate allows. The first round starts at the first tick.
    #[must_use]
    pub fn with_min_round(mut self, ticks: u64) -> Self {
        self.min_round = ticks;
        self
    }

    /// Whether a new round may start now.
    fn may_start_round(&self) -> bool {
        self.round_at
            .is_none_or(|at| self.ticks - at >= self.min_round)
    }
}

/// What one object came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The bytes match; this many were read.
    Intact(u64),
    /// The bytes did not match and the object was dropped; this many were read.
    Dropped(u64),
    /// Not judged: a fill of the name is running, the file changed while it was read, or
    /// something that is not the object's fault failed (counted in `scrub_errors`).
    Left,
    /// The walk was told to stop in the middle of the object.
    Stopped,
}

/// What the sidecar of an object says.
enum Recorded {
    /// The key and the digest the bytes must have.
    Digest(String, String),
    /// The sidecar is not there or is not a sidecar: the bytes cannot be vouched for.
    Bad,
    /// The sidecar could not be read (out of descriptors, a failing disk): no judgment.
    Unreadable,
}

/// The device and inode of a file: which file a name is.
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

impl Scrubber {
    /// The hex of the SHA-256 of what `file` holds, in chunks, and the bytes read; none if the walk
    /// was told to stop first.
    fn hash(&self, file: &mut File) -> io::Result<Option<(String, u64)>> {
        let mut hasher = Sha256::new();
        let mut buffer = Vec::with_capacity(CHUNK);
        let mut total = 0;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            // `read_to_end` of a `take` retries a read that was interrupted.
            buffer.clear();
            let read = file.by_ref().take(CHUNK as u64).read_to_end(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer);
            total += read as u64;
            #[cfg(test)]
            if let Some(hook) = &self.on_chunk {
                hook();
            }
        }
        Ok(Some((hex::encode(hasher.finalize()), total)))
    }
}

impl Inner {
    /// Whether a fill of `hex` is running.
    fn filling(&self, hex: &str) -> bool {
        locked(&self.flights).contains_key(hex)
    }

    /// Drops the object `hex` if no fill of it runs and its data file is still the file `hashed`
    /// says (none: no file). The caller keeps that file open until this returns, so its inode
    /// number cannot have been given to another file meanwhile. Gives whether it was dropped.
    pub(super) fn drop_if_unchanged(&self, hex: &str, hashed: Option<(u64, u64)>) -> bool {
        // Held until the files are gone: no fill of the name can start in the meantime, and a
        // delete or a write that marks a running fill (see `forget`) finds the fill, which this
        // does not touch.
        let flights = locked(&self.flights);
        // A cache that does not hold the lock of its directory deletes nothing in it, and one
        // that lost the lock in the middle of a pass stops dropping.
        if !self.read_write() {
            return false;
        }
        let now = std::fs::metadata(data_path(&self.config.path, hex))
            .ok()
            .map(|meta| identity(&meta));
        let drop_it = !flights.contains_key(hex) && now == hashed;
        if drop_it {
            self.drop_entry(hex);
        }
        drop_it
    }

    /// What the sidecar of `hex` says the bytes must hash to: the name for a key named by its
    /// content, else what the fill recorded.
    fn recorded(&self, hex: &str) -> Recorded {
        let text = match std::fs::read(meta_path(&self.config.path, hex)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Recorded::Bad,
            Err(_) => return Recorded::Unreadable,
        };
        match serde_json::from_slice::<Sidecar>(&text) {
            Ok(sidecar) => {
                let digest = name_of(&sidecar.key).content.unwrap_or(sidecar.sha256);
                Recorded::Digest(sidecar.key, digest)
            }
            Err(_) => Recorded::Bad,
        }
    }

    /// Counts something the scrub could not judge for a reason that is not the object's.
    fn scrub_error(&self) -> Verdict {
        bump(&self.counters.scrub_errors);
        Verdict::Left
    }

    /// Hashes the object `hex` again and drops it if it does not match.
    fn scrub_object(&self, hex: &str, state: &Scrubber) -> Verdict {
        self.scrub_object_between(hex, state, || {})
    }

    /// [`scrub_object`](Self::scrub_object), running `between` after the bytes are judged and
    /// before the object is dropped.
    pub(super) fn scrub_object_between(
        &self,
        hex: &str,
        state: &Scrubber,
        between: impl FnOnce(),
    ) -> Verdict {
        if self.filling(hex) {
            return Verdict::Left;
        }
        let path = data_path(&self.config.path, hex);
        // Open until the end: the file's inode is not freed while it is.
        let mut file = match File::open(&path) {
            Ok(file) => file,
            // The file is gone: the entry is a lie, and is forgotten if nothing has put a file
            // there since.
            Err(error) if super::gone(&error) => {
                let _ = self.drop_if_unchanged(hex, None);
                return Verdict::Left;
            }
            Err(_) => return self.scrub_error(),
        };
        // Without the file's identity nothing is dropped: it is not the one that is on the path.
        let opened = file.metadata().ok().map(|meta| identity(&meta));
        let (matches, read, key) = match self.recorded(hex) {
            Recorded::Unreadable => return self.scrub_error(),
            Recorded::Bad => (false, 0, hex.to_owned()),
            Recorded::Digest(key, digest) => match state.hash(&mut file) {
                Ok(Some((found, read))) => (found == digest, read, key),
                Ok(None) => return Verdict::Stopped,
                Err(_) => return self.scrub_error(),
            },
        };
        if matches {
            return Verdict::Intact(read);
        }
        between();
        if !self.drop_if_unchanged(hex, opened) {
            return Verdict::Left;
        }
        eprintln!("tierstore: scrub dropped {key}: its bytes no longer match their digest");
        Verdict::Dropped(read)
    }

    /// One tick: scrubs objects while there is credit, and counts them.
    pub(super) fn scrub_tick(&self, state: &mut Scrubber) -> ScrubPass {
        // A cache that is read-only (it does not hold the lock of its directory) leaves the files
        // to the holder: no pass reads or drops anything. A tick still counts, and the pass
        // starts when the cache takes the lock.
        if !self.read_write() {
            return ScrubPass::default();
        }
        let per_tick = i128::from(state.per_tick);
        state.ticks += 1;
        state.credit = (state.credit + per_tick).min(per_tick);
        let mut pass = ScrubPass::default();
        let mut started = false;
        while state.credit > 0 && !state.stop.load(Ordering::SeqCst) {
            let Some(hex) = state.queue.pop_front() else {
                // One new round at most for a tick: an empty or all-skipped cache must end it.
                if started || !state.may_start_round() {
                    break;
                }
                started = true;
                state.round_at = Some(state.ticks);
                let mut names = locked(&self.index).names();
                names.sort();
                state.queue = names.into();
                continue;
            };
            // An object whose check panics is counted and skipped for this round, and the walk
            // goes on past it.
            let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.scrub_object(&hex, state)
            }));
            let (bytes, dropped) = match checked.unwrap_or_else(|_| self.scrub_error()) {
                Verdict::Intact(bytes) => (bytes, false),
                Verdict::Dropped(bytes) => (bytes, true),
                Verdict::Left => continue,
                Verdict::Stopped => break,
            };
            state.credit -= i128::from(bytes);
            pass.scrubbed += 1;
            pass.bytes += bytes;
            pass.dropped += usize::from(dropped);
            bump(&self.counters.scrubbed_objects);
            add(&self.counters.scrubbed_bytes, bytes);
            if dropped {
                bump(&self.counters.scrub_mismatches);
            }
        }
        pass
    }
}

impl LocalCache {
    /// One tick of the scrub now, on the calling thread (which reads files): see the module docs.
    #[must_use]
    pub fn scrub_once(&self, state: &mut Scrubber) -> ScrubPass {
        self.inner.scrub_tick(state)
    }

    /// Scrubs `per_tick` bytes of objects at each tick of `ticker`, until `halt` is notified or
    /// the ticker ends. `None` is the scrub turned off: it returns at once and never asks the
    /// ticker for anything. A halt ends the pass that is running at the next chunk. A tick that
    /// panics is reported on the standard error stream and the walk starts over: the scrub never
    /// takes the cache down with it.
    pub async fn scrub(
        &self,
        per_tick: Option<u64>,
        min_round: u64,
        ticker: impl Ticker,
        halt: Arc<Notify>,
    ) {
        let Some(per_tick) = per_tick else {
            return;
        };
        let state = Scrubber::new(per_tick).with_min_round(min_round);
        self.scrub_with(state, ticker, halt).await;
    }

    /// [`scrub`](Self::scrub) from a walk that is already made.
    pub(super) async fn scrub_with(
        &self,
        mut state: Scrubber,
        mut ticker: impl Ticker,
        halt: Arc<Notify>,
    ) {
        loop {
            let more = tokio::select! {
                biased;
                () = halt.notified() => false,
                more = ticker.tick() => more,
            };
            if !more {
                break;
            }
            let inner = Arc::clone(&self.inner);
            let stop = Arc::clone(&state.stop);
            let (per_tick, min_round) = (state.per_tick, state.min_round);
            let mut running = tokio::task::spawn_blocking(move || {
                let pass = inner.scrub_tick(&mut state);
                (state, pass)
            });
            let finished = tokio::select! {
                biased;
                () = halt.notified() => None,
                done = &mut running => Some(done),
            };
            let Some(done) = finished else {
                stop.store(true, Ordering::SeqCst);
                let _ = running.await;
                break;
            };
            // A tick that fails outside an object's check starts the walk over: it is reported and
            // does not take the cache down.
            state = done.map_or_else(
                |error| {
                    eprintln!("tierstore: a scrub tick failed, the walk starts over: {error}");
                    let fresh = Scrubber::new(per_tick).with_min_round(min_round);
                    Scrubber { stop, ..fresh }
                },
                |(state, _)| state,
            );
        }
    }
}
