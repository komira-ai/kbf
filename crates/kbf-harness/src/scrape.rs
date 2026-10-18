//! The frontend's `/metrics`, read every second for the whole scenario into a file of
//! samples (one JSON line each) and kept in memory for the gates.
//!
//! Only the frontend exposes metrics today (the storage and daemon sources have no
//! route), so these are the frontend's series; the other roles are covered by the
//! process table. The reader takes the Prometheus text format as the frontend writes it.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::api;

/// One scrape: when (seconds since the scraper started) and every series, keyed as the
/// exposition writes it (`name` or `name{labels}`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sample {
    pub at: f64,
    pub series: BTreeMap<String, f64>,
}

/// Parses the Prometheus text format: `name value` and `name{labels} value [timestamp]`
/// lines; comments and blank lines are skipped. A line that is neither is an error, so a
/// page that is not metrics (an error page) is not read as an empty scrape.
///
/// # Errors
/// The first line that does not parse.
pub fn parse(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let mut series = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("not a metrics line: {line:?}");
        let (key, rest) = split_key(line).ok_or_else(bad)?;
        let value = rest.split_whitespace().next().ok_or_else(bad)?;
        let value: f64 = value.parse().map_err(|_| bad())?;
        series.insert(key.to_owned(), value);
    }
    Ok(series)
}

/// The series key (`name` or `name{...}`, quotes and escapes in the labels respected) and
/// the rest of the line.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let end = match line.find('{') {
        None => line.find(char::is_whitespace)?,
        Some(open) if !line[..open].contains(char::is_whitespace) => {
            let (mut quoted, mut escaped) = (false, false);
            let mut close = None;
            for (i, c) in line[open..].char_indices() {
                match (escaped, quoted, c) {
                    (true, _, _) => escaped = false,
                    (false, true, '\\') => escaped = true,
                    (false, _, '"') => quoted = !quoted,
                    (false, false, '}') => {
                        close = Some(open + i + 1);
                        break;
                    }
                    _ => {}
                }
            }
            close?
        }
        Some(_) => line.find(char::is_whitespace)?,
    };
    Some((&line[..end], &line[end..]))
}

/// The family name of a series key: `name` for `name{labels}`.
#[must_use]
pub fn family(key: &str) -> &str {
    key.split('{').next().unwrap_or(key)
}

impl Sample {
    /// The sum of every series of the family `name`, or `None` if it has none: an absent
    /// metric is not zero.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<f64> {
        let mut found = self
            .series
            .iter()
            .filter(|(key, _)| family(key) == name)
            .map(|(_, v)| *v)
            .peekable();
        found.peek()?;
        Some(found.sum())
    }
}

/// What a scraper saw.
#[derive(Clone, Debug, Default)]
pub struct Scrapes {
    pub samples: Vec<Sample>,
    /// Scrapes that got no page, or one that did not parse, with the reason of the last.
    pub failed: u64,
    pub last_error: Option<String>,
}

/// A scraper thread. [`Scraper::stop`] ends it and returns what it read.
pub struct Scraper {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Scrapes>,
}

impl Scraper {
    /// Scrapes `GET /metrics` of the operator API at `127.0.0.1:api` every `every`,
    /// appending each sample to `file` as a JSON line.
    ///
    /// # Panics
    /// When `file` cannot be created.
    #[must_use]
    pub fn start(api: u16, every: Duration, file: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let mut out = std::fs::File::create(file).expect("the series file");
        let handle = std::thread::spawn(move || {
            let begun = Instant::now();
            let mut scrapes = Scrapes::default();
            let mut once = |scrapes: &mut Scrapes| match scrape_once(api) {
                Ok(series) => {
                    let sample = Sample {
                        at: begun.elapsed().as_secs_f64(),
                        series,
                    };
                    let line = serde_json::to_string(&sample).expect("json");
                    let _ = writeln!(out, "{line}");
                    scrapes.samples.push(sample);
                }
                Err(e) => {
                    scrapes.failed += 1;
                    scrapes.last_error = Some(e);
                }
            };
            while !flag.load(Ordering::Relaxed) {
                let tick = Instant::now();
                once(&mut scrapes);
                // Short sleeps, so a stop is heard promptly.
                let until = Instant::now() + every.saturating_sub(tick.elapsed());
                while Instant::now() < until && !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            // The last sample is taken after the stop was asked for: the scenario's end.
            once(&mut scrapes);
            scrapes
        });
        Self { stop, handle }
    }

    /// Ends the scraper after one last scrape (so the scenario's last sample is taken
    /// after it asked for the stop), and returns what it read. The first sample is the
    /// one taken when it started.
    ///
    /// # Panics
    /// When the thread panicked.
    #[must_use]
    pub fn stop(self) -> Scrapes {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().expect("the scraper thread")
    }
}

/// One scrape of the frontend.
///
/// # Errors
/// No answer, a status other than 200, or a page that is not metrics.
pub fn scrape_once(api: u16) -> Result<BTreeMap<String, f64>, String> {
    let (code, body) = api::get(api, "/metrics").ok_or("no answer from /metrics")?;
    if code != 200 {
        return Err(format!("/metrics answered {code}"));
    }
    parse(&body)
}

/// The path of the series file in `logs`.
#[must_use]
pub fn series_file(logs: &Path) -> PathBuf {
    logs.join("metrics.jsonl")
}

#[cfg(test)]
#[path = "scrape_tests.rs"]
mod tests;
