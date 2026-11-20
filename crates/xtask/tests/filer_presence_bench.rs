//! `cargo xtask filer-presence-bench` over fakes: the timing and the lag over a checker
//! that moves an injected clock, and the whole verb over a fake filer (and a fake S3
//! service for the lag) on loopback. No test reads a real clock or bounds a time.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use clap::Parser;
use kbf_objstore::filer::fake::FakeFiler;
use kbf_objstore::filer::{Ask, Presence, PresenceCheck};
use kbf_objstore::s3::fake::{Failure, FakeS3};
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore};
use kbf_proto::filer_pb::{Entry, LookupDirectoryEntryResult};
use kbf_testkit::dir::TestDir;
use tonic::Code;
use xtask::Error;
use xtask::filer_presence_bench::{
    Args, LAG_OBJECT_BYTES, Lag, LagError, Row, lag_once, measure, percentile, run, table_line,
};

const ROOT: &str = "/buckets/kbf-cas";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A clock the test moves: milliseconds since the start.
#[derive(Clone, Default)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn advance(&self, by: u64) {
        self.0.fetch_add(by, Ordering::SeqCst);
    }

    fn reader(&self) -> impl Fn() -> Duration + use<> {
        let at = Arc::clone(&self.0);
        move || ms(at.load(Ordering::SeqCst))
    }
}

/// What the scripted checker answers on its nth call (from 0).
type Script = Box<dyn Fn(usize, &[Ask]) -> Result<Vec<Presence>, String> + Send + Sync>;

/// A checker that moves the clock by `per_call` during each call and answers from a script.
struct Scripted {
    clock: Clock,
    per_call: u64,
    calls: AtomicUsize,
    script: Script,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Refused(String);

impl PresenceCheck for Scripted {
    type Error = Refused;

    async fn check(&self, asks: &[Ask]) -> Result<Vec<Presence>, Refused> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.advance(self.per_call);
        (self.script)(call, asks).map_err(Refused)
    }
}

fn scripted(clock: &Clock, per_call: u64, script: Script) -> Scripted {
    Scripted {
        clock: clock.clone(),
        per_call,
        calls: AtomicUsize::new(0),
        script,
    }
}

fn ask(key: &str, size: u64) -> Ask {
    Ask {
        key: ObjectKey::new(key).expect("key"),
        size,
    }
}

fn present_then_absent(present: usize) -> Script {
    Box::new(move |_, asks| {
        let answer = |n| match n < present {
            true => Presence::Present { size: 1 },
            false => Presence::Absent,
        };
        Ok((0..asks.len()).map(answer).collect())
    })
}

/// Catches a bench that times more than the lookup (the warm-up checks, or a call that
/// failed) or reports a time it did not take: with a call that takes 3 ms on the clock, 2
/// untimed and 4 timed checks give exactly four times of 3 ms, and a failed timed check is
/// counted, with its text, and leaves no time.
#[tokio::test]
async fn the_bench_times_the_lookup_call_only() {
    let clock = Clock::default();
    let asks = [ask("cas/l/a", 1), ask("cas/l/a.absent", 1)];
    let check = scripted(&clock, 3, present_then_absent(1));

    let row = measure(&check, &asks, 1, (2, 4), &clock.reader()).await;

    let want = Row {
        keys: 2,
        present: 1,
        times: vec![ms(3); 4],
        failed: 0,
        wrong: 0,
        first_failure: None,
    };
    assert_eq!(row, want);
    assert_eq!(check.calls.load(Ordering::SeqCst), 6);

    let failing = scripted(
        &clock,
        3,
        Box::new(|call, asks| match call {
            // The warm-up call and the second timed one.
            0 | 2 => Err(format!("call {call} refused")),
            _ => present_then_absent(1)(call, asks),
        }),
    );
    let row = measure(&failing, &asks, 1, (1, 4), &clock.reader()).await;
    assert_eq!(row.times, [ms(3); 3], "the failed timed call left no time");
    assert_eq!(row.failed, 2, "the warm-up failure counts too");
    assert_eq!(row.first_failure.as_deref(), Some("call 0 refused"));
}

/// Catches a bench that trusts a filer's answers: a present key answered absent, an absent
/// key answered present or short, and an answer with a key missing are each counted wrong.
#[tokio::test]
async fn every_answer_is_judged() {
    let clock = Clock::default();
    let asks = [ask("a", 1), ask("b", 1), ask("c", 1), ask("d", 1)];
    let check = scripted(
        &clock,
        1,
        Box::new(|call, _| match call {
            // Present, present wrongly absent, absent wrongly short, absent wrongly present.
            0 => Ok(vec![
                Presence::Present { size: 1 },
                Presence::Absent,
                Presence::Short { size: 0 },
                Presence::Present { size: 1 },
            ]),
            // One answer for four keys: the three left out are wrong.
            _ => Ok(vec![Presence::Present { size: 1 }]),
        }),
    );

    let row = measure(&check, &asks, 2, (0, 2), &clock.reader()).await;

    assert_eq!(row.wrong, 3 + 3);
    assert_eq!(row.times.len(), 2, "a wrong answer is still timed");

    // Wrong answers in the warm-up count too: the warm-up call (call 0 of a new checker)
    // answers both present keys absent, and the timed one leaves three keys out.
    let again = scripted(
        &clock,
        1,
        Box::new(|call, _| match call {
            0 => Ok(vec![Presence::Absent; 4]),
            _ => Ok(vec![Presence::Present { size: 1 }]),
        }),
    );
    let row = measure(&again, &asks, 2, (1, 1), &clock.reader()).await;
    assert_eq!((row.wrong, row.times.len()), (2 + 3, 1));
}

/// Catches a table whose numbers are not the row's: the full line of a row of 100 timed
/// checks of 1 to 100 ms, and of a row with no times.
#[test]
fn a_row_prints_its_percentiles_in_milliseconds() {
    let row = Row {
        keys: 1000,
        present: 500,
        times: (1..=100).map(ms).collect(),
        failed: 1,
        wrong: 2,
        first_failure: Some("refused".to_owned()),
    };
    assert_eq!(
        table_line(&row),
        "| 1000 | 500 | 100 | 50.000 | 99.000 | 100.000 | 1 | 2 |"
    );
    let empty = Row {
        times: vec![],
        ..row
    };
    assert_eq!(table_line(&empty), "| 1000 | 500 | 0 | - | - | - | 1 | 2 |");
    let fraction = Row {
        times: vec![Duration::from_micros(1500)],
        ..empty
    };
    assert_eq!(
        table_line(&fraction),
        "| 1000 | 500 | 1 | 1.500 | 1.500 | 1.500 | 1 | 2 |"
    );
}

/// Catches a percentile off by one rank, or read from no times: nearest rank, so the p50
/// and p99 of 1..=100 ms are 50 and 99 ms, one time is every percentile, and no times give
/// none.
#[test]
fn percentiles_are_nearest_rank() {
    let hundred: Vec<Duration> = (1..=100).map(ms).collect();
    assert_eq!(percentile(&hundred, 500), Some(ms(50)));
    assert_eq!(percentile(&hundred, 990), Some(ms(99)));
    assert_eq!(percentile(&hundred, 1000), Some(ms(100)));
    assert_eq!(percentile(&hundred, 0), Some(ms(1)));
    assert_eq!(percentile(&[ms(7)], 990), Some(ms(7)));
    assert_eq!(percentile(&[ms(1), ms(2), ms(3)], 500), Some(ms(2)));
    assert_eq!(percentile(&[], 500), None);
}

/// A checker that answers from the store it shares with the test: absent for the first
/// `hidden` looks, short on the next, then present; it fails the test if it is asked
/// before the object was written.
fn seen_after(clock: &Clock, store: Arc<MemoryStore>, hidden: usize) -> Scripted {
    scripted(
        clock,
        10,
        Box::new(move |call, asks| {
            let key = &asks[0].key;
            let written = futures_now(store.len(key)).expect("len");
            assert!(written.is_some(), "looked for {key} before it was written");
            Ok(vec![match call {
                n if n < hidden => Presence::Absent,
                n if n == hidden => Presence::Short { size: 1 },
                _ => Presence::Present { size: 4096 },
            }])
        }),
    )
}

/// Polls a future that is ready at once (the memory store's).
fn futures_now<T>(future: impl Future<Output = T>) -> T {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("the memory store's future was not ready"),
    }
}

/// A store whose writes take 1000 ms on the clock: a lag counted from before the write was
/// acknowledged would hold them.
struct SlowWrites {
    inner: Arc<MemoryStore>,
    clock: Clock,
}

impl ObjectStore for SlowWrites {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: bytes::Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), kbf_objstore::ObjectStoreError> {
        self.clock.advance(1000);
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: kbf_objstore::ByteRange,
    ) -> Result<bytes::Bytes, kbf_objstore::ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), kbf_objstore::ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &kbf_objstore::KeyPrefix,
        after: Option<&kbf_objstore::ListToken>,
        max_keys: kbf_objstore::PageSize,
    ) -> Result<kbf_objstore::ListPage, kbf_objstore::ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// Catches a bench that reports a lag it did not measure: one counted from before the write
/// was acknowledged (the write takes 1000 ms on the clock), or that stops at a look that did
/// not see the whole object. Two absent looks and a short one, 10 ms each on the clock, then
/// the look that sees it: a lag of 40 ms over 4 looks, every look after the write.
#[tokio::test]
async fn the_bench_reports_a_lag_only_after_it_saw_the_key() {
    let clock = Clock::default();
    let store = Arc::new(MemoryStore::new(Capabilities::default()));
    let check = seen_after(&clock, Arc::clone(&store), 2);
    let ask = ask("kbf-bench/lag/n1/0", LAG_OBJECT_BYTES as u64);
    let slow = SlowWrites {
        inner: Arc::clone(&store),
        clock: clock.clone(),
    };

    let lag = lag_once(&slow, &check, &ask, (Duration::ZERO, 10), &clock.reader()).await;

    let lag = lag.expect("seen");
    assert_eq!(
        lag,
        Lag {
            seen_after: ms(40),
            polls: 4
        }
    );
    let written = store.len(&ask.key).await.expect("len");
    assert_eq!(written, Some(LAG_OBJECT_BYTES as u64));
}

/// Catches a lag reported for an object the filer never saw, or after a look or the write
/// failed: each gives no number, and says which.
#[tokio::test]
async fn a_lag_not_seen_or_not_asked_gives_no_number() {
    let clock = Clock::default();
    let store = Arc::new(MemoryStore::new(Capabilities::default()));
    let never = seen_after(&clock, Arc::clone(&store), usize::MAX);
    let key = ask("kbf-bench/lag/n1/1", 8);
    let poll = (Duration::ZERO, 5);

    let error = lag_once(&*store, &never, &key, poll, &clock.reader()).await;
    assert!(
        matches!(&error, Err(LagError::NotSeen { polls: 5, .. })),
        "{error:?}"
    );
    assert_eq!(
        error.expect_err("not seen").to_string(),
        "the filer had not seen kbf-bench/lag/n1/1 after 5 looks"
    );

    let failing = scripted(&clock, 1, Box::new(|_, _| Err("filer down".to_owned())));
    let error = lag_once(&*store, &failing, &key, poll, &clock.reader()).await;
    assert_eq!(
        error.expect_err("look failed").to_string(),
        "a look for kbf-bench/lag/n1/1 failed: filer down"
    );

    let conditional = Capabilities {
        conditional_put: true,
        ..Capabilities::default()
    };
    let once = MemoryStore::new(conditional);
    once.put_new(&key.key, vec![0; 8].into(), None)
        .await
        .expect("first write");
    let error = lag_once(&once, &failing, &key, poll, &clock.reader()).await;
    let error = error.expect_err("write refused");
    assert!(matches!(error, LagError::Put { .. }), "{error:?}");
    assert!(
        error
            .to_string()
            .starts_with("the write of kbf-bench/lag/n1/1 failed: ")
    );
}

/// The verb's flags as clap gives them, from `line` after the verb's name.
fn args(line: &[&str]) -> Args {
    #[derive(Parser)]
    struct Verb {
        #[command(flatten)]
        args: Args,
    }
    let argv = std::iter::once("filer-presence-bench").chain(line.iter().copied());
    Verb::try_parse_from(argv).expect("flags").args
}

/// A runtime the fakes serve on, beside the one the verb makes for itself.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A fake filer holding `files` files under `<ROOT>/cas/l`, named `f00`, `f01`, ..., of
/// 100 bytes and more, with a directory `f01-dir` among them.
fn filer_with(rt: &tokio::runtime::Runtime, files: usize) -> FakeFiler {
    let filer = rt.block_on(FakeFiler::start());
    for n in 0..files {
        filer.put_file(&format!("{ROOT}/cas/l/f{n:02}"), 100 + n as u64);
    }
    let directory = Entry {
        is_directory: true,
        ..Entry::default()
    };
    filer.put_entry(&format!("{ROOT}/cas/l/f01-dir"), directory);
    filer
}

/// The rows of the verb's own test: three sizes, three shares, three timed checks each.
const ROWS: [&str; 8] = [
    "--keys",
    "1,5,12",
    "--present",
    "100,50,0",
    "--rounds",
    "3",
    "--warmup",
    "1",
];

/// Three timed checks after one untimed, for the tests that give their own rows.
const FAST: [&str; 4] = ["--rounds", "3", "--warmup", "1"];

/// `--filer <filer> --filer-root <ROOT>` and `extra`.
fn bench_line<'a>(filer: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut line = vec!["--filer", filer, "--filer-root", ROOT];
    line.extend(extra);
    line
}

/// The lag flags, writing through `endpoint` with the key pair at `keys`, and `extra` after
/// them (a bucket or nonce in `extra` takes the place of the default one).
fn lag_flags<'a>(endpoint: &'a str, keys: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut line = vec![
        "--keys",
        "1",
        "--lag-store-endpoint",
        endpoint,
        "--lag-store-credentials-file",
        keys,
    ];
    for (flag, value) in [("--lag-store-bucket", "swfs-bench"), ("--lag-nonce", "n1")] {
        if !extra.contains(&flag) {
            line.extend([flag, value]);
        }
    }
    line.extend(extra);
    line
}

/// Catches a verb that measures other keys than it says, skips or repeats a row, or reads the listing
/// wrongly (a directory taken for a file, a page that repeats or drops a name): over a
/// fake filer of 12 files and a directory, every row is printed, every check asks every key
/// of its row, and the present keys are the listed files and the rest the same names with
/// `.absent`.
#[test]
fn the_verb_prints_a_row_for_each_size_and_share() {
    let rt = runtime();
    let filer = filer_with(&rt, 12);
    let address = filer.address();

    let lines = run(&args(&bench_line(&address, &ROWS))).expect("bench");

    assert_eq!(
        lines[0],
        format!(
            "filer-presence-bench: the filer at {address}, root {ROOT}, 12 files listed under cas/l"
        )
    );
    assert_eq!(
        lines[1],
        "each row: 3 timed checks after 1 untimed; a check asks every key of the row, in calls \
         of at most 4096 names sent at once"
    );
    // 1 key at 50% is 0 present keys, the row of 0%: it is measured once.
    assert_eq!(lines.len(), 4 + 8);
    let rows: Vec<String> = lines[4..]
        .iter()
        .map(|line| line.split(" | ").take(3).collect::<Vec<_>>().join(" | "))
        .collect();
    assert_eq!(
        rows,
        [
            "| 1 | 1 | 3",
            "| 1 | 0 | 3",
            "| 5 | 5 | 3",
            "| 5 | 2 | 3",
            "| 5 | 0 | 3",
            "| 12 | 12 | 3",
            "| 12 | 6 | 3",
            "| 12 | 0 | 3",
        ]
    );
    assert!(
        lines[4..].iter().all(|line| line.ends_with(" | 0 | 0 |")),
        "{lines:#?}"
    );
    assert_eq!(
        filer.lookups(),
        [vec![1; 8], vec![5; 12], vec![12; 12]].concat()
    );
    let asked = filer.asked();
    assert!(
        asked
            .iter()
            .all(|(directory, _)| *directory == format!("{ROOT}/cas/l"))
    );
    let last_check = &asked[asked.len() - 12..];
    let names: Vec<&str> = last_check.iter().map(|(_, name)| name.as_str()).collect();
    let want: Vec<String> = (0..12).map(|n| format!("f{n:02}.absent")).collect();
    assert_eq!(
        names, want,
        "the listed files, f01-dir left out, taken absent"
    );
    let first_check_of_12 = &asked[8 + 4 * 5 * 3..][..12];
    let names: Vec<&str> = first_check_of_12
        .iter()
        .map(|(_, name)| name.as_str())
        .collect();
    let want: Vec<String> = (0..12).map(|n| format!("f{n:02}")).collect();
    assert_eq!(names, want, "the listed files, taken present");
}

/// Catches a number reported from a filer that answered wrongly or failed: an absent key
/// the filer answers present makes its rows wrong, a refused call makes every row failed
/// with no times, and either fails the verb with the table and the reason printed.
#[test]
fn a_wrong_or_failed_answer_fails_the_verb_with_the_table() {
    let rt = runtime();
    let filer = filer_with(&rt, 12);
    let found = LookupDirectoryEntryResult {
        found: true,
        entry: Some(Entry::default()),
        ..LookupDirectoryEntryResult::default()
    };
    filer.answer_raw(&format!("{ROOT}/cas/l/f00.absent"), found);
    let address = filer.address();
    let line = [&["--keys", "1", "--present", "0,100"][..], &FAST].concat();

    let Err(Error::Refused(lines)) = run(&args(&bench_line(&address, &line))) else {
        panic!("not refused");
    };
    assert!(lines[4].starts_with("| 1 | 0 | 3 | "), "{}", lines[4]);
    assert!(
        lines[4].ends_with(" | 0 | 4 |"),
        "four checks, one wrong key each: {}",
        lines[4]
    );
    assert!(lines[5].starts_with("| 1 | 1 | 3 | "), "{}", lines[5]);
    assert!(lines[5].ends_with(" | 0 | 0 |"), "{}", lines[5]);
    assert_eq!(lines[6..], ["1 keys, 0 present: 4 wrong answers"]);

    filer.fail_lookups(Code::Unavailable, "filer store is down");
    let Err(Error::Refused(lines)) = run(&args(&bench_line(&address, &line))) else {
        panic!("not refused");
    };
    assert_eq!(lines[4], "| 1 | 0 | 0 | - | - | - | 4 | 0 |");
    assert_eq!(lines[5], "| 1 | 1 | 0 | - | - | - | 4 | 0 |");
    let refused = format!("1 keys, 0 present: the filer at {address} failed a lookup of 1 names: ");
    assert!(lines[6].starts_with(&refused), "{}", lines[6]);
    assert!(lines[6].contains("filer store is down"), "{}", lines[6]);
    assert!(lines[7].starts_with("1 keys, 1 present: "), "{}", lines[7]);
    assert_eq!(lines.len(), 8);
}

/// Catches a bench that measures fewer keys than it reports, or reports a listing that
/// failed as numbers: a directory with fewer files than the largest batch, a filer that
/// cannot be reached for the listing, and a listed name that makes no key each fail it.
#[test]
fn a_listing_short_or_failed_fails_the_verb() {
    let rt = runtime();
    let filer = filer_with(&rt, 12);
    let address = filer.address();

    let line = [&["--keys", "13"][..], &FAST].concat();
    let error = run(&args(&bench_line(&address, &line))).expect_err("short");
    assert_eq!(
        error.to_string(),
        format!(
            "{ROOT}/cas/l holds 12 files on the filer at {address}; the largest batch needs 13"
        )
    );

    let dead = kbf_testkit::port::dead_addr().to_string();
    let error = run(&args(&bench_line(&dead, &ROWS))).expect_err("no filer");
    let text = error.to_string();
    assert!(
        text.starts_with(&format!(
            "the listing of {ROOT}/cas/l on the filer at {dead}: "
        )),
        "{text}"
    );

    filer.put_file(&format!("{ROOT}/cas/l/{}", "x".repeat(1100)), 1);
    let error = run(&args(&bench_line(&address, &line))).expect_err("long name");
    assert!(matches!(&error, Error::Failed(_)), "{error:?}");
    assert!(
        error.to_string().starts_with("the listed file \"xxx"),
        "{error}"
    );
}

/// Catches flags that would bench nothing or the wrong thing accepted: each is a usage
/// error that names the flag, before anything is asked or written.
#[test]
fn flags_that_do_not_fit_are_usage_errors() {
    let rt = runtime();
    let filer = filer_with(&rt, 1);
    let address = filer.address();
    let long_prefix = "p".repeat(1100);
    let dead = format!("http://{}", kbf_testkit::port::dead_addr());
    let dir = scratch("usage");
    let missing = dir.path().join("keys.json");
    let missing = missing.to_str().expect("utf-8");
    let lag = |extra: &[&'static str]| lag_flags(&dead, missing, extra);
    let cases: Vec<(Vec<&str>, String)> =
        vec![
        (
            vec!["--keys", "1", "--sample", "cas//l"],
            format!(
                "the filer root \"{ROOT}/cas//l\" is not a directory path such as /buckets/<bucket>"
            ),
        ),
        (vec!["--keys", "1,0"], "--keys: each batch needs at least one key".to_owned()),
        (
            vec!["--keys", "1", "--present", "101"],
            "--present: a share is a percent, 0 to 100".to_owned(),
        ),
        (
            vec!["--keys", "1", "--rounds", "0"],
            "--rounds and --timeout-ms must be above 0".to_owned(),
        ),
        (
            vec!["--keys", "1", "--timeout-ms", "0"],
            "--rounds and --timeout-ms must be above 0".to_owned(),
        ),
        (
            lag(&["--lag-nonce", "a/b"]),
            "--lag-nonce \"a/b\": use A-Z a-z 0-9 . _ - only".to_owned(),
        ),
        (lag(&["--lag-nonce", ""]), "--lag-nonce \"\": use A-Z a-z 0-9 . _ - only".to_owned()),
        (
            lag(&["--lag-rounds", "0"]),
            "--lag-rounds and --lag-max-polls must be above 0".to_owned(),
        ),
        (
            lag(&["--lag-max-polls", "0"]),
            "--lag-rounds and --lag-max-polls must be above 0".to_owned(),
        ),
        (
            lag(&["--lag-store-bucket", "a/../b"]),
            "--lag-store-bucket: the filer root \"/buckets/a/../b\" is not a directory path such \
             as /buckets/<bucket>"
                .to_owned(),
        ),
        (
            lag_flags(&dead, missing, &["--lag-prefix", &long_prefix]),
            "--lag-prefix: object key has 1104 bytes; at most 1024 are allowed".to_owned(),
        ),
        (
            lag(&[]),
            format!(
                "--lag-store-credentials-file {missing}: No such file or directory (os error 2)"
            ),
        ),
    ];
    for (extra, want) in cases {
        let line = bench_line(&address, &extra);
        let error = run(&args(&line)).expect_err(&want);
        assert!(matches!(&error, Error::Usage(_)), "{extra:?}: {error:?}");
        assert_eq!(error.to_string(), want, "{extra:?}");
    }
    let relative = [
        "--filer",
        &address,
        "--filer-root",
        "buckets/kbf-cas",
        "--keys",
        "1",
    ];
    let error = run(&args(&relative)).expect_err("relative root");
    assert_eq!(
        error.to_string(),
        "the filer root \"buckets/kbf-cas\" is not a directory path such as /buckets/<bucket>"
    );
    for (filer, want) in [
        ("a b:1", "--filer: invalid URI"),
        (
            "filer-host",
            "the filer address \"filer-host\" is not <host>:<port>",
        ),
    ] {
        let error = run(&args(&bench_line(filer, &ROWS))).expect_err(filer);
        assert!(matches!(&error, Error::Usage(_)), "{error:?}");
        assert_eq!(error.to_string(), want);
    }
    assert!(filer.lookups().is_empty(), "nothing was asked");
}

/// Catches lag flags taken one without the others: clap refuses `--lag-store-endpoint`
/// alone, and a set of flags built without clap is refused by the verb.
#[test]
fn the_lag_flags_come_together() {
    #[derive(Parser)]
    struct Verb {
        #[command(flatten)]
        args: Args,
    }
    let alone = [
        "x",
        "--filer",
        "f:1",
        "--filer-root",
        ROOT,
        "--lag-store-endpoint",
        "http://s:1",
    ];
    let refused = Verb::try_parse_from(alone).err().expect("refused");
    assert_eq!(
        refused.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );

    let mut flags = args(&["--filer", "f:1", "--filer-root", ROOT]);
    flags.lag_store_endpoint = Some("http://s:1".to_owned());
    let error = run(&flags).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "--lag-store-endpoint needs --lag-store-bucket, --lag-store-credentials-file and --lag-nonce"
    );

    // No batch at all, which clap's list of sizes cannot give but a caller can.
    let mut flags = args(&["--filer", "f:1", "--filer-root", ROOT]);
    flags.keys.clear();
    let error = run(&flags).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "--keys: each batch needs at least one key"
    );
}

/// The defaults the verb runs with when only the filer and root are given.
#[test]
fn the_defaults_are_the_measured_sizes_and_shares() {
    let flags = args(&["--filer", "f:1", "--filer-root", ROOT]);
    assert_eq!(flags.keys, [1, 10, 100, 1000, 4096, 10_000]);
    assert_eq!(flags.present, [100, 50, 0]);
    assert_eq!(
        (flags.rounds, flags.warmup, flags.timeout_ms),
        (200, 5, 10_000)
    );
    assert_eq!(flags.sample, "cas/l");
    assert!(
        flags.lag_store_endpoint.is_none(),
        "nothing is written by default"
    );
}

fn keys_file(dir: &Path, name: &str, mode: u32, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write keys");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-filer-bench"),
        name,
    )
}

/// Catches a key pair taken from a file others can read, or a reason that would show the
/// secret: a group-readable file, a file that is not the key pair (not JSON, or JSON with
/// no secret), a directory, and an endpoint the S3 client refuses are usage errors, before
/// anything is written.
#[test]
fn the_lag_store_is_checked_before_anything_is_written() {
    let rt = runtime();
    let filer = filer_with(&rt, 1);
    let address = filer.address();
    let dir = scratch("checked");
    let dead = format!("http://{}", kbf_testkit::port::dead_addr());
    let open = keys_file(
        dir.path(),
        "open.json",
        0o640,
        r#"{"access_key_id":"a","secret_access_key":"s"}"#,
    );
    let junk = keys_file(dir.path(), "junk.json", 0o600, "secret-looking text");
    let no_secret = keys_file(dir.path(), "half.json", 0o600, r#"{"access_key_id":"a"}"#);
    let good = keys_file(
        dir.path(),
        "good.json",
        0o400,
        r#"{"access_key_id":"a","secret_access_key":"s"}"#,
    );
    let not_a_file = dir.path().join("keys-dir");
    std::fs::create_dir(&not_a_file).expect("mkdir");
    std::fs::set_permissions(&not_a_file, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let refuse = |endpoint: &str, file: &Path| {
        let file = file.to_str().expect("utf-8");
        let extra = [
            "--keys",
            "1",
            "--lag-store-endpoint",
            endpoint,
            "--lag-store-bucket",
            "swfs-bench",
            "--lag-store-credentials-file",
            file,
            "--lag-nonce",
            "n1",
        ];
        let error = run(&args(&bench_line(&address, &extra))).expect_err("refused");
        assert!(matches!(&error, Error::Usage(_)), "{error:?}");
        error.to_string()
    };

    assert_eq!(
        refuse(&dead, &open),
        format!(
            "--lag-store-credentials-file {}: mode 0640 lets others read a secret: use 0600",
            open.display()
        )
    );
    assert_eq!(
        refuse(&dead, &junk),
        format!(
            "--lag-store-credentials-file {}: not JSON with access_key_id and secret_access_key",
            junk.display()
        )
    );
    assert_eq!(
        refuse(&dead, &no_secret),
        format!(
            "--lag-store-credentials-file {}: not JSON with access_key_id and secret_access_key",
            no_secret.display()
        )
    );
    assert_eq!(
        refuse(&dead, &not_a_file),
        format!(
            "--lag-store-credentials-file {}: Is a directory (os error 21)",
            not_a_file.display()
        )
    );
    let bad_endpoint = refuse("ftp://127.0.0.1:1", &good);
    assert!(
        bad_endpoint.starts_with("--lag-store-endpoint or --lag-store-bucket: endpoint \"ftp://"),
        "{bad_endpoint}"
    );
    assert!(filer.lookups().is_empty(), "nothing was asked");
}

/// Catches a lag measured on another object than was written, a lag object left behind,
/// or a lost or failed round reported as a number: over a fake S3 service and a fake filer
/// that already holds the round's object, each round is seen on its first look and its
/// object deleted; a round the filer never sees, and a delete that fails, fail the verb.
#[test]
fn the_lag_rounds_write_look_and_delete() {
    let rt = runtime();
    let filer = filer_with(&rt, 1);
    let s3 = rt.block_on(FakeS3::start());
    let address = filer.address();
    let dir = scratch("lag");
    let keys = keys_file(
        dir.path(),
        "keys.json",
        0o600,
        r#"{"access_key_id":"a","secret_access_key":"s"}"#,
    );
    let keys = keys.to_str().expect("utf-8");
    let endpoint = s3.endpoint().to_owned();
    for round in 0..2 {
        filer.put_file(
            &format!("/buckets/swfs-bench/kbf-bench/lag/n1/{round}"),
            4096,
        );
    }
    let lag_line = |rounds| {
        let flags = [
            "--keys",
            "1",
            "--present",
            "100",
            "--lag-store-endpoint",
            &endpoint,
            "--lag-store-bucket",
            "swfs-bench",
            "--lag-store-credentials-file",
            keys,
            "--lag-nonce",
            "n1",
            "--lag-rounds",
            rounds,
            "--lag-poll-ms",
            "0",
            "--lag-max-polls",
            "3",
        ];
        args(&bench_line(&address, &[&flags[..], &FAST].concat()))
    };

    let lines = run(&lag_line("2")).expect("bench");

    let last = lines.last().expect("lag line");
    assert!(
        last.starts_with(&format!(
            "lag (written through {endpoint}, seen by the filer at {address}, a look every 0 ms): \
             2 of 2 rounds seen, 2 looks; p50 "
        )),
        "{last}"
    );
    assert!(
        s3.keys().is_empty(),
        "every lag object was deleted: {:?}",
        s3.keys()
    );
    let lag_names: Vec<(String, String)> = filer
        .asked()
        .into_iter()
        .filter(|(directory, _)| directory.starts_with("/buckets/swfs-bench"))
        .collect();
    assert_eq!(
        lag_names,
        [
            // The untimed look that opens the connection, then one look a round.
            (
                "/buckets/swfs-bench/kbf-bench/lag/n1".to_owned(),
                "0".to_owned()
            ),
            (
                "/buckets/swfs-bench/kbf-bench/lag/n1".to_owned(),
                "0".to_owned()
            ),
            (
                "/buckets/swfs-bench/kbf-bench/lag/n1".to_owned(),
                "1".to_owned()
            ),
        ]
    );

    s3.fail(Failure::Delete(1));
    let Err(Error::Refused(lines)) = run(&lag_line("3")) else {
        panic!("not refused");
    };
    let tail = &lines[lines.len() - 3..];
    assert!(tail[0].contains("2 of 3 rounds seen, 2 looks"), "{tail:?}");
    assert!(
        tail[1].starts_with("lag: the delete of kbf-bench/lag/n1/0 failed: "),
        "{tail:?}"
    );
    assert_eq!(
        tail[2],
        "lag: the filer had not seen kbf-bench/lag/n1/2 after 3 looks"
    );
}

/// Catches a flag of the verb that clashes with one of `cargo xtask` itself (its global
/// `--root` did, and the verb panicked before reading a flag): the real binary takes the
/// verb's flags and refuses an address that is not `<host>:<port>` as a usage error.
#[test]
fn the_verb_runs_from_the_command_line() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "filer-presence-bench",
            "--filer",
            "filer-host",
            "--filer-root",
            ROOT,
        ])
        .output()
        .expect("run xtask");
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned()
        ),
        (
            Some(2),
            "xtask: the filer address \"filer-host\" is not <host>:<port>\n".to_owned()
        )
    );
}
