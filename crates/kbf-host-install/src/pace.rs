//! How the waits of this program tell time and pause: the clock of the machine in the program,
//! a made-up one in tests, so that a test drives time and asserts what happened (how many
//! looks, in what order, with what result) and never how long it took.

use std::time::{Duration, Instant};

/// A monotonic reading.
pub type Now = Box<dyn Fn() -> Duration>;

/// The clock of the machine: the time since this reading was made.
pub fn real_now() -> Now {
    let start = Instant::now();
    Box::new(move || start.elapsed())
}

/// How the waits tell time and pause: the real clock in the program, a made-up one in tests.
pub struct Pace {
    /// How long to pause between two looks.
    pub poll: Duration,
    /// A monotonic reading.
    pub now: Now,
    pub sleep: Box<dyn Fn(Duration)>,
}

impl Pace {
    /// The clock of the machine.
    pub fn real() -> Self {
        Self {
            poll: Duration::from_secs(2),
            now: real_now(),
            sleep: Box::new(std::thread::sleep),
        }
    }
}

/// A made-up clock for tests: time moves only when the code under test sleeps (or, for a
/// clock that `stepping`, when it is read).
#[cfg(test)]
pub mod fake {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Duration;

    use super::{Now, Pace};

    /// The most sleeps one clock allows: a wait that never ends fails the test instead of
    /// hanging it.
    const MOST_SLEEPS: usize = 100_000;

    type OnSleep = Box<dyn Fn(usize)>;

    struct Inner {
        now: Cell<Duration>,
        step: Duration,
        sleeps: RefCell<Vec<Duration>>,
        reads: Cell<usize>,
        on_sleep: RefCell<Option<OnSleep>>,
    }

    /// The virtual time of one test, shared by the clock and the sleep it hands out.
    #[derive(Clone)]
    pub struct Fake(Rc<Inner>);

    impl Fake {
        /// A clock that starts at zero and moves only when slept on.
        pub fn new() -> Self {
            Self::stepping(Duration::ZERO)
        }

        /// A clock that also moves by `step` every time it is read.
        pub fn stepping(step: Duration) -> Self {
            Self(Rc::new(Inner {
                now: Cell::new(Duration::ZERO),
                step,
                sleeps: RefCell::new(Vec::new()),
                reads: Cell::new(0),
                on_sleep: RefCell::new(None),
            }))
        }

        /// Run `hook` with the number of the sleep (from 1) each time the code sleeps,
        /// before time moves: what happens in the world while the code waits.
        pub fn on_sleep(&self, hook: impl Fn(usize) + 'static) {
            *self.0.on_sleep.borrow_mut() = Some(Box::new(hook));
        }

        /// The reading of the clock.
        pub fn now_fn(&self) -> Now {
            let inner = Rc::clone(&self.0);
            Box::new(move || {
                inner.reads.set(inner.reads.get() + 1);
                let now = inner.now.get();
                inner.now.set(now + inner.step);
                now
            })
        }

        /// A pace over this clock: a sleep moves time by what was slept.
        pub fn pace(&self) -> Pace {
            let inner = Rc::clone(&self.0);
            Pace {
                poll: Duration::from_secs(1),
                now: self.now_fn(),
                sleep: Box::new(move |d| {
                    let count = {
                        let mut sleeps = inner.sleeps.borrow_mut();
                        sleeps.push(d);
                        sleeps.len()
                    };
                    assert!(count <= MOST_SLEEPS, "a wait that never ends");
                    if let Some(hook) = inner.on_sleep.borrow().as_ref() {
                        hook(count);
                    }
                    inner.now.set(inner.now.get() + d);
                }),
            }
        }

        /// What was slept, in order.
        pub fn sleeps(&self) -> Vec<Duration> {
            self.0.sleeps.borrow().clone()
        }

        /// How many times the clock was read.
        pub fn reads(&self) -> usize {
            self.0.reads.get()
        }
    }
}
