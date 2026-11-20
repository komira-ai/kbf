//! One run of the tool: the checks, the passes, the files it writes and the exit status.

use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser as _;
use kbf_front::action_tree::Sleeper;
use kbf_front::large_backfill::LargeStore;
use kbf_objstore::s3::S3Store;
use kbf_pack_index::postgres::PostgresPackIndex;
use kbf_pack_index::{PackIndex, PackIndexError};
use kbf_state::postgres::PostgresConfig;

use crate::audit;
use crate::backfill::{self, Settings};
use crate::cli::{Args, AuditPlan, Plan};
use crate::error::Error;
use crate::files;
use crate::pace::{Clock, Pace, SystemClock, TokioSleeper};
use crate::report::{self, Audit, Backfill, Report};
use crate::store::Bucket;

/// How the pack index is opened from the `--pack-index` URL.
pub type OpenIndex = dyn Fn(&PostgresConfig) -> Result<Arc<dyn PackIndex>, PackIndexError>;

/// How the store the lists and objects are read from and the sidecars written to is made of
/// the one the flags describe.
pub type OpenStore = dyn Fn(S3Store) -> Arc<dyn LargeStore>;

/// The places a test reaches into.
pub struct Seams {
    /// Opens the pack index ([`open_postgres`]).
    pub open_index: Box<OpenIndex>,
    /// Makes the engine's store of the flags' store ([`open_bucket`]).
    pub open_store: Box<OpenStore>,
    /// The clock that spaces the trees.
    pub clock: Arc<dyn Clock>,
    /// What waits out the spacing.
    pub sleeper: Arc<dyn Sleeper>,
    /// The seed of an audit sample that `--audit-seed` does not give.
    pub seed: Box<dyn Fn() -> u64>,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            open_index: Box::new(open_postgres),
            open_store: Box::new(open_bucket),
            clock: Arc::new(SystemClock::new()),
            sleeper: Arc::new(TokioSleeper),
            seed: Box::new(clock_seed),
        }
    }
}

/// Opens the pack index on the Postgres state store `config` names, with connections of its
/// own. It creates nothing: the installer owns the tables.
///
/// # Errors
/// [`PackIndexError`]: the server cannot be reached, or the schema lacks the tables.
pub fn open_postgres(config: &PostgresConfig) -> Result<Arc<dyn PackIndex>, PackIndexError> {
    let index = PostgresPackIndex::open(config)?;
    Ok(Arc::new(index))
}

/// The engine's store over the S3 store of the flags.
#[must_use]
pub fn open_bucket(store: S3Store) -> Arc<dyn LargeStore> {
    Arc::new(Bucket::new(Arc::new(store)))
}

/// A seed from the system clock: the nanoseconds since the epoch, folded to 64 bits. It is the
/// only place the clock is read for the sample, and the report says the seed.
fn clock_seed() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = since.as_nanos();
    (nanos ^ (nanos >> 64)) as u64
}

/// Runs the tool on a command line: prints the report to `out` and any error to `err`, and
/// returns the exit status (0 done, 2 refused or a usage error, 1 failed).
pub fn main_with(argv: Vec<OsString>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    main_with_seams(argv, &Seams::default(), out, err)
}

/// [`main_with`] with the places a test reaches into passed in.
pub fn main_with_seams(
    argv: Vec<OsString>,
    seams: &Seams,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let args = match Args::try_parse_from(argv) {
        Ok(args) => args,
        Err(usage) => return usage_exit(&usage, out, err),
    };
    match run(&args, seams, out) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "kbf-l2-backfill: {error}");
            error.code()
        }
    }
}

/// Help goes to `out` and ends the run well; a usage error goes to `err` and refuses it.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let text = usage.render().to_string();
    let failed = usage.use_stderr();
    let _ = if failed {
        err.write_all(text.as_bytes())
    } else {
        out.write_all(text.as_bytes())
    };
    u8::from(failed) * 2
}

/// The checks and the passes.
///
/// In order: the flags and the credentials file; the pack index; the backfill pass (if the run
/// has one); the sample (if one is asked for); the report, printed to `out` and, with
/// `--report-json`, written to its file. The report is printed and written whether the passes
/// ended well or not.
///
/// # Errors
/// [`Error::Refused`] when a precondition does not hold (nothing was changed);
/// [`Error::Failed`] when a call fails, a file cannot be written, or any tree failed: a pass's
/// own failure comes before a report file that could not be written, that before a report that
/// could not be printed, and that before the trees that failed.
pub fn run(args: &Args, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    let plan = args.plan()?;
    let backfills = plan.backfills();
    let Plan {
        store,
        index: index_config,
        mode,
        within,
        within_days,
        rate,
        limit_trees,
        report_json,
        audit,
    } = plan;
    let opened = (seams.open_index)(&index_config);
    let index = opened.map_err(|error| Error::Failed(error.to_string()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut report = Report {
        mode,
        backfill: None,
        audit: None,
    };
    let parts = Parts {
        store,
        within_days,
        backfill: backfills.then(|| Settings {
            mode,
            within,
            pace: Pace::new(rate),
            limit: limit_trees,
        }),
        audit,
    };
    let ended = runtime.block_on(passes(parts, &index, seams, &mut report));
    let printed = out.write_all(report::render(&report).as_bytes());
    let written = match &report_json {
        Some(path) => files::write_json(path, &report),
        None => Ok(()),
    };
    ended?;
    written?;
    printed?;
    let failed = report.backfill.as_ref().map_or(0, |b| b.failed);
    if failed > 0 {
        return Err(Error::failed(format!(
            "{failed} trees failed; their unlisted rows are kept (see the report)"
        )));
    }
    Ok(())
}

/// The backfill and the sample, in that order.
async fn passes(
    parts: Parts,
    index: &Arc<dyn PackIndex>,
    seams: &Seams,
    report: &mut Report,
) -> Result<(), Error> {
    if let Some(settings) = parts.backfill {
        let store = (seams.open_store)(parts.store);
        let mut pass = Backfill {
            within_days: parts.within_days,
            ..Backfill::default()
        };
        let done = backfill::run(
            settings,
            index,
            &store,
            seams.clock.as_ref(),
            seams.sleeper.as_ref(),
            &mut pass,
        )
        .await;
        report.backfill = Some(pass);
        done?;
    }
    if let Some(plan) = parts.audit {
        report.audit = Some(sample(&plan, index.as_ref(), seams)?);
    }
    Ok(())
}

/// Takes the sample and writes its file.
fn sample(plan: &AuditPlan, index: &dyn PackIndex, seams: &Seams) -> Result<Audit, Error> {
    let seed = plan.seed.unwrap_or_else(|| (seams.seed)());
    let swept = audit::sweep(index, seed, plan.keys)
        .map_err(|error| Error::failed(format!("sweeping the live packs: {error}")))?;
    let text = audit::file_text(&swept.keys);
    files::write_atomic(&plan.out, text.as_bytes())?;
    Ok(Audit {
        seed,
        requested: plan.keys.get(),
        live_packs: swept.live_packs,
        written: swept.keys.len(),
    })
}

/// The parts of a [`Plan`] the passes consume.
struct Parts {
    store: S3Store,
    within_days: u32,
    backfill: Option<Settings>,
    audit: Option<AuditPlan>,
}
