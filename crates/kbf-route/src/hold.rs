//! The bounded hold: how long a call waits for an owner to come back before it is
//! UNAVAILABLE.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

/// The bound the frontend and the daemon hold a call for: 60 seconds. A placeholder: it must
/// exceed one storage node's stop plus start, and neither has been measured yet. There is
/// no flag for it: the processes pass this constant.
pub const DEFAULT_HOLD: Duration = Duration::from_secs(60);

/// The pause before the second try; each pause after it is twice the one before. The pauses
/// are not jittered: the calls that stalled together try again together, which a node that
/// has just registered answers as it answers any burst.
const FIRST_PAUSE: Duration = Duration::from_millis(250);

/// The longest pause between two tries.
const MAX_PAUSE: Duration = Duration::from_secs(5);

/// The time and the sleeping a hold uses, so that a test runs a hold without real time.
/// [`Router::with_hold_clock`](crate::Router::with_hold_clock) and [`Hold::with_clock`] set
/// it; the default is Tokio's clock, which a test can pause.
pub trait HoldClock: Send + Sync {
    /// The time passed since some fixed moment of this clock's choosing. It never goes back.
    fn elapsed(&self) -> Duration;

    /// Completes after `pause` of this clock's time.
    fn sleep(&self, pause: Duration) -> BoxFuture<'_, ()>;
}

/// Tokio's clock.
struct TokioClock {
    origin: tokio::time::Instant,
}

impl TokioClock {
    fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl HoldClock for TokioClock {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, pause: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(pause))
    }
}

/// How long a call that no owner could answer is held, and by which clock. A bound of zero
/// holds nothing. A [`Router`](crate::Router) has one; a caller that talks to the nodes
/// itself from a [`Placement`](crate::Placement) (the frontend's pass-through of a
/// ByteStream) holds with its own, which pauses and ends as the router's does.
#[derive(Clone)]
pub struct Hold {
    pub(crate) bound: Duration,
    pub(crate) clock: Arc<dyn HoldClock>,
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hold")
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

impl Hold {
    /// A hold of `bound`, by Tokio's clock.
    #[must_use]
    pub fn new(bound: Duration) -> Self {
        Self {
            bound,
            clock: Arc::new(TokioClock::new()),
        }
    }

    /// No hold: a call that no owner could answer is UNAVAILABLE at once.
    #[must_use]
    pub fn none() -> Self {
        Self::new(Duration::ZERO)
    }

    /// The same hold, by `clock` (the time, and the sleeping between tries) instead of
    /// Tokio's: for a test that runs a hold without real time.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn HoldClock>) -> Self {
        self.clock = clock;
        self
    }

    /// How long a call is held at most.
    #[must_use]
    pub fn bound(&self) -> Duration {
        self.bound
    }

    /// The start of one call's hold.
    #[must_use]
    pub fn begin(&self) -> Waiting<'_> {
        Waiting {
            hold: self,
            start: self.clock.elapsed(),
            pause: FIRST_PAUSE,
        }
    }
}

/// One call's hold, from the first try.
pub struct Waiting<'a> {
    hold: &'a Hold,
    start: Duration,
    pause: Duration,
}

impl Waiting<'_> {
    /// Pauses before the next try and answers true, unless the bound has passed since the
    /// call began, which it answers at once with false. The pauses double up to five
    /// seconds, and the last is cut short to end at the bound, so the call is tried once
    /// more at the bound and never held past it.
    pub async fn again(&mut self) -> bool {
        let spent = self.hold.clock.elapsed().saturating_sub(self.start);
        if spent >= self.hold.bound {
            return false;
        }
        let pause = self.pause.min(self.hold.bound - spent);
        self.hold.clock.sleep(pause).await;
        self.pause = (self.pause * 2).min(MAX_PAUSE);
        true
    }
}
