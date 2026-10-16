//! A process-wide capture of what the unit tests log.
//!
//! A subscriber set for one thread is consulted for a callsite only while it is the one
//! subscriber that exists, and a callsite first reached by a test thread with none can be
//! cached as disabled for all of them: under parallel tests, a thread-local capture
//! sometimes misses an event. One global subscriber sees every thread's events. Tests
//! assert on text that only they log (node names of their own), never on a count of all.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

/// The captured log text.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Log {
    /// Everything captured so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log").clone()).expect("UTF-8")
    }

    /// The captured lines that contain `marker`.
    pub fn lines_with(&self, marker: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains(marker))
            .map(str::to_owned)
            .collect()
    }
}

/// The capture, installed as the global subscriber on first use.
pub fn captured() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the one global subscriber");
        log
    })
}
