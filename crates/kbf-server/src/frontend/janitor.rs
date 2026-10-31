//! The janitor of `cas/b/`: the flags `--task-janitor-key-file` and its companions, and the
//! [`Janitor`] they open, which sweeps the task packs nothing recorded.
//!
//! A task writes its packs (`cas/b/<lease>/<nonce>-<k>`) and large objects (`...-L<i>`) through
//! presigned writes. A pack the ingest records stays where it lies and the index holds it. What
//! no one holds is an orphan: a run that was refused or failed after its write, the keys of a
//! key set the task did not use, and a large object the ingest has copied into the CAS. The
//! janitor deletes those, and nothing else.
//!
//! **What is an orphan.** A key under `cas/b/` that
//!
//! 1. says its age (`kbf_pack::task_key_age`: the minute it was minted in, from a `-<k>` or
//!    `-L<i>` name) and is older than `--task-janitor-grace-secs`, which is at least the 4 hours
//!    a presigned write is good for and an hour more for the ingest, so a key is never deleted
//!    while a task can still write it or the ingest still read it (the flag's default is 6
//!    hours);
//! 2. is not under verification: every key of the key set an ingest is working on is held in the
//!    packs' [`kbf_front::VerifyingKeys`] until the ingest ends; and
//! 3. is not recorded in the pack index, asked again for that one key just before its delete.
//!
//! **The invariant that makes a delete safe.** The janitor takes a claim on the key in the same
//! registry the ingests hold keys in, after the index's last answer for the page and held through
//! the index's answer for that key and the delete. A claim and a hold on one key exclude each
//! other under one lock: an ingest that has begun makes the janitor keep the key, and an ingest
//! that begins after the claim is refused (`Claimed`) before it reads anything. So the index is
//! never told of a pack whose object is being deleted. An ingest also refuses a key as old as
//! `kbf_pack::MIN_GRACE`, the least grace the flag takes, so an over-age key (a run seen as ended
//! only after a long outage) is never committed. The leadership is asked before each page and each
//! delete, and a sweep that finds it lost stops (`abandoned`). A 403 on a delete ends the
//! sweep at the first key (a key that may not delete may not delete any), and so do a number of
//! deletes failing in a row; both are counted in `sweep_errors` and logged once, and what the sweep
//! did before it stopped is counted too. A 401 (a bad credential) ends it as a 403 does.
//!
//! **Across processes.** Holds and claims are per process (a lock in the packs). A fenced old
//! leader that is still ingesting while the janitor runs on the new leader, or two frontends whose
//! clocks differ, are protected by the age rule alone: the ingest refuses a key at
//! `kbf_pack::MIN_GRACE` and the janitor deletes at the grace, so the margin is
//! `grace - MIN_GRACE`, an hour at least (`--task-janitor-grace-secs` takes nothing under
//! `MIN_GRACE` plus that hour, 6 hours).
//!
//! A key whose name the parser does not understand is kept and counted (`kept_unparsed`).
//!
//! **Only `cas/b/`.** The listing is for the prefix `cas/b/`, each key it returns is checked to
//! be under it ([`kbf_pack::Sweeper`]), and the key file is the tier store's GC key, whose prefix
//! must be exactly `cas/b/`, so the store itself refuses a list or a delete anywhere else:
//! `cas/l/` (content-bound large objects), `cas/f/<writer>/` (frontend packs) and `cas/d/` are
//! never reached.
//!
//! **The key file** is the JSON the tier store reads for `--gc-key-file`,
//! `{"access_key_id": "...", "secret_access_key": "...", "prefix": "cas/b/"}`, with the same
//! owner and mode rules as the other key files (the secret is never a flag). The store is
//! the pack store of `--pack-store-endpoint` (same endpoint, bucket and region), which the
//! janitor needs, and so does the pack index.
//!
//! **The pace.** A sweep lists `--task-janitor-page-size` keys at a time and waits
//! `--task-janitor-page-pause-ms` after each page, so it deletes at most that many keys in that
//! long; a sweep starts every `--task-janitor-interval-secs`, on the leader only (the server
//! that holds the scheduler role). `--task-janitor-dry-run` counts and logs what a sweep would
//! delete and deletes nothing.
//!
//! Off unless `--task-janitor-key-file` is given.

use std::error::Error;
use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use clap::{Arg, ArgMatches};
use futures::future::BoxFuture;
use kbf_front::{Packs, VerifyingKeys};
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, ObjectStore, PageSize};
use kbf_pack::{
    SWEPT_PREFIX, SweepConfig, SweepError, SweepGates, SweepReport, Sweeper, task_key_age,
};
use kbf_pack_index::PackIndex;
use zeroize::Zeroizing;

use super::pack_flags::PackFlags;
use crate::health::Readiness;
use crate::token::read_owner_only;

const KEY_FILE: &str = "task_janitor_key_file";
const INTERVAL: &str = "task_janitor_interval_secs";
const GRACE: &str = "task_janitor_grace_secs";
const PAGE_SIZE: &str = "task_janitor_page_size";
const PAGE_PAUSE: &str = "task_janitor_page_pause_ms";
const DRY_RUN: &str = "task_janitor_dry_run";
const SECRET: &str = "task_secret_file";

/// The margin above the age an ingest refuses a key at that the grace must have: holds and
/// claims are per process, so across frontends (two hosts with clocks that differ, or a fenced
/// old leader still ingesting while the new one sweeps) a key is safe only by the age rule, and
/// this is its margin: an hour.
const FENCE_MARGIN_SECS: u64 = 3_600;

/// The least `--task-janitor-grace-secs`: the age an ingest refuses a key at
/// ([`kbf_pack::MIN_GRACE`], the one constant both sides are built from) and the margin above it.
const MIN_GRACE_SECS: u64 = kbf_pack::MIN_GRACE.as_secs() + FENCE_MARGIN_SECS;

/// The help of `--task-janitor-grace-secs`, its bounds from the constants.
fn grace_help() -> String {
    format!(
        "How old a key must be, in seconds, before the janitor may delete it, by the minute its \
         name says it was minted in. At least {MIN_GRACE_SECS} (the {ingest} seconds at which an \
         ingest refuses a key, and an hour more as a margin for hosts with different clocks and \
         for an old leader still ingesting) and at most {MAX_GRACE_SECS}. Used only with \
         `--task-janitor-key-file`",
        ingest = kbf_pack::MIN_GRACE.as_secs(),
    )
}

/// The most `--task-janitor-grace-secs`: a week.
const MAX_GRACE_SECS: u64 = 7 * 24 * 3_600;

/// The most `--task-janitor-interval-secs`: a day.
const MAX_INTERVAL_SECS: u64 = 24 * 3_600;

/// The most `--task-janitor-page-pause-ms`: a minute.
const MAX_PAUSE_MS: u64 = 60_000;

/// The interval when `--task-janitor-interval-secs` is left out: 15 minutes (test-only: clap
/// takes the default as a literal, and the default-flags test checks it against this).
#[cfg(test)]
const DEFAULT_INTERVAL_SECS: u64 = 900;

/// The page when `--task-janitor-page-size` is left out (test-only, as above).
#[cfg(test)]
const DEFAULT_PAGE_SIZE: u16 = 500;

/// The pause when `--task-janitor-page-pause-ms` is left out: a second, which with the page makes
/// 500 keys a second at most (test-only, as above).
#[cfg(test)]
const DEFAULT_PAGE_PAUSE_MS: u64 = 1_000;

/// The most bytes the key file may hold.
const MAX_FILE_BYTES: usize = 4096;

/// What a key file that is not the JSON of a GC key is told.
const NOT_KEY: &str = "not JSON of exactly access_key_id, secret_access_key and prefix";

/// The six flags.
pub(super) fn args() -> [Arg; 6] {
    [
        Arg::new(KEY_FILE)
            .long("task-janitor-key-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(SECRET)
            .help(
                "Turns on the janitor of `cas/b/`: it deletes the task packs and large objects \
                 nothing recorded (a refused run, an unused key, a large object already copied \
                 into the CAS) once they are older than `--task-janitor-grace-secs`, are not \
                 under verification and are not in the pack index. The file is the tier \
                 store's GC key, `{\"access_key_id\": \"...\", \"secret_access_key\": \"...\", \
                 \"prefix\": \"cas/b/\"}` (the prefix must be exactly `cas/b/`), owned by this \
                 user and readable by it alone (mode 0600 or 0400); it is used against \
                 `--pack-store-endpoint`'s store, which this needs. The janitor never lists or \
                 deletes under `cas/l/`, `cas/f/` or `cas/d/`. Runs on the leader only. Needs \
                 `--task-secret-file` and `--pack-store-endpoint`. Left out, nothing is deleted",
            ),
        Arg::new(INTERVAL)
            .long("task-janitor-interval-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_INTERVAL_SECS))
            // DEFAULT_INTERVAL_SECS, which clap cannot take as a computed string; the
            // default-flags test parses it back and compares it with the constant.
            .default_value("900")
            .help(
                "How often, in seconds, the janitor starts a sweep of `cas/b/`, at least 1 and \
                 at most 86400. Used only with `--task-janitor-key-file`",
            ),
        Arg::new(GRACE)
            .long("task-janitor-grace-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(MIN_GRACE_SECS..=MAX_GRACE_SECS))
            // The default grace of `kbf_pack`, 6 hours, as above.
            .default_value("21600")
            .help(grace_help()),
        Arg::new(PAGE_SIZE)
            .long("task-janitor-page-size")
            .value_name("KEYS")
            .value_parser(clap::value_parser!(u16).range(1..=1000))
            // DEFAULT_PAGE_SIZE, as above.
            .default_value("500")
            .help(
                "The keys of `cas/b/` the janitor lists at a time, 1 to 1000, and so the keys it \
                 checks against the pack index in one question. Used only with \
                 `--task-janitor-key-file`",
            ),
        Arg::new(PAGE_PAUSE)
            .long("task-janitor-page-pause-ms")
            .value_name("MILLISECONDS")
            .value_parser(clap::value_parser!(u64).range(0..=MAX_PAUSE_MS))
            // DEFAULT_PAGE_PAUSE_MS, as above.
            .default_value("1000")
            .help(
                "How long, in milliseconds, the janitor waits after each page before it lists \
                 the next, at most 60000: with `--task-janitor-page-size`, the most keys a \
                 second it lists and deletes. Used only with `--task-janitor-key-file`",
            ),
        Arg::new(DRY_RUN)
            .long("task-janitor-dry-run")
            .action(clap::ArgAction::SetTrue)
            .help(
                "Log and count the keys the janitor would delete, and delete none. Used only \
                 with `--task-janitor-key-file`",
            ),
    ]
}

/// What the janitor flags say, when `--task-janitor-key-file` is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JanitorFlags {
    /// `--task-janitor-key-file`.
    pub key_file: PathBuf,
    /// `--task-janitor-interval-secs`.
    pub interval: Duration,
    /// `--task-janitor-grace-secs`.
    pub grace: Duration,
    /// `--task-janitor-page-size`.
    pub page: PageSize,
    /// `--task-janitor-page-pause-ms`.
    pub pause: Duration,
    /// `--task-janitor-dry-run`.
    pub dry_run: bool,
}

/// The setting the flags in `m` make: `None` unless `--task-janitor-key-file` is given.
pub(super) fn from_matches(m: &ArgMatches) -> Option<JanitorFlags> {
    let key_file = m.get_one::<PathBuf>(KEY_FILE)?;
    // The others have a default, and clap holds them to their ranges.
    let secs = |id: &str| Duration::from_secs(m.get_one::<u64>(id).copied().unwrap_or_default());
    let page = m.get_one::<u16>(PAGE_SIZE).copied().unwrap_or_default();
    let pause = m.get_one::<u64>(PAGE_PAUSE).copied().unwrap_or_default();
    Some(JanitorFlags {
        key_file: key_file.clone(),
        interval: secs(INTERVAL),
        grace: secs(GRACE),
        page: PageSize::new(page).unwrap_or(PageSize::MAX),
        pause: Duration::from_millis(pause),
        dry_run: m.get_flag(DRY_RUN),
    })
}

/// The janitor needs the packs: its store is theirs, and the keys it deletes are those the
/// ingest through them leaves.
///
/// # Errors
/// The janitor is on and `--pack-store-endpoint` is not given.
pub(super) fn check(
    janitor: Option<&JanitorFlags>,
    packs: Option<&PackFlags>,
) -> Result<(), String> {
    match (janitor, packs) {
        (Some(_), None) => Err("--task-janitor-key-file needs --pack-store-endpoint (and \
             --pack-index): the janitor lists and deletes in the pack store, and asks the \
             pack index which keys it holds"
            .to_owned()),
        _ => Ok(()),
    }
}

/// Why the janitor cannot be opened. No message carries the secret.
#[derive(Debug, thiserror::Error)]
pub enum JanitorFlagError {
    /// The key file cannot be used.
    #[error("--task-janitor-key-file {path}: {why}")]
    Key {
        /// The file.
        path: String,
        /// What is wrong with it.
        why: String,
    },
    /// The pack store's endpoint or bucket is not one the store client takes.
    #[error("--pack-store-endpoint (for --task-janitor-key-file): {0}")]
    Store(#[from] kbf_objstore::s3::S3ConfigError),
}

/// What the key file holds: exactly the three fields of the tier store's `--gc-key-file`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    prefix: String,
}

/// The credentials in the file at `path`, whose prefix must be exactly [`SWEPT_PREFIX`].
fn read_key(path: &Path) -> Result<Credentials, JanitorFlagError> {
    let bad = |why: String| JanitorFlagError::Key {
        path: path.display().to_string(),
        why,
    };
    let bytes = read_owner_only(path, MAX_FILE_BYTES).map_err(|error| bad(error.to_string()))?;
    let bytes = Zeroizing::new(bytes);
    // The error of the parser is dropped: for a value of the wrong type it quotes the value.
    let key = serde_json::from_slice::<KeyFile>(&bytes).map_err(|_| bad(NOT_KEY.to_owned()))?;
    if key.prefix != SWEPT_PREFIX {
        return Err(bad(format!(
            "the prefix is {:?} and must be exactly {SWEPT_PREFIX:?}: the janitor sweeps \
             nothing else",
            key.prefix
        )));
    }
    if key.access_key_id.is_empty() || key.secret_access_key.is_empty() {
        return Err(bad(NOT_KEY.to_owned()));
    }
    Ok(Credentials::new(key.access_key_id, key.secret_access_key))
}

/// What the sweeps counted since the janitor started. Every family is
/// `kbf_cas_b_janitor_...`; each one's `# HELP` line says what it counts.
#[derive(Debug, Default)]
pub struct JanitorMetrics {
    sweeps: AtomicU64,
    sweep_errors: AtomicU64,
    listed: AtomicU64,
    kept_young: AtomicU64,
    kept_verifying: AtomicU64,
    kept_referenced: AtomicU64,
    kept_unparsed: AtomicU64,
    kept_outside: AtomicU64,
    reaped: AtomicU64,
    would_reap: AtomicU64,
    reap_errors: AtomicU64,
    abandoned: AtomicU64,
    done: Done,
}

/// The count of sweeps the janitor has finished, led or not, whole or stopped by an error,
/// that a caller can wait on.
#[derive(Debug)]
struct Done(tokio::sync::watch::Sender<u64>);

impl Default for Done {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(0).0)
    }
}

impl JanitorMetrics {
    /// Counts what one sweep did.
    fn record(&self, report: &SweepReport, ended: bool) {
        let add = |counter: &AtomicU64, n: usize| {
            counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        };
        add(&self.sweeps, usize::from(report.led && ended));
        add(&self.listed, report.listed);
        add(&self.kept_young, report.young);
        add(&self.kept_verifying, report.verifying);
        add(&self.kept_referenced, report.referenced);
        add(&self.kept_unparsed, report.strays.len());
        add(&self.kept_outside, report.outside);
        add(&self.reaped, report.deleted);
        add(&self.would_reap, report.would_delete);
        add(&self.reap_errors, report.delete_failed);
        add(&self.abandoned, usize::from(report.abandoned));
    }

    /// Tells the waiters a sweep is finished.
    fn finished_one(&self) {
        self.done.0.send_modify(|finished| *finished += 1);
    }

    /// The count of sweeps finished so far, told as each one ends: a caller waits for a sweep by
    /// this event and not by time.
    #[must_use]
    pub fn finished(&self) -> tokio::sync::watch::Receiver<u64> {
        self.done.0.subscribe()
    }

    /// The sweeps that led and ran to the end of the listing.
    #[must_use]
    pub fn sweeps(&self) -> u64 {
        self.sweeps.load(Ordering::Relaxed)
    }

    /// The count under `family` (the name after `kbf_cas_b_janitor_`, without `_total`), for the
    /// tests.
    #[cfg(test)]
    fn get(&self, family: &str) -> u64 {
        let counter = match family {
            "sweeps" => &self.sweeps,
            "sweep_errors" => &self.sweep_errors,
            "listed" => &self.listed,
            "kept_young" => &self.kept_young,
            "kept_verifying" => &self.kept_verifying,
            "kept_referenced" => &self.kept_referenced,
            "kept_unparsed" => &self.kept_unparsed,
            "kept_outside" => &self.kept_outside,
            "abandoned" => &self.abandoned,
            "reaped" => &self.reaped,
            "would_reap" => &self.would_reap,
            _ => &self.reap_errors,
        };
        counter.load(Ordering::Relaxed)
    }

    /// Appends the families to `out` in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let counters = [
            (
                "sweeps",
                "Sweeps of cas/b/ that this frontend led and that ended without an error, those \
                 stopped part way by losing leadership included",
                &self.sweeps,
            ),
            (
                "sweep_errors",
                "Sweeps stopped by an error: the listing or the pack index failed, a delete was \
                 refused with 401 or 403, or deletes failed several times in a row",
                &self.sweep_errors,
            ),
            (
                "listed",
                "Keys the listings of cas/b/ returned",
                &self.listed,
            ),
            (
                "kept_young",
                "Keys kept because they are younger than the grace",
                &self.kept_young,
            ),
            (
                "kept_verifying",
                "Keys kept because an ingest was verifying them",
                &self.kept_verifying,
            ),
            (
                "kept_referenced",
                "Keys kept because the pack index records a pack under them",
                &self.kept_referenced,
            ),
            (
                "kept_unparsed",
                "Keys kept because their names do not say when they were minted",
                &self.kept_unparsed,
            ),
            (
                "kept_outside",
                "Keys a listing returned that are not under cas/b/, kept",
                &self.kept_outside,
            ),
            (
                "abandoned",
                "Sweeps stopped part way because this frontend stopped leading",
                &self.abandoned,
            ),
            ("reaped", "Orphan keys deleted", &self.reaped),
            (
                "would_reap",
                "Orphan keys a dry run would have deleted",
                &self.would_reap,
            ),
            (
                "reap_errors",
                "Deletes of an orphan key the store refused",
                &self.reap_errors,
            ),
        ];
        for (name, help, counter) in counters {
            let name = format!("kbf_cas_b_janitor_{name}_total");
            let value = counter.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value}"
            );
        }
    }
}

/// The clock the sweeps judge ages by.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// One sweep, over whatever store the janitor was built on.
type Pass = Box<dyn Fn(SystemTime) -> BoxFuture<'static, Swept> + Send + Sync>;

/// What a sweep did, and the error that stopped it, if one did: the report is whole either way.
type Swept = (SweepReport, Result<(), SweepError>);

/// Sweeps `cas/b/` on an interval: see the module docs.
pub struct Janitor {
    pass: Pass,
    interval: Duration,
    clock: Clock,
    metrics: Arc<JanitorMetrics>,
}

impl Janitor {
    /// A janitor over `store` and `index` that sweeps with `config` every `interval`, only while
    /// `is_leader` says so, never deleting a key `verifying` holds, judging ages by `clock`.
    pub fn new<S: ObjectStore>(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        config: SweepConfig,
        interval: Duration,
        verifying: Arc<VerifyingKeys>,
        is_leader: Box<dyn Fn() -> bool + Send + Sync>,
        clock: Clock,
    ) -> Self {
        let gates = SweepGates {
            age_of: Box::new(task_key_age),
            is_leader,
            verifying: Box::new({
                let verifying = Arc::clone(&verifying);
                move |key| verifying.is_held(key)
            }),
            // Atomic with the ingests' holds: see `kbf_front::VerifyingKeys`.
            claim: Box::new(move |key| {
                verifying
                    .claim(key)
                    .map(|claim| Box::new(claim) as kbf_pack::Claim)
            }),
        };
        let sweeper = Arc::new(Sweeper::new(store, index, config, gates));
        let pass: Pass = Box::new(move |now| {
            let sweeper = Arc::clone(&sweeper);
            Box::pin(async move {
                let mut report = SweepReport::default();
                let ended = sweeper.sweep_into(now, &mut report).await;
                (report, ended)
            })
        });
        Self {
            pass,
            interval,
            clock,
            metrics: Arc::default(),
        }
    }

    /// What the sweeps count.
    #[must_use]
    pub fn metrics(&self) -> Arc<JanitorMetrics> {
        Arc::clone(&self.metrics)
    }

    /// One sweep, judged at the time the clock says now.
    pub async fn sweep_once(&self) {
        // What the sweep did before an error stopped it is counted too.
        let (report, ended) = (self.pass)((self.clock)()).await;
        self.metrics.record(&report, ended.is_ok());
        if let Err(error) = ended {
            tracing::warn!(%error, "the sweep of cas/b/ stopped early");
            self.metrics.sweep_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.metrics.finished_one();
    }

    /// Sweeps now and then every interval, for ever. A sweep that runs past an interval delays
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

impl JanitorFlags {
    /// The janitor these flags describe, over the pack store `store` names with the GC key of
    /// the key file, asking `index`, never deleting what `packs`' ingests are verifying, and
    /// sweeping only while `readiness` says this server is the leader. Nothing is sent.
    ///
    /// # Errors
    /// [`JanitorFlagError`]: the key file is missing, not private, not the JSON of a GC key or of
    /// another prefix than `cas/b/`; or the pack store's endpoint or bucket is refused.
    pub fn open(
        &self,
        store: &PackFlags,
        packs: &Packs,
        index: Arc<dyn PackIndex>,
        readiness: Arc<Readiness>,
    ) -> Result<Janitor, JanitorFlagError> {
        let credentials = read_key(&self.key_file)?;
        let store = S3Store::new(S3Config {
            endpoint: store.endpoint.clone(),
            region: store.region.clone(),
            bucket: store.bucket.clone(),
            credentials,
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: NonZeroU64::new(1 << 20),
        })?;
        let config = SweepConfig {
            grace: self.grace,
            page: self.page,
            dry_run: self.dry_run,
            pause: self.pause,
            ..SweepConfig::default()
        };
        Ok(Janitor::new(
            Arc::new(store),
            index,
            config,
            self.interval,
            packs.verifying_keys(),
            Box::new(move || readiness.is_leader()),
            Arc::new(SystemTime::now),
        ))
    }
}

/// The janitor the flags ask for, if they ask for one.
///
/// # Errors
/// The janitor is on and there are no packs or no pack index, or as [`JanitorFlags::open`].
pub(super) fn open(
    flags: Option<&JanitorFlags>,
    pack_flags: Option<&PackFlags>,
    packs: Option<&Packs>,
    index: Option<&Arc<dyn PackIndex>>,
    readiness: &Arc<Readiness>,
) -> Result<Option<Janitor>, Box<dyn Error>> {
    let Some(flags) = flags else {
        return Ok(None);
    };
    let (Some(pack_flags), Some(packs), Some(index)) = (pack_flags, packs, index) else {
        return Err("--task-janitor-key-file needs --pack-store-endpoint and --pack-index".into());
    };
    let janitor = flags.open(pack_flags, packs, Arc::clone(index), Arc::clone(readiness))?;
    Ok(Some(janitor))
}

#[cfg(test)]
mod tests;
