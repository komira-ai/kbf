//! Reading the log lines a test's code writes, without a race between tests.
//!
//! `tracing` caches, for the whole process, whether each callsite is enabled, computed
//! against the subscribers that exist when the callsite is first reached. A subscriber set
//! for one thread (`set_default`) changes what that thread sees, but not a cache another
//! test's thread filled in while no subscriber was set: that test can leave a callsite
//! cached as never, and this thread's subscriber then sees nothing from it.
//!
//! This module installs one global subscriber for the test binary, the first time any test
//! asks, so every callsite is computed against a subscriber that is always there. Its
//! filter is dynamic, so the answer is taken per event and never cached: an event is
//! written when its level is at or above that of the capture running on the thread that
//! wrote it (INFO when none runs), and the line goes to that capture's buffer, or nowhere.
//!
//! A capture sees the lines written on the thread that started it. A test whose code logs
//! from a task runs it on a current-thread runtime (`#[tokio::test]`, the default flavour),
//! where every task runs on the test's thread.

use std::cell::RefCell;
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::Duration;

use tracing::Level;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// How long [`Capture::ended`] waits for the line.
const BOUND: Duration = Duration::from_secs(10);

/// What one capture collects.
struct Sink {
    /// The most verbose level the capture takes.
    level: Level,
    bytes: Mutex<Vec<u8>>,
    /// The text whose line ends a wait, and the wake-up sent when a line holds it.
    wake: Option<(&'static [u8], tokio::sync::Notify)>,
}

impl Sink {
    fn append(&self, line: &[u8]) {
        self.bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(line);
        let woken =
            (self.wake.as_ref()).filter(|(text, _)| line.windows(text.len()).any(|w| w == *text));
        if let Some((_, notify)) = woken {
            notify.notify_one();
        }
    }
}

thread_local! {
    /// The captures running on this thread, the innermost last.
    static SINKS: RefCell<Vec<Arc<Sink>>> = const { RefCell::new(Vec::new()) };
}

/// The writer of the global subscriber: the innermost capture of the writing thread.
#[derive(Clone, Copy)]
struct Writer;

impl io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        SINKS.with(|sinks| {
            if let Some(sink) = sinks.borrow().last() {
                sink.append(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Writer {
    type Writer = Writer;

    fn make_writer(&'a self) -> Writer {
        *self
    }
}

/// Installs the global subscriber, once for the process.
///
/// # Panics
/// Another global subscriber was installed first: the capture would see nothing.
pub(crate) fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let level_here = |meta: &tracing::Metadata<'_>| {
            let wanted = SINKS.with(|sinks| sinks.borrow().last().map(|sink| sink.level));
            *meta.level() <= wanted.unwrap_or(Level::INFO)
        };
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(Writer)
                    .with_filter(dynamic_filter_fn(move |meta, _| level_here(meta))),
            )
            .try_init()
            .expect("no other global subscriber is installed in the test binary");
    });
}

/// The lines the current thread's code writes, from [`capture`] until this drops.
pub(crate) struct Capture {
    sink: Arc<Sink>,
    /// Not `Send`: it ends the capture on the thread that started it.
    _thread: PhantomData<Rc<()>>,
}

impl Capture {
    fn start(level: Level, wake: Option<&'static str>) -> Self {
        install();
        let sink = Arc::new(Sink {
            level,
            bytes: Mutex::default(),
            wake: wake.map(|text| (text.as_bytes(), tokio::sync::Notify::new())),
        });
        SINKS.with(|sinks| sinks.borrow_mut().push(Arc::clone(&sink)));
        Self {
            sink,
            _thread: PhantomData,
        }
    }

    /// Everything written since the capture started.
    pub(crate) fn text(&self) -> String {
        let bytes = self
            .sink
            .bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes).expect("UTF-8 logs")
    }

    /// Waits, at most ten seconds, for a line holding the text [`capture_until`] was given.
    ///
    /// # Panics
    /// No such line came in time, or the capture was started without a text.
    pub(crate) async fn ended(&self) {
        let (_, notify) = self.sink.wake.as_ref().expect("a capture given a text");
        let ended = tokio::time::timeout(BOUND, notify.notified()).await;
        ended.expect("no line holding the text waited for");
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        SINKS.with(|sinks| {
            sinks
                .borrow_mut()
                .retain(|sink| !Arc::ptr_eq(sink, &self.sink));
        });
    }
}

/// Starts capturing the INFO and above lines written on this thread.
pub(crate) fn capture() -> Capture {
    Capture::start(Level::INFO, None)
}

/// Starts capturing the lines at `level` and above written on this thread.
pub(crate) fn capture_at(level: Level) -> Capture {
    Capture::start(level, None)
}

/// Starts capturing the INFO and above lines written on this thread, and lets
/// [`Capture::ended`] wait for a line holding `text`.
pub(crate) fn capture_until(text: &'static str) -> Capture {
    Capture::start(Level::INFO, Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line written while a capture runs is in its text; a line written after it ended,
    /// or on another thread, is not.
    #[test]
    fn a_capture_holds_the_lines_of_its_own_thread_while_it_runs() {
        let log = capture();
        tracing::info!("seen by the capture");
        std::thread::spawn(|| tracing::info!("written on another thread"))
            .join()
            .expect("the thread ends");
        let text = log.text();
        assert!(text.contains("seen by the capture"), "{text}");
        assert!(!text.contains("another thread"), "{text}");
        drop(log);
        let later = capture();
        tracing::info!("seen by the second capture");
        assert!(!later.text().contains("seen by the capture"));
        assert!(later.text().contains("seen by the second capture"));
    }

    /// A capture takes INFO and above unless it asks for more, and the inner capture of a
    /// thread takes the lines while it runs, the outer one the rest.
    #[test]
    fn a_capture_takes_the_level_it_asks_for_and_the_inner_one_wins() {
        let outer = capture();
        tracing::debug!("a debug line");
        tracing::info!("outer line");
        {
            let inner = capture_at(Level::TRACE);
            tracing::debug!("inner debug line");
            assert!(inner.text().contains("inner debug line"));
        }
        tracing::info!("outer again");
        let text = outer.text();
        assert!(!text.contains("a debug line"), "{text}");
        assert!(!text.contains("inner debug line"), "{text}");
        assert!(text.contains("outer line"), "{text}");
        assert!(text.contains("outer again"), "{text}");
    }

    /// The writer takes a flush, as a `fmt` layer's writer may be asked for.
    #[test]
    fn the_writer_flushes_without_a_capture() {
        io::Write::flush(&mut Writer).expect("a flush");
    }

    /// `ended` returns once a line holding the text was written, even one written before the
    /// wait began.
    #[tokio::test]
    async fn ended_returns_for_a_line_written_before_the_wait() {
        let log = capture_until("the awaited text");
        tracing::info!("a line with the awaited text");
        log.ended().await;
    }
}
