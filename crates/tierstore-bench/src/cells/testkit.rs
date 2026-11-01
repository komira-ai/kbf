//! What the tests of the cells share: a scripted machine, a configuration over loopback stores,
//! and a context to run a cell in. Compiled for tests only.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::cells::CellContext;
use crate::cells::budget::{Campaign, Ledger};
use crate::cells::engine::LoadSource;
use crate::coldstate::{ColdStateSource, NoColdState};
use crate::config::{ByteCaps, RateCaps, RunConfig, Secret, StoreSpec};
use crate::error::BenchError;
use crate::gate::{LoadGate, LoadSample};
use crate::requestlog::RequestLog;
use crate::result::{ColdState, Provenance, Window};
use crate::role::Role;

/// A machine at rest.
pub fn calm() -> LoadSample {
    LoadSample {
        loadavg_1m: 0.5,
        psi_cpu_avg10: 0.0,
        psi_io_avg10: 0.0,
        disk_util_pct: 1.0,
    }
}

/// A machine over the load limit.
pub fn busy() -> LoadSample {
    LoadSample {
        loadavg_1m: 99.0,
        ..calm()
    }
}

/// A machine that answers what it was told, in order, then is calm; `None` is a machine that
/// cannot be read.
pub struct Script(Mutex<VecDeque<Option<LoadSample>>>);

impl Script {
    /// A machine that is calm for ever.
    pub fn calm() -> Arc<Self> {
        Self::of(vec![])
    }

    /// A machine that answers `readings` first.
    pub fn of(readings: Vec<Option<LoadSample>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(readings.into())))
    }
}

impl LoadSource for Script {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        let next = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        next.unwrap_or(Some(calm()))
            .ok_or(BenchError::NotYet("load sample"))
    }
}

/// A cold store that answers what it was told, in order, and then the last answer for ever; an
/// `Err` entry is a store that cannot be asked.
pub struct ColdScript {
    answers: Mutex<VecDeque<Result<ColdState, ()>>>,
    last: Mutex<Option<ColdState>>,
    reads: std::sync::atomic::AtomicU32,
}

/// A reading of a store that is `healing` or not, taken at `read_unix_ms`.
pub fn cold_state(healing: bool, read_unix_ms: u64) -> ColdState {
    ColdState {
        healing,
        percent_done: healing.then_some(40.0),
        drives: vec![crate::result::DriveState {
            id: "d-00000000".to_owned(),
            state: "ok".to_owned(),
            healing,
        }],
        read_unix_ms,
        source: Some(crate::result::ColdSource::Mc),
    }
}

impl ColdScript {
    /// A store that answers `answers` in turn; after the last it keeps giving that one.
    pub fn of(answers: Vec<Result<ColdState, ()>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            last: Mutex::new(None),
            reads: 0.into(),
        })
    }

    /// How many times it was asked.
    pub fn reads(&self) -> u32 {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ColdStateSource for ColdScript {
    fn read(&self) -> Result<Option<ColdState>, BenchError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(next) = answers.pop_front() {
            let next = next.map_err(|()| BenchError::NotYet("cold state"))?;
            *last = Some(next);
        }
        Ok(last.clone())
    }
}

/// Twin of `tests/dead_store/mod.rs`: keep the two identical.
///
/// A store address that gives no answer. Tests that need "a store that is not there" use this and
/// not a port bound and then closed: a closed port is handed to the next listener any other test
/// binds on this machine, and that test's fake store then answers the requests meant for the
/// dead one (and counts them). Here the address stays bound for as long as the value lives, and
/// every connection made to it is closed at once, so a request to it ends without an answer.
pub struct DeadStore {
    url: String,
    accepting: tokio::task::JoinHandle<()>,
}

impl DeadStore {
    /// Binds a loopback address and holds it.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepting = tokio::spawn(async move {
            loop {
                // An accept error (no free descriptor under parallel tests) is skipped, so
                // the store keeps its address and keeps closing connections.
                let Ok(_conn) = listener.accept().await else {
                    tokio::task::yield_now().await;
                    continue;
                };
            }
        });
        Self { url, accepting }
    }

    /// The endpoint, as a store spec names it.
    pub fn endpoint(&self) -> &str {
        &self.url
    }
}

impl Drop for DeadStore {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::DeadStore;

    /// Catches a dead store whose port is free to be taken by another test's listener, which then
    /// answers for it: the next listener is asked for the very address.
    #[tokio::test]
    async fn the_address_of_a_dead_store_cannot_be_taken_by_another_listener() {
        let dead = DeadStore::start().await;
        let addr = dead.endpoint().trim_start_matches("http://").to_owned();
        let intruder = tokio::net::TcpListener::bind(&addr).await;
        assert!(intruder.is_err(), "another listener took {addr}");
    }

    /// Catches a dead store that answers: a request to it gets no HTTP answer.
    #[tokio::test]
    async fn a_dead_store_gives_no_answer() {
        let dead = DeadStore::start().await;
        let got = reqwest::Client::new().get(dead.endpoint()).send().await;
        assert!(got.is_err());
    }
}

/// A store spec for a loopback service.
pub fn spec(endpoint: &str) -> StoreSpec {
    StoreSpec {
        endpoint: endpoint.to_owned(),
        bucket: "kbf-fake".to_owned(),
        region: "us-east-1".to_owned(),
        access_key_id: "fake-key".to_owned(),
        secret_access_key: Secret::new("fake-secret"),
    }
}

/// The byte caps of the tests: generous, so a test that is about a cap sets its own.
pub const CAPS: ByteCaps = ByteCaps {
    campaign: 1 << 40,
    pass: 1 << 38,
    cell: 1 << 39,
};

/// A context over `stores` (role and endpoint), results in `dir`, in `window`, with the request
/// log in the directory. The rate caps are high, so a test does not wait for them.
pub fn ctx(dir: &Path, stores: &[(Role, &str)], window: Window) -> CellContext {
    std::fs::create_dir_all(dir).unwrap();
    let log = Arc::new(RequestLog::create(&dir.join("requests.jsonl")).unwrap());
    ctx_with_log(dir, stores, window, log)
}

/// A writer that takes `n` writes and then refuses, as a disk that fills up does.
pub struct FailAfter(pub usize);

impl std::io::Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
            return Err(std::io::Error::other("disk full"));
        }
        self.0 -= 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`ctx`] with the request log given.
pub fn ctx_with_log(
    dir: &Path,
    stores: &[(Role, &str)],
    window: Window,
    log: Arc<RequestLog>,
) -> CellContext {
    let config = RunConfig {
        result_dir: dir.to_owned(),
        test_prefix: "kbf-".to_owned(),
        stores: stores
            .iter()
            .map(|(role, endpoint)| (*role, spec(endpoint)))
            .collect::<BTreeMap<_, _>>(),
        window,
        seed: 1,
        gate: LoadGate {
            max_loadavg_1m: 8.0,
            max_psi_cpu_avg10: 10.0,
            max_psi_io_avg10: 10.0,
            max_disk_util_pct: 60.0,
        },
        cold_guard: crate::cells::coldguard::ColdGuardLimits::default(),
        cold_state_timeout: std::time::Duration::from_secs(30),
        cold_state_max_age: std::time::Duration::from_secs(30),
        rates: RateCaps {
            cold_rps: 100_000,
            hot_rps: 100_000,
            list_poll_rps: 100_000,
        },
        bytes: CAPS,
    };
    // The campaign of a test lives beside the results (`ledger.json`, which the tests read), and
    // is begun once: a second context over the same directory continues it.
    let ledger = dir.join("ledger.json");
    if !ledger.exists() {
        Ledger::create(&ledger, CAPS).unwrap();
    }
    CellContext {
        config,
        log,
        campaign: Campaign { ledger },
        cold_state: Arc::new(NoColdState),
        provenance: Provenance {
            run_id: "test-run".to_owned(),
            ..Provenance::unrecorded()
        },
    }
}

/// The request log lines of a test directory, parsed.
pub fn requests(dir: &Path) -> Vec<crate::requestlog::RequestRecord> {
    std::fs::read_to_string(dir.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A fault proxy that keeps the modes it was told and counts what it is told to have seen.
#[derive(Default)]
pub struct SwitchLog {
    modes: Mutex<Vec<crate::proxy::FaultMode>>,
    count: std::sync::atomic::AtomicU64,
}

impl SwitchLog {
    /// The modes set so far, in order.
    pub fn modes(&self) -> Vec<crate::proxy::FaultMode> {
        self.modes.lock().unwrap().clone()
    }
}

impl crate::cells::engine::FaultSwitch for SwitchLog {
    fn set_mode(&self, mode: crate::proxy::FaultMode) -> Result<(), BenchError> {
        self.modes.lock().unwrap().push(mode);
        Ok(())
    }

    /// Five more than the last time it was asked, so a pass that asks before and after sees five.
    fn request_count(&self) -> u64 {
        self.count.fetch_add(5, std::sync::atomic::Ordering::SeqCst)
    }
}
