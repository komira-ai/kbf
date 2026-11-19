//! `cargo xtask filer-presence-bench`: how long a SeaweedFS filer takes to say which of a
//! batch of objects it holds, through kbf's own client of its batch lookup
//! ([`kbf_objstore::filer::FilerPresence`]), and, when asked, how long a write through one
//! node takes to be seen by another node's filer. It is run by hand against a real filer;
//! CI runs only its tests, over a fake filer and a fake store.
//!
//! 1. **The present keys** are real objects: the files directly under `<filer-root>/<sample>`
//!    (kbf's large blobs, `cas/l`, by default), listed from the filer (`ListEntries`), as
//!    many as the largest batch needs, each asked with its listed length. **The absent
//!    keys** are the same names with `.absent` appended, in the same directory, so a present
//!    and an absent key cost the filer the same directory walk.
//! 2. **One row per batch size and present share** (`--keys`, `--present`): the first
//!    `keys * present / 100` keys of the row are present, the rest absent. Each row makes
//!    `--warmup` checks that are not timed, then `--rounds` that are. A check is one
//!    [`PresenceCheck::check`] of every key of the row: calls of at most 4,096 names, sent
//!    at once. Only that call is timed ([`measure`]): not the listing, nor the building or
//!    judging of the keys.
//! 3. **Every answer is judged.** A present key answered other than present, or an absent
//!    key other than absent, is counted `wrong`; a check that failed is counted `failed` and
//!    not timed. Either fails the verb, with the table printed: a number from a filer that
//!    answered wrongly measures nothing.
//! 4. **The replication lag** (with `--lag-store-endpoint` only; otherwise nothing is
//!    written anywhere): `--lag-rounds` times, an object of 4 KiB is written under
//!    `<lag-prefix><lag-nonce>/<n>` through the S3 endpoint given (one node's tierstore),
//!    and `--filer` (another node's) is asked for it every `--lag-poll-ms` until it answers
//!    it present ([`lag_once`]). The lag is the time from the write's acknowledgement to the
//!    answer that saw it, so it is at most one poll interval and one lookup late. A key the
//!    filer has not seen after `--lag-max-polls` looks gives no number and fails the verb.
//!    Each object is deleted after its round. The filer finds the objects of bucket `<b>`
//!    under `/buckets/<b>`, the S3 gateway's default.
//!
//! Percentiles are nearest-rank ([`percentile`]); times are printed in milliseconds.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use clap::Args as ClapArgs;
use kbf_objstore::filer::{
    Ask, FilerError, FilerPresence, FilerRoot, MAX_NAMES_PER_CALL, Presence, PresenceCheck,
};
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_proto::filer_pb::ListEntriesRequest;
use kbf_proto::filer_pb::seaweed_filer_client::SeaweedFilerClient;
use zeroize::Zeroizing;

use crate::Error;

/// The length of each object the lag rounds write.
pub const LAG_OBJECT_BYTES: usize = 4096;

/// What is appended to a listed name to make a key the filer does not hold.
pub const ABSENT_SUFFIX: &str = ".absent";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The filer's gRPC address, `<host>:<port>`: its HTTP port plus 10000 (18888 beside 8888).
    #[arg(long)]
    pub filer: String,
    /// The filer directory of the bucket asked about, `/buckets/<bucket>` behind the S3 gateway.
    #[arg(long)]
    pub filer_root: String,
    /// The directory under the root whose files are the present keys.
    #[arg(long, default_value = "cas/l")]
    pub sample: String,
    /// The batch sizes, in keys per check.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1,10,100,1000,4096,10000"
    )]
    pub keys: Vec<usize>,
    /// The shares of each batch's keys that are present, in percent.
    #[arg(long, value_delimiter = ',', default_value = "100,50,0")]
    pub present: Vec<u8>,
    /// The checks timed per row.
    #[arg(long, default_value_t = 200)]
    pub rounds: usize,
    /// The checks made, and not timed, before a row's first timed one.
    #[arg(long, default_value_t = 5)]
    pub warmup: usize,
    /// The most a call, or opening a connection, may take, in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    pub timeout_ms: u64,
    /// Measure the replication lag: the S3 endpoint (`http://<host>:<port>`) of the node the
    /// objects are written through. Left out, nothing is written.
    #[arg(long, requires_all = ["lag_store_bucket", "lag_store_credentials_file", "lag_nonce"])]
    pub lag_store_endpoint: Option<String>,
    /// The bucket the lag objects are written to.
    #[arg(long)]
    pub lag_store_bucket: Option<String>,
    /// The region the writes are signed for.
    #[arg(long, default_value = "us-east-1")]
    pub lag_store_region: String,
    /// A file with the key pair for `--lag-store-endpoint`, as JSON `{"access_key_id": "...",
    /// "secret_access_key": "..."}`, readable by its owner alone (the secret is never a flag).
    #[arg(long)]
    pub lag_store_credentials_file: Option<PathBuf>,
    /// The key prefix of the lag objects.
    #[arg(long, default_value = "kbf-bench/lag/")]
    pub lag_prefix: String,
    /// A text no earlier run used (A-Z a-z 0-9 . _ -): the lag objects are `<prefix><nonce>/<n>`.
    #[arg(long)]
    pub lag_nonce: Option<String>,
    /// The objects written and waited for.
    #[arg(long, default_value_t = 20)]
    pub lag_rounds: usize,
    /// The time between two looks for an object, in milliseconds.
    #[arg(long, default_value_t = 5)]
    pub lag_poll_ms: u64,
    /// The looks for one object before it counts as not seen.
    #[arg(long, default_value_t = 12_000)]
    pub lag_max_polls: usize,
}

/// One row of the table: a batch size and its present keys, and what its checks gave.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
    /// The keys of each check.
    pub keys: usize,
    /// How many of them are present.
    pub present: usize,
    /// The time of each timed check that did not fail, in order.
    pub times: Vec<Duration>,
    /// The checks that failed, warm-up included.
    pub failed: usize,
    /// The keys answered other than expected, over every check, warm-up included.
    pub wrong: usize,
    /// What the first failed check said.
    pub first_failure: Option<String>,
}

/// Makes `warmup` untimed checks of `asks`, then `rounds` timed ones, and judges every
/// answer: the first `present` asks must be answered present and the rest absent. `now`
/// is the clock; it is read just before and just after each check, and nowhere else.
pub async fn measure<P: PresenceCheck>(
    check: &P,
    asks: &[Ask],
    present: usize,
    (warmup, rounds): (usize, usize),
    now: &dyn Fn() -> Duration,
) -> Row {
    let mut row = Row {
        keys: asks.len(),
        present,
        times: Vec::with_capacity(rounds),
        failed: 0,
        wrong: 0,
        first_failure: None,
    };
    for round in 0..warmup + rounds {
        let start = now();
        let answered = check.check(asks).await;
        let took = now().saturating_sub(start);
        match answered {
            Ok(answers) => {
                row.wrong += wrong(&answers, asks.len(), present);
                if round >= warmup {
                    row.times.push(took);
                }
            }
            Err(error) => {
                row.failed += 1;
                row.first_failure.get_or_insert_with(|| error.to_string());
            }
        }
    }
    row
}

/// The keys of `answers` that are not what was expected of `asked` keys, the first
/// `present` present and the rest absent; a missing or extra answer is wrong too.
fn wrong(answers: &[Presence], asked: usize, present: usize) -> usize {
    let judged = answers.iter().enumerate().filter(|(index, answer)| {
        if *index < present {
            !matches!(answer, Presence::Present { .. })
        } else {
            **answer != Presence::Absent
        }
    });
    judged.count() + asked.abs_diff(answers.len())
}

/// The nearest-rank percentile `permille`/1000 of `sorted` (ascending), or `None` for no
/// times.
#[must_use]
pub fn percentile(sorted: &[Duration], permille: usize) -> Option<Duration> {
    let rank = (sorted.len() * permille).div_ceil(1000).max(1);
    sorted.get(rank - 1).copied()
}

/// One lag round's result.
#[derive(Debug, PartialEq, Eq)]
pub struct Lag {
    /// The time from the write's acknowledgement to the answer that saw the object.
    pub seen_after: Duration,
    /// The looks it took, the one that saw it included.
    pub polls: usize,
}

/// Why a lag round gave no number.
#[derive(Debug, thiserror::Error)]
pub enum LagError {
    /// The write failed.
    #[error("the write of {key} failed: {error}")]
    Put {
        /// The object's key.
        key: ObjectKey,
        /// What the store said.
        error: ObjectStoreError,
    },
    /// A look failed.
    #[error("a look for {key} failed: {error}")]
    Check {
        /// The object's key.
        key: ObjectKey,
        /// What the check said.
        error: String,
    },
    /// The filer did not answer the object present in time.
    #[error("the filer had not seen {key} after {polls} looks")]
    NotSeen {
        /// The object's key.
        key: ObjectKey,
        /// The looks made.
        polls: usize,
    },
}

/// Writes `ask.size` bytes under `ask.key` to `store`, then asks `check` for the key every
/// `poll` until it answers it present, at most `max_polls` times. The lag is read from `now`
/// after the write was acknowledged and after the look that saw the object, and only then.
///
/// # Errors
/// [`LagError`]: the write or a look failed, or no look saw the object.
pub async fn lag_once<S: ObjectStore, P: PresenceCheck>(
    store: &S,
    check: &P,
    ask: &Ask,
    (poll, max_polls): (Duration, usize),
    now: &dyn Fn() -> Duration,
) -> Result<Lag, LagError> {
    let body = Bytes::from(vec![
        0;
        usize::try_from(ask.size).unwrap_or(LAG_OBJECT_BYTES)
    ]);
    let key = || ask.key.clone();
    let put = store.put_new(&ask.key, body, None).await;
    put.map_err(|error| LagError::Put { key: key(), error })?;
    let written = now();
    for polls in 1..=max_polls {
        let answer = check.check(std::slice::from_ref(ask)).await;
        let answer = answer.map_err(|error| LagError::Check {
            key: key(),
            error: error.to_string(),
        })?;
        if matches!(answer.as_slice(), [Presence::Present { .. }]) {
            let seen_after = now().saturating_sub(written);
            return Ok(Lag { seen_after, polls });
        }
        tokio::time::sleep(poll).await;
    }
    Err(LagError::NotSeen {
        key: key(),
        polls: max_polls,
    })
}

/// The flags, checked.
struct Plan<'a> {
    args: &'a Args,
    root: FilerRoot,
    /// `<root>/<sample>`, the directory listed.
    sample: FilerRoot,
    timeout: Duration,
    lag: Option<LagPlan<'a>>,
}

struct LagPlan<'a> {
    endpoint: &'a str,
    bucket: &'a str,
    credentials_file: &'a Path,
    /// `/buckets/<bucket>`, where the filer finds the bucket's objects.
    root: FilerRoot,
    /// The object of each round.
    keys: Vec<ObjectKey>,
}

fn usage(why: impl Into<String>) -> Error {
    Error::Usage(why.into())
}

/// The client of `filer` for the objects under `root`.
fn presence(filer: &str, root: FilerRoot, timeout: Duration) -> Result<FilerPresence, Error> {
    FilerPresence::connect_lazy(filer, root, timeout).map_err(|error| usage(error.to_string()))
}

impl<'a> Plan<'a> {
    fn new(args: &'a Args) -> Result<Self, Error> {
        let bad_root = |error: FilerError| usage(error.to_string());
        let root = FilerRoot::new(&args.filer_root).map_err(bad_root)?;
        let sample =
            FilerRoot::new(format!("{}/{}", args.filer_root, args.sample)).map_err(bad_root)?;
        if args.keys.is_empty() || args.keys.contains(&0) {
            return Err(usage("--keys: each batch needs at least one key"));
        }
        if args.present.iter().any(|share| *share > 100) {
            return Err(usage("--present: a share is a percent, 0 to 100"));
        }
        if args.rounds == 0 || args.timeout_ms == 0 {
            return Err(usage("--rounds and --timeout-ms must be above 0"));
        }
        let lag = match &args.lag_store_endpoint {
            Some(endpoint) => Some(Self::lag(args, endpoint)?),
            None => None,
        };
        let timeout = Duration::from_millis(args.timeout_ms);
        Ok(Self {
            args,
            root,
            sample,
            timeout,
            lag,
        })
    }

    /// The lag flags, which clap makes come together.
    fn lag(args: &'a Args, endpoint: &'a str) -> Result<LagPlan<'a>, Error> {
        let (Some(bucket), Some(credentials_file), Some(nonce)) = (
            &args.lag_store_bucket,
            &args.lag_store_credentials_file,
            &args.lag_nonce,
        ) else {
            return Err(usage(
                "--lag-store-endpoint needs --lag-store-bucket, --lag-store-credentials-file and --lag-nonce",
            ));
        };
        let nonce_ok = nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if nonce.is_empty() || !nonce_ok {
            return Err(usage(format!(
                "--lag-nonce {nonce:?}: use A-Z a-z 0-9 . _ - only"
            )));
        }
        if args.lag_rounds == 0 || args.lag_max_polls == 0 {
            return Err(usage("--lag-rounds and --lag-max-polls must be above 0"));
        }
        let root = FilerRoot::new(format!("/buckets/{bucket}"));
        let root = root.map_err(|error| usage(format!("--lag-store-bucket: {error}")))?;
        let key = |round| ObjectKey::new(format!("{}{nonce}/{round}", args.lag_prefix));
        let keys = (0..args.lag_rounds).map(key).collect::<Result<Vec<_>, _>>();
        let keys = keys.map_err(|error| usage(format!("--lag-prefix: {error}")))?;
        Ok(LagPlan {
            endpoint,
            bucket,
            credentials_file,
            root,
            keys,
        })
    }
}

/// Runs the verb (module docs) on a runtime of its own.
///
/// # Errors
/// [`Error::Usage`] for flags that do not fit, [`Error::Failed`] when the listing failed or
/// holds too few files, and [`Error::Refused`] with the table when a check failed or
/// answered wrongly, or a lag round gave no number.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let plan = Plan::new(args)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::spawn("the bench's runtime".to_owned()))?;
    runtime.block_on(bench(&plan))
}

async fn bench(plan: &Plan<'_>) -> Result<Vec<String>, Error> {
    let args = plan.args;
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{}", args.filer));
    let endpoint = endpoint.map_err(|error| usage(format!("--filer: {error}")))?;
    let check = presence(&args.filer, plan.root.clone(), plan.timeout)?;
    let lag = plan.lag.as_ref();
    let lag_store = lag
        .map(|lag| lag_store(lag, args, plan.timeout))
        .transpose()?;
    let largest = args.keys.iter().copied().max().unwrap_or(0);
    let files = sample(plan, endpoint, largest).await?;
    let start = Instant::now();
    let now = move || start.elapsed();
    let mut lines = vec![
        format!(
            "filer-presence-bench: the filer at {}, root {}, {} files listed under {}",
            args.filer,
            plan.root.as_str(),
            files.len(),
            args.sample
        ),
        format!(
            "each row: {} timed checks after {} untimed; a check asks every key of the row, \
             in calls of at most {MAX_NAMES_PER_CALL} names sent at once",
            args.rounds, args.warmup
        ),
        "| keys | present | timed | p50 ms | p99 ms | max ms | failed | wrong |".to_owned(),
        "|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    let mut problems = Vec::new();
    for &keys in &args.keys {
        let mut measured = Vec::new();
        for &share in &args.present {
            // A share that rounds down to a count already measured for this size is that row.
            let present = keys * usize::from(share) / 100;
            if measured.contains(&present) {
                continue;
            }
            measured.push(present);
            let asks = row_asks(&files[..keys], present, &args.sample)?;
            let rounds = (args.warmup, args.rounds);
            let mut row = measure(&check, &asks, present, rounds, &now).await;
            row.times.sort_unstable();
            lines.push(table_line(&row));
            if let Some(failure) = &row.first_failure {
                problems.push(format!("{keys} keys, {present} present: {failure}"));
            }
            if row.wrong > 0 {
                let wrong = row.wrong;
                problems.push(format!(
                    "{keys} keys, {present} present: {wrong} wrong answers"
                ));
            }
        }
    }
    if let (Some(lag), Some(store)) = (lag, lag_store) {
        let lag_check = presence(&args.filer, lag.root.clone(), plan.timeout)?;
        // One untimed look first, so that no round's lag holds the opening of the
        // connection; what it answers is not used.
        let opening = lag.keys.iter().take(1).map(|key| Ask {
            key: key.clone(),
            size: 0,
        });
        let _ = lag_check.check(&opening.collect::<Vec<_>>()).await;
        let out = (&mut lines, &mut problems);
        lag_rounds(lag, args, (&store, &lag_check), &now, out).await;
    }
    if problems.is_empty() {
        Ok(lines)
    } else {
        lines.extend(problems);
        Err(Error::Refused(lines))
    }
}

/// The asks of a row: the first `present` files as they are, the rest with
/// [`ABSENT_SUFFIX`].
fn row_asks(files: &[(String, u64)], present: usize, sample: &str) -> Result<Vec<Ask>, Error> {
    let mut asks = Vec::with_capacity(files.len());
    for (index, (name, size)) in files.iter().enumerate() {
        let (key, size) = if index < present {
            (format!("{sample}/{name}"), *size)
        } else {
            (format!("{sample}/{name}{ABSENT_SUFFIX}"), 1)
        };
        let bad = |error| Error::Failed(format!("the listed file {name:?} makes no key: {error}"));
        asks.push(Ask {
            key: ObjectKey::new(key).map_err(bad)?,
            size,
        });
    }
    Ok(asks)
}

fn millis(time: Option<Duration>) -> String {
    time.map_or_else(
        || "-".to_owned(),
        |time| format!("{:.3}", time.as_secs_f64() * 1e3),
    )
}

/// The table line of `row`, whose times are sorted: its keys, present keys, timed checks,
/// p50, p99 and max in milliseconds (`-` with no times), failed checks and wrong answers.
#[must_use]
pub fn table_line(row: &Row) -> String {
    let times = &row.times;
    format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} |",
        row.keys,
        row.present,
        times.len(),
        millis(percentile(times, 500)),
        millis(percentile(times, 990)),
        millis(times.last().copied()),
        row.failed,
        row.wrong
    )
}

/// The first `want` files directly under the sample directory, by name, with their listed
/// lengths: as many pages of `ListEntries` as it takes.
async fn sample(
    plan: &Plan<'_>,
    endpoint: tonic::transport::Endpoint,
    want: usize,
) -> Result<Vec<(String, u64)>, Error> {
    let filer = &plan.args.filer;
    let directory = plan.sample.as_str();
    let failed = |status: tonic::Status| {
        Error::Failed(format!(
            "the listing of {directory} on the filer at {filer}: {status}"
        ))
    };
    let endpoint = endpoint.connect_timeout(plan.timeout).timeout(plan.timeout);
    let mut client = SeaweedFilerClient::new(endpoint.connect_lazy());
    let mut files = Vec::with_capacity(want);
    let mut after = String::new();
    while files.len() < want {
        let request = ListEntriesRequest {
            directory: directory.to_owned(),
            start_from_file_name: after.clone(),
            limit: u32::try_from(want - files.len()).unwrap_or(u32::MAX),
            omit_chunks: true,
            ..ListEntriesRequest::default()
        };
        let mut stream = client
            .list_entries(request)
            .await
            .map_err(failed)?
            .into_inner();
        let mut listed = 0;
        while let Some(page) = stream.message().await.map_err(failed)? {
            // A listed message carries its entry; one without names nothing.
            for entry in page.entry.into_iter() {
                listed += 1;
                after.clone_from(&entry.name);
                if !entry.is_directory {
                    let size = entry
                        .attributes
                        .map_or(0, |attributes| attributes.file_size);
                    files.push((entry.name, size));
                }
            }
        }
        if listed == 0 {
            break;
        }
    }
    if files.len() < want {
        return Err(Error::Failed(format!(
            "{directory} holds {} files on the filer at {filer}; the largest batch needs {want}",
            files.len(),
        )));
    }
    files.truncate(want);
    Ok(files)
}

/// The store the lag objects are written through.
fn lag_store(lag: &LagPlan<'_>, args: &Args, timeout: Duration) -> Result<S3Store, Error> {
    let file = lag.credentials_file;
    let credentials = read_credentials(file).map_err(|why| {
        usage(format!(
            "--lag-store-credentials-file {}: {why}",
            file.display()
        ))
    })?;
    let config = S3Config {
        endpoint: lag.endpoint.to_owned(),
        region: args.lag_store_region.clone(),
        bucket: lag.bucket.to_owned(),
        credentials,
        capabilities: Capabilities::default(),
        connect_timeout: timeout,
        request_timeout: timeout,
        min_body_rate: None,
    };
    S3Store::new(config).map_err(|error| {
        usage(format!(
            "--lag-store-endpoint or --lag-store-bucket: {error}"
        ))
    })
}

/// The lag rounds: their summary goes to `lines`, what failed to `problems`.
async fn lag_rounds(
    lag: &LagPlan<'_>,
    args: &Args,
    (store, check): (&S3Store, &FilerPresence),
    now: &dyn Fn() -> Duration,
    (lines, problems): (&mut Vec<String>, &mut Vec<String>),
) {
    let poll = Duration::from_millis(args.lag_poll_ms);
    let mut lags = Vec::with_capacity(lag.keys.len());
    let mut polls = 0;
    for key in &lag.keys {
        let ask = Ask {
            key: key.clone(),
            size: LAG_OBJECT_BYTES as u64,
        };
        match lag_once(store, check, &ask, (poll, args.lag_max_polls), now).await {
            Ok(seen) => {
                lags.push(seen.seen_after);
                polls += seen.polls;
            }
            Err(error) => problems.push(format!("lag: {error}")),
        }
        if let Err(error) = store.delete(&ask.key).await {
            problems.push(format!("lag: the delete of {} failed: {error}", ask.key));
        }
    }
    lags.sort_unstable();
    lines.push(format!(
        "lag (written through {}, seen by the filer at {}, a look every {} ms): {} of {} \
         rounds seen, {polls} looks; p50 {} ms, p99 {} ms, max {} ms (nearest rank: under 100 \
         rounds the p99 is the max)",
        lag.endpoint,
        args.filer,
        args.lag_poll_ms,
        lags.len(),
        args.lag_rounds,
        millis(percentile(&lags, 500)),
        millis(percentile(&lags, 990)),
        millis(lags.last().copied())
    ));
}

/// The key pair in the file at `path`: JSON with `access_key_id` and `secret_access_key`, in
/// a file with no permission for group or others. The reason of an error never holds a part
/// of the secret.
fn read_credentials(path: &Path) -> Result<Credentials, String> {
    const NOT_KEYS: &str = "not JSON with access_key_id and secret_access_key";
    let mode = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        let shown = mode & 0o777;
        return Err(format!(
            "mode {shown:04o} lets others read a secret: use 0600"
        ));
    }
    let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|error| error.to_string())?);
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| NOT_KEYS.to_owned())?;
    let field = |name| value.get(name).and_then(serde_json::Value::as_str);
    let (Some(id), Some(secret)) = (field("access_key_id"), field("secret_access_key")) else {
        return Err(NOT_KEYS.to_owned());
    };
    Ok(Credentials::new(id.to_owned(), secret.to_owned()))
}
