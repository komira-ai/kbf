//! What a run may spend: a rate cap on requests and a ledger of bytes written to the cold store.
//!
//! The rate cap ([`Throttle`]) is a token bucket of one request at a time: the harness holds it
//! whatever a cell asks for. The ledger ([`Ledger`]) keeps the bytes written so far in the file
//! `--campaign-ledger` names, outside every result directory, so a campaign of many runs shares
//! one cap and no cleaning of a result directory resets it. A cell asks it before it starts and is
//! refused when the budget left is less than the cell's own cost.
//!
//! The ledger errs on the side of too much. Bytes are charged *before* they are sent (a pass
//! reserves what it plans to write, [`Ledger::reserve`]) and the charge is corrected to what was
//! written once the pass ends ([`Ledger::settle`]); a run that dies in the middle leaves the
//! reservation counted. Every save replaces the file atomically ([`crate::atomic`]). A ledger is
//! made only by `--new-campaign` ([`Campaign::begin`]); a missing one is a refusal, never a fresh
//! start, and `--new-campaign` over a ledger that exists is a refusal too. A run holds the
//! ledger's lock file ([`LedgerLock`]) from [`Campaign::begin`] to its end, so two runs never
//! write one ledger.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::atomic;
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
    /// The ledger of a campaign that has begun: the file at `path`.
    ///
    /// # Errors
    /// [`BenchError::Budget`] if there is no file (a campaign is begun with `--new-campaign`,
    /// not by a path that is wrong), [`BenchError::Io`] if it cannot be read,
    /// [`BenchError::Result`] if it is not a ledger.
    pub fn open(path: &Path, caps: ByteCaps) -> Result<Self, BenchError> {
        let spent = match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str::<LedgerFile>(&text)
                    .map_err(|e| BenchError::Result(format!("ledger: {e}")))?
                    .cold_written_bytes
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BenchError::Budget(format!(
                    "there is no campaign ledger at {}; --new-campaign begins one",
                    path.display()
                )));
            }
            Err(source) => {
                return Err(BenchError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Ok(Self {
            path: path.to_owned(),
            caps,
            spent,
        })
    }

    /// Begins a campaign: makes the ledger at `path`, with nothing written.
    ///
    /// # Errors
    /// [`BenchError::Budget`] if a file is already there (`--new-campaign` never resets a
    /// campaign), [`BenchError::Io`] if it cannot be made.
    pub fn create(path: &Path, caps: ByteCaps) -> Result<Self, BenchError> {
        let ledger = Self {
            path: path.to_owned(),
            caps,
            spent: 0,
        };
        let io = |source| BenchError::Io {
            path: path.to_owned(),
            source,
        };
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(BenchError::Budget(format!(
                    "the campaign ledger at {} exists; --new-campaign would reset it",
                    path.display()
                )));
            }
            Err(e) => return Err(io(e)),
        };
        file.write_all(ledger.text().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(io)?;
        Ok(ledger)
    }

    /// The file's text for what is spent now.
    fn text(&self) -> String {
        serde_json::to_string(&LedgerFile {
            cold_written_bytes: self.spent,
        })
        .expect("a ledger serialises")
    }

    /// Saves the ledger, replacing the file whole.
    fn save(&self) -> Result<(), BenchError> {
        atomic::write(&self.path, self.text().as_bytes()).map_err(|source| BenchError::Io {
            path: self.path.clone(),
            source,
        })
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
        self.save()
    }

    /// Counts `planned` bytes as written, before any is sent, and saves. [`Self::settle`] corrects
    /// the count afterwards; if the run dies first the reservation stays.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file cannot be written.
    pub fn reserve(&mut self, planned: u64) -> Result<(), BenchError> {
        self.charge(planned)
    }

    /// Replaces a reservation of `planned` bytes by what was written, `actual` (less, or more),
    /// and saves.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file cannot be written.
    pub fn settle(&mut self, planned: u64, actual: u64) -> Result<(), BenchError> {
        self.spent = self.spent.saturating_sub(planned).saturating_add(actual);
        self.save()
    }
}

/// The campaign a run belongs to: where its ledger is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Campaign {
    /// The ledger file.
    pub ledger: PathBuf,
}

/// The lock file of the ledger file `ledger`: its name and `.lock`, beside it.
#[must_use]
pub fn lock_path(ledger: &Path) -> PathBuf {
    let mut name = ledger.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    ledger.with_file_name(name)
}

/// The exclusive hold of one run on a ledger: the file `<ledger>.lock`, made with `create_new`
/// and holding the process id, removed when the hold is dropped. Two runs that read the ledger,
/// charge it and write it back would lose one another's charges, so a second run is refused
/// while the first lives. A lock that a dead run left is never broken by the tool (that could
/// not be told from a live run on another host): the refusal names the file, and removing it is
/// the operator's step.
#[derive(Debug)]
pub struct LedgerLock {
    path: PathBuf,
}

impl LedgerLock {
    /// Takes the lock of the ledger file `ledger` (already resolved).
    fn acquire(ledger: &Path) -> Result<Self, BenchError> {
        let path = lock_path(ledger);
        let io = |source| BenchError::Io {
            path: path.clone(),
            source,
        };
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                return Err(BenchError::Budget(format!(
                    "the campaign ledger is in use: the lock file {} exists (process {}). If no \
                     run is using the ledger, check that the process is gone and delete the lock \
                     file by hand",
                    path.display(),
                    holder.trim()
                )));
            }
            Err(e) => return Err(io(e)),
        };
        // The process id only tells the operator who held the lock; the file is the lock.
        write!(file, "{}", std::process::id()).ok();
        Ok(Self { path })
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        // Nothing to do about a failure here; the lock file is then a stale one.
        std::fs::remove_file(&self.path).ok();
    }
}

/// `ledger` with its directory (and itself, if it is there) resolved through links and `..`.
fn resolve_ledger(ledger: &Path) -> Result<PathBuf, BenchError> {
    let name = ledger.file_name().ok_or_else(|| {
        BenchError::Budget(format!(
            "the campaign ledger path {} names no file",
            ledger.display()
        ))
    })?;
    let parent = ledger
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize().map_err(|source| BenchError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let file = parent.join(name);
    Ok(file.canonicalize().unwrap_or(file))
}

/// `dir` made absolute, with its longest existing part resolved through links and `..` (the
/// result directory may not exist yet).
fn resolve_dir(dir: &Path) -> PathBuf {
    let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
    abs.ancestors()
        .find_map(|a| a.canonicalize().ok().map(|real| (a, real)))
        .map_or(abs.clone(), |(a, real)| {
            real.join(abs.strip_prefix(a).unwrap_or(Path::new("")))
        })
}

impl Campaign {
    /// Begins (`new`) or continues the campaign whose ledger is `ledger`, for a run whose results
    /// go to `result_dir`, and takes the ledger's lock, which the caller holds for the whole run.
    /// The ledger must lie outside `result_dir` once both are resolved through links and `..`
    /// (the ledger's directory must exist), so that clearing a result directory does not start
    /// the count again.
    ///
    /// # Errors
    /// [`BenchError::Budget`] for a ledger inside the result directory, a ledger in use by
    /// another run (its lock file exists), a missing one without `new`, an existing one with it;
    /// [`BenchError::Io`], [`BenchError::Result`] as [`Ledger::open`] and [`Ledger::create`] say.
    pub fn begin(
        ledger: &Path,
        new: bool,
        result_dir: &Path,
        caps: ByteCaps,
    ) -> Result<(Self, LedgerLock), BenchError> {
        let resolved = resolve_ledger(ledger)?;
        if resolved.starts_with(resolve_dir(result_dir)) {
            return Err(BenchError::Budget(
                "the campaign ledger must be outside the result directory".to_owned(),
            ));
        }
        let lock = LedgerLock::acquire(&resolved)?;
        if new {
            Ledger::create(&resolved, caps)?;
        } else {
            Ledger::open(&resolved, caps)?;
        }
        Ok((Self { ledger: resolved }, lock))
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

    /// A new ledger in a scratch directory, with its path.
    fn fresh(name: &str) -> (TestDir, PathBuf, Ledger) {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let path = dir.path().join("ledger.json");
        let ledger = Ledger::create(&path, CAPS).unwrap();
        (dir, path, ledger)
    }

    /// Catches a ledger that forgets a charge across runs, and one that starts from nothing
    /// when a file is there.
    #[test]
    fn a_charge_is_kept_for_the_next_run() {
        let (_dir, path, mut ledger) = fresh("tb-ledger-keep");
        assert_eq!(ledger.spent(), 0);
        ledger.charge(7).unwrap();
        ledger.charge(5).unwrap();
        assert_eq!(Ledger::open(&path, CAPS).unwrap().spent(), 12);
    }

    /// Catches a missing ledger being taken for an empty one (a lost file would lift the
    /// campaign cap), and a ledger made over one that is there (a flag left in a command line
    /// would reset the count).
    #[test]
    fn a_missing_ledger_is_refused_and_an_existing_one_is_never_remade() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-ledger-missing");
        let path = dir.path().join("ledger.json");
        let err = Ledger::open(&path, CAPS).unwrap_err();
        assert!(matches!(err, BenchError::Budget(_)), "{err}");
        assert!(err.to_string().contains("--new-campaign"), "{err}");
        assert!(!path.exists());

        let mut ledger = Ledger::create(&path, CAPS).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"cold_written_bytes\":0}"
        );
        ledger.charge(9).unwrap();
        let err = Ledger::create(&path, CAPS).unwrap_err();
        assert!(matches!(err, BenchError::Budget(_)), "{err}");
        assert_eq!(Ledger::open(&path, CAPS).unwrap().spent(), 9);

        let nowhere = dir.path().join("absent").join("ledger.json");
        assert!(matches!(
            Ledger::create(&nowhere, CAPS),
            Err(BenchError::Io { .. })
        ));
    }

    /// Catches bytes counted only after they are sent: a reservation is in the file before
    /// anything else happens, a pass that wrote less is corrected down, one that wrote more is
    /// corrected up, and a ledger never goes below what other passes charged.
    #[test]
    fn a_reservation_is_in_the_file_at_once_and_settles_to_what_was_written() {
        let (_dir, path, mut ledger) = fresh("tb-ledger-reserve");
        ledger.charge(5).unwrap();
        ledger.reserve(30).unwrap();
        assert_eq!(Ledger::open(&path, CAPS).unwrap().spent(), 35);
        ledger.settle(30, 10).unwrap();
        assert_eq!(Ledger::open(&path, CAPS).unwrap().spent(), 15);
        ledger.reserve(10).unwrap();
        ledger.settle(10, 25).unwrap();
        assert_eq!(Ledger::open(&path, CAPS).unwrap().spent(), 40);
        ledger.settle(1000, 0).unwrap();
        assert_eq!(ledger.spent(), 0, "a settle cannot go below zero");
    }

    /// Catches each cap not being enforced, with the equality of cost and cap allowed, and the
    /// campaign cap measured against what is left rather than the whole.
    #[test]
    fn each_cap_refuses_what_it_is_over() {
        let (_dir, _path, mut ledger) = fresh("tb-ledger-caps");
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
        let damaged = dir.path().join("ledger.json");
        std::fs::write(&damaged, "not json").unwrap();
        assert!(matches!(
            Ledger::open(&damaged, CAPS),
            Err(BenchError::Result(_))
        ));
        let other = TestDir::under(std::env::temp_dir(), "tb-ledger-dir");
        let a_directory = other.path().join("ledger.json");
        std::fs::create_dir(&a_directory).unwrap();
        assert!(matches!(
            Ledger::open(&a_directory, CAPS),
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

    /// Catches a campaign begun the wrong way: a ledger inside the result directory (cleaning
    /// the directory would reset the count), no ledger without `--new-campaign`, a ledger made
    /// over one that exists, and the plain cases that work. An empty path has no file name and
    /// is refused.
    #[test]
    fn a_campaign_begins_with_a_new_ledger_outside_the_results_and_continues_with_it() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-campaign");
        let results = dir.path().join("results");
        let ledger = dir.path().join("campaign").join("ledger.json");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&results).unwrap();
        let real = ledger.parent().unwrap().canonicalize().unwrap();
        let real = real.join("ledger.json");

        let inside = results.join("ledger.json");
        let err = Campaign::begin(&inside, true, &results, CAPS).unwrap_err();
        assert!(
            err.to_string().contains("outside the result directory"),
            "{err}"
        );
        assert!(!inside.exists());

        let err = Campaign::begin(&ledger, false, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("no campaign ledger"), "{err}");
        assert!(!ledger.exists());
        assert!(!real.with_file_name("ledger.json.lock").exists());

        let (campaign, lock) = Campaign::begin(&ledger, true, &results, CAPS).unwrap();
        assert_eq!(campaign.ledger, real);
        assert!(ledger.is_file());
        drop(lock);
        let err = Campaign::begin(&ledger, true, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("would reset it"), "{err}");
        assert_eq!(
            Campaign::begin(&ledger, false, &results, CAPS).unwrap().0,
            campaign
        );

        let err = Campaign::begin(Path::new(""), false, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("names no file"), "{err}");
        let err = Campaign::begin(Path::new("/"), false, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("names no file"), "{err}");
        let err = Campaign::begin(
            &dir.path().join("none").join("l.json"),
            true,
            &results,
            CAPS,
        )
        .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }

    /// Catches an inside-the-results check that is only lexical: a ledger reached through a
    /// link to the result directory, and one reached through `..` (both are inside, and
    /// cleaning the result directory would take them), the same with the result directory given
    /// by a link or not made yet, and a ledger beside the result directory (not inside, so
    /// refusing it would be a different defect).
    #[cfg(unix)]
    #[test]
    fn a_ledger_that_resolves_into_the_results_is_refused() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-campaign-resolve");
        let results = dir.path().join("results");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&results, &link).unwrap();

        let through_link = link.join("ledger.json");
        let err = Campaign::begin(&through_link, true, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("outside the result"), "{err}");
        let through_dots = other.join("..").join("results").join("ledger.json");
        let err = Campaign::begin(&through_dots, true, &results, CAPS).unwrap_err();
        assert!(err.to_string().contains("outside the result"), "{err}");
        let err = Campaign::begin(&results.join("ledger.json"), true, &link, CAPS).unwrap_err();
        assert!(err.to_string().contains("outside the result"), "{err}");
        assert!(!results.join("ledger.json").exists());
        assert!(
            !results.join("ledger.json.lock").exists(),
            "a refusal locks nothing"
        );
        let err = Campaign::begin(&other.join("l.json"), true, Path::new(""), CAPS).unwrap_err();
        assert!(matches!(err, BenchError::Budget(_)), "{err}");

        let not_yet = link.join("not-yet").join("deeper");
        assert!(Campaign::begin(&other.join("l.json"), true, &not_yet, CAPS).is_ok());
        assert!(Campaign::begin(&results.join("m.json"), true, &other, CAPS).is_ok());
    }

    /// Catches two runs sharing a ledger (each would write back its own total and lose the
    /// other's charges): a second `begin` while the first run's hold lives is refused with the
    /// lock file named, nothing of the ledger changes, and the hold is released when it is
    /// dropped, also when `begin` fails after taking it.
    #[test]
    fn a_ledger_in_use_refuses_a_second_run_and_is_free_again_afterwards() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-campaign-lock");
        let results = dir.path().join("results");
        let ledger = dir.path().join("ledger.json");
        let lock_file = dir.path().join("ledger.json.lock");

        let (_, first) = Campaign::begin(&ledger, true, &results, CAPS).unwrap();
        assert_eq!(first.path().file_name(), lock_file.file_name());
        assert_eq!(
            std::fs::read_to_string(&lock_file).unwrap(),
            std::process::id().to_string()
        );
        Ledger::open(&ledger, CAPS).unwrap().charge(3).unwrap();
        for new in [false, true] {
            let err = Campaign::begin(&ledger, new, &results, CAPS).unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, BenchError::Budget(_)), "{text}");
            assert!(text.contains("ledger.json.lock"), "{text}");
            assert!(text.contains(&std::process::id().to_string()), "{text}");
        }
        assert_eq!(Ledger::open(&ledger, CAPS).unwrap().spent(), 3);
        assert!(lock_file.exists(), "a refused run leaves the holder's lock");

        drop(first);
        assert!(!lock_file.exists());
        // `begin` fails after taking the lock (new over an existing ledger): the lock goes too.
        Campaign::begin(&ledger, true, &results, CAPS).unwrap_err();
        assert!(!lock_file.exists());
        let (_, second) = Campaign::begin(&ledger, false, &results, CAPS).unwrap();
        assert!(lock_file.exists());
        drop(second);
        assert!(!lock_file.exists());
    }

    /// Catches a stale lock being broken by the tool (it cannot tell a dead run from one on
    /// another host): the run is refused, the message says how to clear it, the file stays; once
    /// the operator deletes it, the run goes. A lock file that cannot be made at all (a name over
    /// the file-name limit) is an I/O error, not a refusal.
    #[test]
    fn a_stale_lock_is_never_broken_and_the_refusal_says_how_to_clear_it() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-campaign-stale");
        let results = dir.path().join("results");
        let ledger = dir.path().join("ledger.json");
        Ledger::create(&ledger, CAPS).unwrap();
        let lock_file = dir.path().join("ledger.json.lock");
        std::fs::write(&lock_file, "4000000\n").unwrap();

        let err = Campaign::begin(&ledger, false, &results, CAPS).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("ledger.json.lock"), "{text}");
        assert!(text.contains("4000000"), "{text}");
        assert!(text.contains("delete the lock file"), "{text}");
        assert_eq!(std::fs::read_to_string(&lock_file).unwrap(), "4000000\n");

        std::fs::remove_file(&lock_file).unwrap();
        assert!(Campaign::begin(&ledger, false, &results, CAPS).is_ok());

        let long = dir.path().join("a".repeat(255));
        let err = Campaign::begin(&long, true, &results, CAPS).unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }
}
