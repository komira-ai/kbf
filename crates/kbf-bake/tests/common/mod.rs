//! Fixtures shared by the evaluator's tests: a farm's metrics text, a pinned clock, and a
//! bake in a scratch directory driven by a script.

#![allow(dead_code)]

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use kbf_bake::drive::{Fetched, Outcome, drive};
use kbf_bake::eval::{Decision, Evaluator, Observation};
use kbf_bake::state::State;
use kbf_bake::updater::UpdaterLog;
use kbf_gate::{CommitId, Header, Metrics, ResultDoc, Run, SetHash, Stage};

/// The start of an hour, so that load hours are easy to count.
pub const T0: u64 = 3_600 * 1_000;
pub const INTERVAL: u64 = 60;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(T0) };
}

pub fn now() -> u64 {
    NOW.with(Cell::get)
}

pub fn set_now(t: u64) {
    NOW.with(|n| n.set(t));
}

/// What one scrape of a healthy farm says, with knobs to break it.
#[derive(Clone)]
pub struct Farm {
    pub requests: f64,
    pub errors: f64,
    pub lost_blobs: f64,
    pub losses: f64,
    pub under: Option<f64>,
    pub untraced: f64,
    pub failed_infra: f64,
    pub failed_user: f64,
    pub failed_lost: f64,
    pub flush_failures: f64,
    pub mismatches: f64,
    /// Cumulative queue histogram: (le, count); `+Inf` last.
    pub queue: Vec<(&'static str, f64)>,
    /// Metric families left out of the text.
    pub omit: Vec<&'static str>,
}

impl Farm {
    pub fn healthy(requests: f64) -> Self {
        Self {
            requests,
            errors: 0.0,
            lost_blobs: 0.0,
            losses: 0.0,
            under: Some(0.0),
            untraced: 0.0,
            failed_infra: 0.0,
            failed_user: 3.0,
            failed_lost: 0.0,
            flush_failures: 0.0,
            mismatches: 0.0,
            queue: vec![("0.5", requests), ("+Inf", requests)],
            omit: Vec::new(),
        }
    }

    pub fn without(mut self, family: &'static str) -> Self {
        self.omit.push(family);
        self
    }

    pub fn text(&self) -> String {
        let mut lines = vec![
            "# TYPE kbf_cache_lost_blobs gauge".to_owned(),
            format!("kbf_cache_lost_blobs {}", self.lost_blobs),
            format!("kbf_cache_blob_losses_total {}", self.losses),
            format!("kbf_frontend_relay_untraced {}", self.untraced),
            format!("kbf_execute_requests_total {}", self.requests),
            format!("kbf_execute_errors_total {}", self.errors),
            format!(
                "kbf_failed_actions_total{{class=\"infra\"}} {}",
                self.failed_infra
            ),
            format!(
                "kbf_failed_actions_total{{class=\"user\"}} {}",
                self.failed_user
            ),
            format!(
                "kbf_failed_actions_total{{class=\"lost_input\"}} {}",
                self.failed_lost
            ),
            format!(
                "kbf_write_tier_flush_failures_total {}",
                self.flush_failures
            ),
            format!("kbf_tier_flush_digest_mismatch_total {}", self.mismatches),
        ];
        if let Some(u) = self.under {
            lines.push(format!("kbf_under_replicated_blobs {u}"));
        }
        for (le, n) in &self.queue {
            lines.push(format!(
                "kbf_execute_queued_seconds_bucket{{le=\"{le}\"}} {n}"
            ));
        }
        lines.retain(|l| !self.omit.iter().any(|o| l.starts_with(o)));
        lines.join("\n") + "\n"
    }

    pub fn metrics(&self) -> Metrics {
        Metrics::parse(&self.text()).unwrap()
    }
}

pub fn commit() -> CommitId {
    CommitId::parse(&"ab".repeat(20)).unwrap()
}

pub fn set_hash() -> SetHash {
    SetHash::of_sums(b"sums of the release set\n")
}

/// A state for a bake that began at [`T0`], lasting `secs`, with baselines the healthy farm meets.
pub fn state(secs: u64) -> State {
    let mut s = State::new(commit(), set_hash(), "bake-0001".to_owned(), secs, T0);
    s.baseline.error_ratio = Some(0.0);
    s.baseline.queue_p99 = Some(0.5);
    s
}

pub fn evaluator(secs: u64) -> Evaluator {
    Evaluator::new(state(secs), 3 * INTERVAL)
}

pub fn quiet_log() -> UpdaterLog {
    UpdaterLog::default()
}

/// One observation with a quiet updater and `builds` builds.
pub fn observe(
    e: &mut Evaluator,
    at: u64,
    farm: &Farm,
    builds: u64,
    log: Option<&UpdaterLog>,
) -> Decision {
    e.observe(&Observation {
        at,
        metrics: &farm.metrics(),
        builds: Some(builds),
        updater: log,
    })
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A bake on disk: state and verdict files in a scratch directory.
pub struct Bake {
    pub state: PathBuf,
    pub verdict: PathBuf,
}

impl Bake {
    pub fn new(name: &str) -> Self {
        let dir = scratch(name);
        set_now(T0);
        Self {
            state: dir.join("state.json"),
            verdict: dir.join("gamma-verdict.json"),
        }
    }

    pub fn header() -> Header {
        Header {
            stage: Stage::Gamma,
            commit: commit(),
            set_hash: set_hash(),
            run_id: "bake-0001".to_owned(),
            attempt: 1,
        }
    }

    /// The evaluator over the saved state, or a new bake of `secs`.
    pub fn evaluator(&self, secs: u64) -> Evaluator {
        let s = State::load(&self.state)
            .unwrap()
            .unwrap_or_else(|| state(secs));
        Evaluator::new(s, 3 * INTERVAL)
    }

    /// Run until the bake is decided; the clock advances by the interval on each sleep.
    pub fn drive(
        &self,
        eval: &mut Evaluator,
        mut farm_at: impl FnMut(u64) -> Result<Farm, String>,
        builds: Option<u64>,
        log: Option<UpdaterLog>,
    ) -> Outcome {
        let run = Run::start(&self.verdict, Self::header(), now).unwrap();
        drive(
            eval,
            run,
            &self.state,
            INTERVAL,
            &mut || {
                farm_at(now()).map(|f| Fetched {
                    text: f.text(),
                    builds,
                })
            },
            &mut || log.clone(),
            &mut |secs| set_now(now() + secs),
        )
        .unwrap()
    }

    pub fn doc(&self) -> ResultDoc {
        ResultDoc::from_bytes(&std::fs::read(&self.verdict).unwrap()).unwrap()
    }
}

/// A farm whose requests grow by `per_tick` each interval since [`T0`].
pub fn growing(per_tick: f64) -> impl FnMut(u64) -> Result<Farm, String> {
    move |t| Ok(Farm::healthy(1.0 + per_tick * ((t - T0) / INTERVAL) as f64))
}

/// A server on a port of its own that answers every request with `status` and `body`, and hands
/// the raw request text (head and body) to the channel. The URL is its origin.
pub fn api(status: &'static str, body: &str) -> (String, mpsc::Receiver<String>) {
    api_with(status, body, || {})
}

/// [`api`], running `on_request` when a request has arrived, before it is answered.
pub fn api_with(
    status: &'static str,
    body: &str,
    on_request: impl Fn() + Send + 'static,
) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let body = body.to_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).into_owned();
                // The request is whole when its head is and the body has the length it declared.
                let whole = text.split_once("\r\n\r\n").is_some_and(|(head, rest)| {
                    let declared = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    rest.len() >= declared
                });
                if n == 0 || whole {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&got).into_owned());
            on_request();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, rx)
}
