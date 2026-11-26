//! The collector of tree lists: the flags `--executor-list-sweep-key-file` and its companions,
//! and the [`ListSweep`] they open, which deletes the tree lists no job needs any more
//! (docs/design/one-file-action-tree.md, 4.8 and item 1e of section 6).
//!
//! In the provider form (`--executor-input-tree provider`) and for every Mac job, the frontend
//! writes each action's tree as one tree list, `cas/l/<id>`, and its L2 sidecar, `cas/l2/<id>`
//! (`<id>` the SHA-256 of the list). A list is the action's own: unlike the manifests of the
//! manifest form, lists do not dedup across trees, so every start adds one, and nothing else
//! deletes under `cas/l/`.
//!
//! **What is deleted.** A pass lists the sidecars, `cas/l2/`, with the time the store says each
//! was written, and asks the store for the size and the time of the list each names (a HEAD of
//! `cas/l/<id>`). A list is deleted, and then its sidecar, when
//!
//! 1. it is older than a day ([`MIN_AGE`]) by the time the store says it was written: a list is
//!    needed while its jobs can still start, which is the frontend's 600 s memo of a tree and
//!    the later attempts of its operation;
//! 2. no open operation's row names it ([`crate::executor::rows::OpRow::tree`], read just
//!    before the page's deletes): an operation whose job is queued or running, or will be
//!    submitted again, keeps its list whatever its age; and
//! 3. the pack index records no pack under its key: an object with the bytes of a list that is
//!    also a large blob of the CAS is kept.
//!
//! **A list taken up after the rows were read.** A list is written create-only, so the store's
//! time is its first write's: a list an identical tree takes up again a day later is old though
//! in use, and an operation that takes it up (from the 600 s memo, or a write that found it
//! there) after a pass read the rows, but before the pass's delete, finds it deleted. The job's
//! start is then refused by the host, `no tree list at <key>`, which the executor reads as a
//! refused list (`crate::executor`'s outcome): its miss path writes the list again and the run
//! spends no attempt.
//!
//! A sidecar whose list is gone is deleted once the sidecar is older than a day and no open
//! operation's row names its list. A sidecar key
//! that names no list id is kept and counted. A list with no sidecar is never listed, so never
//! deleted (the L2 backfill gives a list recorded unlisted its sidecar, and a later pass then
//! sees it). Only `cas/l/<id>` named by a sidecar and `cas/l2/` are ever deleted: the
//! manifests, large blobs and whole objects that share `cas/l/` are not listed, and a key of
//! `cas/l/` is deleted only as the list of a sidecar.
//!
//! **What stops a pass.** A row of the operations that cannot be read (corrupt, or of a later
//! release) stops the pass before the page's deletes, as does a pack index that fails: what a
//! row names, or what the index records, is then not known. A 401 or 403 ends the pass at once,
//! and so do five store calls failing in a row; a HEAD or a delete that fails otherwise keeps
//! its list and is counted. The list is deleted before its sidecar, so a sidecar delete that
//! fails leaves a sidecar of a list that is gone, which a later pass deletes. The leadership is
//! asked before each page and each delete; a pass that finds it lost stops (`abandoned`).
//!
//! **A dry run by default.** Without `--executor-list-sweep-apply` a pass deletes nothing: it
//! counts what it would delete, and the lists and their bytes by age, on `GET /metrics`
//! (`kbf_tree_list_sweep_*`) and in its log line.
//!
//! **The key files** are two tier store GC keys, the JSON the tier store reads for
//! `--gc-key-file`, `{"access_key_id": "...", "secret_access_key": "...", "prefix": "..."}`: a
//! GC key lists, heads and deletes under its one prefix and reads nothing. The list key's
//! prefix must be exactly `cas/l/`, the sidecar key's exactly `cas/l2/`. Both are used against
//! `--pack-store-endpoint`'s store (same endpoint, bucket and region), which the collector
//! needs, as it needs `--pack-index` and a state store (the operations' rows).
//!
//! **The pace.** A pass lists 1000 sidecars at a time, makes one HEAD after another, and waits a
//! second after each page; a pass starts every `--executor-list-sweep-interval-secs` (an hour by
//! default), on the leader only.
//!
//! Off unless `--executor-list-sweep-key-file` is given.

mod pass;

use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use clap::{Arg, ArgMatches};
use kbf_objstore::Capabilities;
use kbf_pack_index::PackIndex;

use self::pass::{
    AGE_DAYS, Aged, LIST_PREFIX, Pass, PassConfig, PassReport, SIDECAR_PREFIX, Stores,
};
use super::janitor::{Clock, read_gc_key};
use super::pack_flags::PackFlags;
use crate::executor::rows::Rows;
use crate::health::Readiness;

const KEY_FILE: &str = "executor_list_sweep_key_file";
const SIDECAR_KEY_FILE: &str = "executor_list_sweep_sidecar_key_file";
const APPLY: &str = "executor_list_sweep_apply";
const INTERVAL: &str = "executor_list_sweep_interval_secs";

/// The least `--executor-list-sweep-interval-secs`: a minute.
const MIN_INTERVAL_SECS: u64 = 60;

/// The most `--executor-list-sweep-interval-secs`: a day.
const MAX_INTERVAL_SECS: u64 = 86_400;

/// The interval when `--executor-list-sweep-interval-secs` is left out: an hour (test-only:
/// clap takes the default as a literal, and the default-flags test checks it against this).
#[cfg(test)]
const DEFAULT_INTERVAL_SECS: u64 = 3_600;

/// What the collector is called in a refusal of a key file.
const SWEEPER: &str = "the collector of tree lists";

/// The four flags.
pub(super) fn args() -> [Arg; 4] {
    [
        Arg::new(KEY_FILE)
            .long("executor-list-sweep-key-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(SIDECAR_KEY_FILE)
            .help(
                "Turns on the collector of tree lists: it deletes the tree lists `cas/l/<id>` \
                 (and their sidecars `cas/l2/<id>`) older than a day that no open operation \
                 names and the pack index does not record; without \
                 `--executor-list-sweep-apply` it only counts them. The file is the tier \
                 store's GC key of `cas/l/`, `{\"access_key_id\": \"...\", \
                 \"secret_access_key\": \"...\", \"prefix\": \"cas/l/\"}` (the prefix must be \
                 exactly `cas/l/`), owned by this user and readable by it alone (mode 0600 or \
                 0400); it is used against `--pack-store-endpoint`'s store, which this needs, \
                 as it needs `--pack-index` and a state store. Only a list a sidecar names is \
                 deleted, never another object of `cas/l/`. Runs on the leader only. Needs \
                 `--executor-list-sweep-sidecar-key-file`. Left out, nothing is deleted",
            ),
        Arg::new(SIDECAR_KEY_FILE)
            .long("executor-list-sweep-sidecar-key-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(KEY_FILE)
            .help(
                "The tier store's GC key of `cas/l2/` for the collector of tree lists, as \
                 `--executor-list-sweep-key-file`'s but with the prefix exactly `cas/l2/`: \
                 the collector lists the sidecars and deletes them with it. Needs \
                 `--executor-list-sweep-key-file`",
            ),
        Arg::new(APPLY)
            .long("executor-list-sweep-apply")
            .action(clap::ArgAction::SetTrue)
            .requires(KEY_FILE)
            .help(
                "Delete what the collector of tree lists finds. Left out, each pass is a dry \
                 run: it counts and logs what it would delete, and deletes nothing. Needs \
                 `--executor-list-sweep-key-file`",
            ),
        Arg::new(INTERVAL)
            .long("executor-list-sweep-interval-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS))
            // DEFAULT_INTERVAL_SECS, which clap cannot take as a computed string; the
            // default-flags test parses it back and compares it with the constant.
            .default_value("3600")
            .help(
                "How often, in seconds, the collector of tree lists starts a pass, at least 60 \
                 and at most 86400. Used only with `--executor-list-sweep-key-file`",
            ),
    ]
}

/// What the collector's flags say, when `--executor-list-sweep-key-file` is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListSweepFlags {
    /// `--executor-list-sweep-key-file`.
    pub key_file: PathBuf,
    /// `--executor-list-sweep-sidecar-key-file`.
    pub sidecar_key_file: PathBuf,
    /// `--executor-list-sweep-apply`.
    pub apply: bool,
    /// `--executor-list-sweep-interval-secs`.
    pub interval: Duration,
}

/// The setting the flags in `m` make: `None` unless `--executor-list-sweep-key-file` is given.
pub(super) fn from_matches(m: &ArgMatches) -> Option<ListSweepFlags> {
    let key_file = m.get_one::<PathBuf>(KEY_FILE)?;
    // clap makes the two key files come together, and holds the interval to its range.
    let sidecar_key_file = m.get_one::<PathBuf>(SIDECAR_KEY_FILE).cloned();
    let interval = m.get_one::<u64>(INTERVAL).copied().unwrap_or_default();
    Some(ListSweepFlags {
        key_file: key_file.clone(),
        sidecar_key_file: sidecar_key_file.unwrap_or_default(),
        apply: m.get_flag(APPLY),
        interval: Duration::from_secs(interval),
    })
}

/// The collector needs the packs' store: the lists are in it.
///
/// # Errors
/// The collector is on and `--pack-store-endpoint` is not given.
pub(super) fn check(
    sweep: Option<&ListSweepFlags>,
    packs: Option<&PackFlags>,
) -> Result<(), String> {
    match (sweep, packs) {
        (Some(_), None) => Err(
            "--executor-list-sweep-key-file needs --pack-store-endpoint \
             (and --pack-index and a state store): the collector lists and deletes the tree \
             lists in the pack store"
                .to_owned(),
        ),
        _ => Ok(()),
    }
}

/// Why the collector cannot be opened. No message carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum ListSweepFlagError {
    /// A key file cannot be used.
    #[error("{flag} {path}: {why}")]
    Key {
        /// The flag that names the file.
        flag: &'static str,
        /// The file.
        path: String,
        /// What is wrong with it.
        why: String,
    },
    /// The pack store's endpoint or bucket is not one the store client takes.
    #[error("--pack-store-endpoint (for --executor-list-sweep-key-file): {0}")]
    Store(#[from] kbf_objstore::s3::S3ConfigError),
}

impl ListSweepFlags {
    /// The collector these flags describe, over the pack store `store` names, signed with the
    /// two GC keys, asking `index` and the operations' rows `rows`, and running only while
    /// `readiness` says this server is the leader. Nothing is sent.
    ///
    /// # Errors
    /// [`ListSweepFlagError`]: a key file is missing, not private, not the JSON of a GC key or of
    /// another prefix than its own; or the pack store's endpoint or bucket is refused.
    pub fn open(
        &self,
        store: &PackFlags,
        index: Arc<dyn PackIndex>,
        rows: Arc<Rows>,
        readiness: Arc<Readiness>,
    ) -> Result<ListSweep, ListSweepFlagError> {
        let key = |flag: &'static str, path: &PathBuf, prefix: &str| {
            read_gc_key(path, prefix, SWEEPER).map_err(|why| ListSweepFlagError::Key {
                flag,
                path: path.display().to_string(),
                why,
            })
        };
        let lists = key(
            "--executor-list-sweep-key-file",
            &self.key_file,
            LIST_PREFIX,
        )?;
        let sidecars = key(
            "--executor-list-sweep-sidecar-key-file",
            &self.sidecar_key_file,
            SIDECAR_PREFIX,
        )?;
        let stores = Stores {
            lists: store.store(lists, Capabilities::default())?,
            sidecars: store.store(sidecars, Capabilities::default())?,
        };
        let config = PassConfig {
            apply: self.apply,
            ..PassConfig::default()
        };
        let is_leader = Box::new(move || readiness.is_leader());
        let pass = Pass::new(stores, index, rows, config, is_leader);
        Ok(ListSweep::new(
            pass,
            self.interval,
            Arc::new(SystemTime::now),
        ))
    }
}

/// The collector the flags ask for, if they ask for one.
///
/// # Errors
/// The collector is on and there are no pack flags, no pack index or no state store, or as
/// [`ListSweepFlags::open`].
pub(super) fn open(
    flags: Option<&ListSweepFlags>,
    pack_flags: Option<&PackFlags>,
    index: Option<&Arc<dyn PackIndex>>,
    state: Option<&Arc<dyn kbf_state::Store>>,
    readiness: &Arc<Readiness>,
) -> Result<Option<ListSweep>, Box<dyn Error>> {
    let Some(flags) = flags else {
        return Ok(None);
    };
    let (Some(pack_flags), Some(index), Some(state)) = (pack_flags, index, state) else {
        return Err(
            "--executor-list-sweep-key-file needs --pack-store-endpoint, --pack-index \
                    and a state store"
                .into(),
        );
    };
    let rows = Arc::new(Rows::new(Arc::clone(state), "list-sweep"));
    let sweep = flags.open(pack_flags, Arc::clone(index), rows, Arc::clone(readiness))?;
    Ok(Some(sweep))
}

/// The counters of every pass, and the lists by age of the last whole one. Every family is
/// `kbf_tree_list_sweep_...`; each one's `# HELP` line says what it counts.
#[derive(Debug)]
pub struct ListSweepMetrics {
    apply: bool,
    counters: [AtomicU64; COUNTERS.len()],
    /// The lists and bytes by age of the last pass that reached the end of the listing.
    by_day: Mutex<[Aged; AGE_DAYS]>,
    done: tokio::sync::watch::Sender<u64>,
}

/// The counters: name (after `kbf_tree_list_sweep_`, before `_total`) and help.
const COUNTERS: [(&str, &str); 15] = [
    (
        "passes",
        "Passes this frontend led that reached the end of the listing of cas/l2/",
    ),
    (
        "pass_errors",
        "Passes stopped by an error: a listing, the pack index or the operations' rows failed, \
         a call was refused with 401 or 403, or store calls failed several times in a row",
    ),
    (
        "abandoned",
        "Passes stopped part way because this frontend stopped leading",
    ),
    (
        "sidecars_listed",
        "Sidecars the listings of cas/l2/ returned",
    ),
    (
        "kept_young",
        "Lists, or sidecars whose list is gone, kept because they are a day old or younger",
    ),
    (
        "kept_named",
        "Old lists kept because an open operation's row names them",
    ),
    (
        "kept_recorded",
        "Old lists kept because the pack index records a pack under their key",
    ),
    (
        "kept_unparsed",
        "Sidecar keys kept because they name no list id",
    ),
    ("deleted_lists", "Tree lists deleted"),
    ("deleted_list_bytes", "Bytes of the tree lists deleted"),
    ("deleted_sidecars", "Sidecars deleted"),
    (
        "would_delete_lists",
        "Tree lists a dry run would have deleted",
    ),
    (
        "would_delete_list_bytes",
        "Bytes of the tree lists a dry run would have deleted",
    ),
    (
        "would_delete_sidecars",
        "Sidecars a dry run would have deleted",
    ),
    (
        "store_errors",
        "HEADs of a list and deletes the store failed; the list is kept",
    ),
];

impl ListSweepMetrics {
    fn new(apply: bool) -> Self {
        Self {
            apply,
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            by_day: Mutex::new([Aged::default(); AGE_DAYS]),
            done: tokio::sync::watch::channel(0).0,
        }
    }

    /// Counts what one pass did, and how it ended.
    fn record(&self, report: &PassReport, ended: bool) {
        let whole = report.whole && ended;
        // In the order of [`COUNTERS`].
        let values = [
            u64::from(whole),
            u64::from(!ended),
            u64::from(report.abandoned),
            report.sidecars,
            report.young,
            report.named,
            report.recorded,
            report.unparsed,
            report.deleted_lists,
            report.deleted_bytes,
            report.deleted_sidecars,
            report.would_delete_lists,
            report.would_delete_bytes,
            report.would_delete_sidecars,
            report.failed,
        ];
        for (counter, n) in self.counters.iter().zip(values) {
            counter.fetch_add(n, Ordering::Relaxed);
        }
        if whole {
            *self.by_day.lock().unwrap_or_else(PoisonError::into_inner) = report.by_day;
        }
        self.done.send_modify(|finished| *finished += 1);
    }

    /// The count of passes finished so far (whole, stopped part way or by an error), told as
    /// each ends: a caller waits for a pass by this event and not by time.
    #[must_use]
    pub fn finished(&self) -> tokio::sync::watch::Receiver<u64> {
        self.done.subscribe()
    }

    /// The count under `family` (the name after `kbf_tree_list_sweep_`, without `_total`).
    #[must_use]
    pub fn get(&self, family: &str) -> u64 {
        COUNTERS
            .iter()
            .position(|(counter, _)| *counter == family)
            .map_or(0, |i| self.counters[i].load(Ordering::Relaxed))
    }

    /// Appends the families to `out` in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        for ((name, help), counter) in COUNTERS.iter().zip(&self.counters) {
            let name = format!("kbf_tree_list_sweep_{name}_total");
            let value = counter.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value}"
            );
        }
        let apply = "kbf_tree_list_sweep_apply";
        let _ = writeln!(
            out,
            "# HELP {apply} 1 when the passes delete (--executor-list-sweep-apply), 0 when \
             they are dry runs.\n# TYPE {apply} gauge\n{apply} {}",
            u64::from(self.apply)
        );
        let by_day = *self.by_day.lock().unwrap_or_else(PoisonError::into_inner);
        for (family, help, of) in [
            (
                "kbf_tree_list_sweep_lists",
                "Tree lists with a sidecar the last whole pass found, by their age in whole days \
                 (7+ is a week or more)",
                (|aged: &Aged| aged.lists) as fn(&Aged) -> u64,
            ),
            (
                "kbf_tree_list_sweep_list_bytes",
                "Bytes of those tree lists, by their age in whole days",
                |aged: &Aged| aged.bytes,
            ),
        ] {
            let _ = writeln!(out, "# HELP {family} {help}.\n# TYPE {family} gauge");
            for (day, aged) in by_day.iter().enumerate() {
                let label = if day + 1 == AGE_DAYS {
                    format!("{day}+")
                } else {
                    day.to_string()
                };
                let _ = writeln!(out, "{family}{{age_days=\"{label}\"}} {}", of(aged));
            }
        }
    }
}

/// Collects the tree lists on an interval: see the module docs.
pub struct ListSweep {
    pass: Pass,
    interval: Duration,
    clock: Clock,
    metrics: Arc<ListSweepMetrics>,
}

impl ListSweep {
    /// A collector that runs `pass` every `interval`, judging ages by `clock`.
    #[must_use]
    pub fn new(pass: Pass, interval: Duration, clock: Clock) -> Self {
        let metrics = Arc::new(ListSweepMetrics::new(pass.applies()));
        Self {
            pass,
            interval,
            clock,
            metrics,
        }
    }

    /// What the passes count.
    #[must_use]
    pub fn metrics(&self) -> Arc<ListSweepMetrics> {
        Arc::clone(&self.metrics)
    }

    /// One pass, judged at the time the clock says now. Returns its report.
    pub async fn sweep_once(&self) -> PassReport {
        let mut report = PassReport::default();
        // What the pass did before an error stopped it is counted too.
        let ended = self.pass.run((self.clock)(), &mut report).await;
        self.metrics.record(&report, ended.is_ok());
        let mode = if self.pass.applies() {
            "apply"
        } else {
            "dry run"
        };
        let [deleted, would] = [
            (report.deleted_lists, report.deleted_bytes),
            (report.would_delete_lists, report.would_delete_bytes),
        ];
        match &ended {
            Err(error) => {
                tracing::warn!(%error, mode, "the pass over the tree lists stopped early")
            }
            Ok(()) if report.led => tracing::info!(
                mode,
                whole = report.whole,
                sidecars = report.sidecars,
                young = report.young,
                named = report.named,
                recorded = report.recorded,
                deleted_lists = deleted.0,
                deleted_bytes = deleted.1,
                would_delete_lists = would.0,
                would_delete_bytes = would.1,
                failed = report.failed,
                "a pass over the tree lists ended"
            ),
            Ok(()) => {}
        }
        report
    }

    /// Sweeps now and then every interval, for ever. A pass that runs past an interval delays
    /// the next, and none is run to catch up.
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.sweep_once().await;
        }
    }
}

#[cfg(test)]
mod tests;
