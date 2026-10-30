//! What a run may spend: a rate cap on requests and a ledger of bytes written to the cold store.
//!
//! The rate cap ([`Throttle`]) is a token bucket of one request at a time: the harness holds it
//! whatever a cell asks for. The ledger ([`Ledger`]) keeps the bytes written so far in
//! `ledger.json` of the result directory, so a campaign of many runs shares one cap; a cell asks
//! it before it starts and is refused when the budget left is less than the cell's own cost.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::ByteCaps;
use crate::error::BenchError;

/// A cap on the requests per second of one store role. Cloned `Arc`s of it share one budget.
#[derive(Debug)]
pub struct Throttle {
    gap: Duration,
    next: Mutex<Instant>,
}

impl Throttle {
    /// A cap of `per_sec` requests per second (zero is taken as one).
    #[must_use]
    pub fn new(per_sec: u32) -> Self {
        Self {
            gap: Duration::from_secs(1) / per_sec.max(1),
            next: Mutex::new(Instant::now()),
        }
    }

    /// Waits until this request may be sent. Requests are served in the order they ask, one
    /// `gap` apart, so a burst is spread and the rate over any span is at most the cap.
    pub async fn acquire(&self) {
        let at = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            let at = (*next).max(Instant::now());
            *next = at + self.gap;
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

/// The file the ledger is kept in.
const LEDGER_FILE: &str = "ledger.json";

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    cold_written_bytes: u64,
}

/// Bytes written to the cold store by the campaign so far, against the caps.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    caps: ByteCaps,
    spent: u64,
}

impl Ledger {
    /// The ledger of `dir`: what an earlier run wrote there, or nothing.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file exists and cannot be read, [`BenchError::Result`] if it is
    /// not a ledger.
    pub fn open(dir: &Path, caps: ByteCaps) -> Result<Self, BenchError> {
        let path = dir.join(LEDGER_FILE);
        let spent = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str::<LedgerFile>(&text)
                    .map_err(|e| BenchError::Result(format!("ledger: {e}")))?
                    .cold_written_bytes
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(source) => return Err(BenchError::Io { path, source }),
        };
        Ok(Self { path, caps, spent })
    }

    /// Bytes written so far.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Refuses a cell whose largest pass is over the pass cap, whose passes together are over the
    /// cell cap, or that the campaign cap does not leave room for.
    ///
    /// # Errors
    /// [`BenchError::Budget`], naming the cap and the numbers.
    pub fn admit(&self, pass_cost: u64, cell_cost: u64) -> Result<(), BenchError> {
        let left = self.caps.campaign.saturating_sub(self.spent);
        let over = [
            ("pass", pass_cost, self.caps.pass),
            ("cell", cell_cost, self.caps.cell),
            ("campaign (what is left of it)", cell_cost, left),
        ]
        .into_iter()
        .find(|&(_, cost, cap)| cost > cap);
        match over {
            None => Ok(()),
            Some((which, cost, cap)) => Err(BenchError::Budget(format!(
                "the {which} cap is {cap} bytes and the cell needs {cost}"
            ))),
        }
    }

    /// Adds `bytes` to what was written and saves the ledger.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file cannot be written.
    pub fn charge(&mut self, bytes: u64) -> Result<(), BenchError> {
        self.spent = self.spent.saturating_add(bytes);
        let text = serde_json::to_string(&LedgerFile {
            cold_written_bytes: self.spent,
        })
        .expect("a ledger serialises");
        std::fs::write(&self.path, text).map_err(|source| BenchError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;

    use super::*;

    const CAPS: ByteCaps = ByteCaps {
        campaign: 100,
        pass: 10,
        cell: 30,
    };

    /// Catches a rate cap that lets a burst through: five requests asked at once leave four
    /// gaps of one tenth of a second between them, on the paused clock.
    #[tokio::test(start_paused = true)]
    async fn a_burst_is_spread_one_gap_apart() {
        let throttle = Throttle::new(10);
        let start = Instant::now();
        for _ in 0..5 {
            throttle.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::from_millis(400));
    }

    /// Catches a cap of zero dividing by zero, and a cap that waits when the requests are
    /// already far enough apart.
    #[tokio::test(start_paused = true)]
    async fn a_zero_cap_is_one_and_a_slow_caller_is_not_delayed() {
        let throttle = Throttle::new(0);
        throttle.acquire().await;
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after_sleep = Instant::now();
        throttle.acquire().await;
        assert_eq!(Instant::now(), after_sleep);
        assert!(start.elapsed() >= Duration::from_secs(5));
    }

    /// Catches a ledger that forgets a charge across runs, and one that starts from nothing
    /// when a file is there.
    #[test]
    fn a_charge_is_kept_for_the_next_run() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-ledger-keep");
        let mut ledger = Ledger::open(dir.path(), CAPS).unwrap();
        assert_eq!(ledger.spent(), 0);
        ledger.charge(7).unwrap();
        ledger.charge(5).unwrap();
        assert_eq!(Ledger::open(dir.path(), CAPS).unwrap().spent(), 12);
    }

    /// Catches each cap not being enforced, with the equality of cost and cap allowed, and the
    /// campaign cap measured against what is left rather than the whole.
    #[test]
    fn each_cap_refuses_what_it_is_over() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-ledger-caps");
        let mut ledger = Ledger::open(dir.path(), CAPS).unwrap();
        assert!(ledger.admit(10, 30).is_ok());
        let pass = ledger.admit(11, 11).unwrap_err().to_string();
        assert!(pass.contains("pass cap is 10"), "{pass}");
        let cell = ledger.admit(10, 31).unwrap_err().to_string();
        assert!(cell.contains("cell cap is 30"), "{cell}");
        ledger.charge(80).unwrap();
        assert!(ledger.admit(10, 20).is_ok());
        let campaign = ledger.admit(10, 21).unwrap_err().to_string();
        assert!(
            campaign.contains("is 20 bytes and the cell needs 21"),
            "{campaign}"
        );
        ledger.charge(1000).unwrap();
        assert!(ledger.admit(1, 1).is_err());
    }

    /// Catches a ledger file that is not read as one (a silent restart from zero would lift the
    /// cap), and a path that cannot be read at all.
    #[test]
    fn a_damaged_or_unreadable_ledger_stops_the_run() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-ledger-bad");
        std::fs::write(dir.path().join(LEDGER_FILE), "not json").unwrap();
        assert!(matches!(
            Ledger::open(dir.path(), CAPS),
            Err(BenchError::Result(_))
        ));
        let other = TestDir::under(std::env::temp_dir(), "tb-ledger-dir");
        std::fs::create_dir(other.path().join(LEDGER_FILE)).unwrap();
        assert!(matches!(
            Ledger::open(other.path(), CAPS),
            Err(BenchError::Io { .. })
        ));
        let missing = other.path().join("absent");
        let mut unwritable = Ledger {
            path: missing.join("ledger.json"),
            caps: CAPS,
            spent: 0,
        };
        assert!(matches!(unwritable.charge(1), Err(BenchError::Io { .. })));
    }
}
