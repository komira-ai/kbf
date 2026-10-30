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
//! read of its key is a miss and fills it again. An object whose file cannot be read for another
//! reason (out of descriptors, a failing disk) is left alone and not counted: the scrub says
//! nothing it does not know.
//!
//! # The rate
//!
//! A [`Ticker`] says when to work. Each tick adds `per_tick` bytes of credit (at most one tick's
//! worth is kept), and an object is scrubbed while there is credit, which its size then spends: an
//! object bigger than the credit is read whole and leaves a debt that the next ticks pay before
//! anything else is read. The average is therefore `per_tick` bytes a tick and the largest burst
//! is one object. The walk takes the names in the index at its start, in order, and starts over
//! when it is done, so each object is visited once a round whatever eviction and fills do between.
//!
//! # Races
//!
//! The bytes are hashed from one open descriptor, with no lock held. To drop the object the scrub
//! then takes the table of the fills that run, which no fill can start or end a flight in while it
//! is held, and drops only if no fill of the name runs and the path still names the file that was
//! hashed. A reader therefore sees the object whole (before the drop) or absent (after it), a
//! file linked by a fill that ran between the hash and the drop is never removed for the old
//! file's bytes, and an object whose fill is still ending is left for the next round.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
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

/// Where the walk is and what credit it has.
#[derive(Debug)]
pub struct Scrubber {
    per_tick: u64,
    /// The names still to visit in this round, in order.
    queue: VecDeque<String>,
    /// The bytes that may be read now; negative while an object bigger than a tick's worth is paid
    /// for.
    credit: i128,
}

impl Scrubber {
    /// A walk that reads `per_tick` bytes a tick on average (at least one).
    #[must_use]
    pub fn new(per_tick: u64) -> Self {
        Self {
            per_tick: per_tick.max(1),
            queue: VecDeque::new(),
            credit: 0,
        }
    }
}

/// What one object came to.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The bytes match; this many were read.
    Intact(u64),
    /// The bytes did not match and the object was dropped; this many were read.
    Dropped(u64),
    /// Not judged: a fill of the name is running, the file could not be read, or it changed while
    /// it was read.
    Left,
}

/// A writer that hashes what it is given.
struct Hashing(Sha256);

impl Write for Hashing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The device and inode of a file: which file a name is.
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// The hex of the SHA-256 of what `file` holds, the bytes read, and which file it is.
fn hash_file(mut file: File) -> io::Result<(String, u64, (u64, u64))> {
    let opened = identity(&file.metadata()?);
    let mut hashing = Hashing(Sha256::new());
    let read = io::copy(&mut file, &mut hashing)?;
    Ok((hex::encode(hashing.0.finalize()), read, opened))
}

impl Inner {
    /// Whether a fill of `hex` is running.
    fn filling(&self, hex: &str) -> bool {
        locked(&self.flights).contains_key(hex)
    }

    /// Drops the object `hex` if no fill of it runs and its data file is still the one that was
    /// hashed (`hashed`: none if there was no file). Gives whether it was dropped.
    pub(super) fn drop_if_unchanged(&self, hex: &str, hashed: Option<(u64, u64)>) -> bool {
        // Held until the files are gone: no fill of the name can start in the meantime.
        let flights = locked(&self.flights);
        let now = std::fs::metadata(data_path(&self.config.path, hex))
            .ok()
            .map(|meta| identity(&meta));
        let drop_it = !flights.contains_key(hex) && now == hashed;
        if drop_it {
            self.drop_entry(hex);
        }
        drop_it
    }

    /// The digest the bytes of `hex` must have, read from its sidecar: the name for a key named by
    /// its content, else what the fill recorded. None if the sidecar cannot be read.
    fn expected(&self, hex: &str) -> Option<(String, String)> {
        let text = std::fs::read(meta_path(&self.config.path, hex)).ok()?;
        let sidecar: Sidecar = serde_json::from_slice(&text).ok()?;
        let digest = name_of(&sidecar.key).content.unwrap_or(sidecar.sha256);
        Some((sidecar.key, digest))
    }

    /// Hashes the object `hex` again and drops it if it does not match.
    fn scrub_object(&self, hex: &str) -> Verdict {
        if self.filling(hex) {
            return Verdict::Left;
        }
        let path = data_path(&self.config.path, hex);
        let file = match File::open(&path) {
            Ok(file) => file,
            // The file is gone: the entry is a lie, and is forgotten if nothing has put a file
            // there since.
            Err(error) if super::gone(&error) => {
                let _ = self.drop_if_unchanged(hex, None);
                return Verdict::Left;
            }
            Err(_) => return Verdict::Left,
        };
        let Ok((found, read, opened)) = hash_file(file) else {
            return Verdict::Left;
        };
        let wanted = self.expected(hex);
        if wanted.as_ref().is_some_and(|(_, digest)| *digest == found) {
            return Verdict::Intact(read);
        }
        if !self.drop_if_unchanged(hex, Some(opened)) {
            return Verdict::Left;
        }
        let key = wanted.map_or_else(|| hex.to_owned(), |(key, _)| key);
        eprintln!("tierstore: scrub dropped {key}: its bytes no longer match their digest");
        Verdict::Dropped(read)
    }

    /// One tick: scrubs objects while there is credit, and counts them.
    pub(super) fn scrub_tick(&self, state: &mut Scrubber) -> ScrubPass {
        let per_tick = i128::from(state.per_tick);
        state.credit = (state.credit + per_tick).min(per_tick);
        let mut pass = ScrubPass::default();
        let mut started = false;
        while state.credit > 0 {
            let Some(hex) = state.queue.pop_front() else {
                // One new round at most for a tick: an empty or all-skipped cache must end it.
                if started {
                    break;
                }
                started = true;
                state.queue = locked(&self.index).names().into();
                continue;
            };
            let (bytes, dropped) = match self.scrub_object(&hex) {
                Verdict::Intact(bytes) => (bytes, false),
                Verdict::Dropped(bytes) => (bytes, true),
                Verdict::Left => continue,
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
    /// ticker for anything.
    pub async fn scrub(&self, per_tick: Option<u64>, mut ticker: impl Ticker, halt: Arc<Notify>) {
        let Some(per_tick) = per_tick else {
            return;
        };
        let mut state = Scrubber::new(per_tick);
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
            let tick = tokio::task::spawn_blocking(move || {
                let pass = inner.scrub_tick(&mut state);
                (state, pass)
            });
            (state, _) = tick.await.expect("a scrub tick does not panic");
        }
    }
}
