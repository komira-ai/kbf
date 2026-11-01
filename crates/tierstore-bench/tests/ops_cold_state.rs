//! The cold store's state, read again: the source the run asks before every pass is the one
//! preflight uses (`mc admin info` and the heal status file), and each read goes to the store
//! again.

mod ops_support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tierstore_bench::coldstate::{ColdMeter, ColdStateSource, OpsColdMeter, OpsColdState};
use tierstore_bench::error::BenchError;
use tierstore_bench::result::ColdSource;
use tierstore_bench::role::Role;

use ops_support::*;

fn source(name: &str) -> (kbf_testkit::dir::TestDir, Arc<FakeHost>, OpsColdState) {
    let (dir, host, config, ops) = base(name);
    let host = Arc::new(host);
    let spec = config
        .store(tierstore_bench::role::Role::Cold)
        .unwrap()
        .clone();
    let source = OpsColdState::new(host.clone(), ops, spec, dir.path().to_owned());
    (dir, host, source)
}

/// Catches a heal state that is read once and kept: the first read finds the heal under way
/// (a healing drive, 6.4 percent), the store is then changed to a healed one, and the second
/// read says so, with the drives, the time, and nothing that names the store. Both reads asked
/// the store (`mc admin info`) and did nothing else to it.
#[test]
fn each_read_asks_the_store_again() {
    let (_dir, host, source) = source("tb-cold-state-again");
    let first = source.read().unwrap().unwrap();
    assert!(first.healing);
    assert_eq!(first.percent_done, Some(6.4));
    assert_eq!(first.drives.len(), 2);
    assert_eq!(first.drives.iter().filter(|d| d.healing).count(), 1);

    host.answer(|_| ok(&info_json("RELEASE.mark", false)));
    host.file("/private-mark/heal.json", r#"{"percent_done": 100.0}"#);
    let second = source.read().unwrap().unwrap();
    assert!(!second.healing);
    assert_eq!(second.percent_done, Some(100.0));
    assert!(second.drives.iter().all(|d| !d.healing));
    assert!(second.read_unix_ms >= first.read_unix_ms);

    let asked: Vec<String> = host.ran().iter().map(|c| c.args.join(" ")).collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked.iter().all(|a| a.ends_with("admin info tbops")),
        "{asked:?}"
    );
    let text = serde_json::to_string(&(first, second)).unwrap();
    for private in PRIVATE {
        assert!(!text.contains(private), "{private} in {text}");
    }
}

/// Catches a failing admin entry being swallowed: `mc admin info` fails, and the read is an
/// error even though a fresh and a stale status file are there to answer; a status file that is
/// not one is an error too, and a missing one.
#[test]
fn a_failing_admin_entry_is_an_error_not_a_fallback() {
    let (_dir, host, source) = source("tb-cold-state-admin-fails");
    host.answer(|_| failed(1, ""));
    let err = source.read().unwrap_err();
    assert!(matches!(err, BenchError::Ops(_)), "{err}");
    assert!(err.to_string().starts_with("cold state:"), "{err}");

    host.answer(|_| ok(&info_json("v", true)));
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/heal.json"));
    assert!(
        matches!(source.read(), Err(BenchError::Ops(_))),
        "an unreadable status file"
    );
    host.file("/private-mark/heal.json", "[]");
    let err = source.read().unwrap_err();
    assert!(err.to_string().starts_with("cold state:"), "{err}");
}

/// Catches an admin key file that cannot be read being swallowed: the admin entry names a file
/// that is not there, and the read is an error.
#[test]
fn an_admin_entry_whose_key_file_is_missing_is_an_error() {
    let (_dir, host, source) = source("tb-cold-state-admin-key-missing");
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/admin-cold.json"));
    let err = source.read().unwrap_err();
    assert!(matches!(err, BenchError::Ops(_)), "{err}");
    assert!(err.to_string().starts_with("cold state:"), "{err}");
    assert!(host.ran().is_empty());
}

/// Catches a reading with an admin entry that does not say where it came from.
#[test]
fn a_reading_with_an_admin_entry_is_from_mc() {
    let (_dir, _host, source) = source("tb-cold-state-source-mc");
    let state = source.read().unwrap().unwrap();
    assert_eq!(state.source, Some(ColdSource::Mc));
}

/// Catches a run with no admin entry and no status file reading anything: it is an error, and
/// nothing ran.
#[test]
fn no_admin_entry_and_no_status_file_is_an_error() {
    let (dir, host, config, mut ops) = base("tb-cold-state-neither");
    ops = file_mode(&ops);
    ops.cold_heal_status_file = None;
    let host = Arc::new(host);
    let spec = config.store(Role::Cold).unwrap().clone();
    let source = OpsColdState::new(host.clone(), ops, spec, dir.path().to_owned());
    let err = source.read().unwrap_err();
    assert!(err.to_string().starts_with("cold state:"), "{err}");
    assert!(host.ran().is_empty());
}

/// A source in file mode (no cold admin entry) whose status file says `text`, the clock at
/// [`NOW_MS`], a limit of 30 s, and a machine that fails any command.
fn file_source(name: &str, text: &str) -> (kbf_testkit::dir::TestDir, Arc<FakeHost>, OpsColdState) {
    let (dir, host, config, ops) = base(name);
    host.file("/private-mark/heal.json", text);
    host.answer(|_| failed(1, "mc must not run in file mode"));
    let host = Arc::new(host);
    let spec = config.store(Role::Cold).unwrap().clone();
    let source = OpsColdState::new(host.clone(), file_mode(&ops), spec, dir.path().to_owned())
        .with_max_age(Duration::from_secs(30))
        .with_clock(Arc::new(|| NOW_MS));
    (dir, host, source)
}

/// Catches file mode asking the store (a file mode that calls `mc`, even to ignore the answer),
/// a reading stamped with the time the tool read the file instead of the writer's, a reading
/// that loses the file's healing flag, percent or drives, and one that does not say `file`.
#[test]
fn file_mode_never_spawns_mc_and_stamps_the_writers_time() {
    let stamp = NOW_MS - 10_000;
    let (_dir, host, source) = file_source("tb-cold-state-file", &status_file(stamp));
    let state = source.read().unwrap().unwrap();
    assert_eq!(state.read_unix_ms, stamp);
    assert_eq!(state.source, Some(ColdSource::File));
    assert!(state.healing);
    assert_eq!(state.percent_done, Some(12.5));
    assert_eq!(state.drives.len(), 2);
    assert_eq!(state.drives[0].id, "d-0123abcd");
    assert!(state.drives[0].healing && !state.drives[1].healing);
    assert!(host.ran().is_empty(), "{:?}", host.ran());
    let text = serde_json::to_string(&state).unwrap();
    assert!(text.contains(r#""source":"file""#), "{text}");
}

/// Catches a file that says it is not healing being read as healing, and a missing percent
/// being read as an error: the optional fields are optional, `healing` is the file's own word.
#[test]
fn file_mode_takes_the_healing_flag_from_the_file_and_the_percent_is_optional() {
    let text = format!(r#"{{"read_unix_ms": {NOW_MS}, "healing": false, "drives": []}}"#);
    let (_dir, _host, source) = file_source("tb-cold-state-file-quiet", &text);
    let state = source.read().unwrap().unwrap();
    assert!(!state.healing);
    assert_eq!(state.percent_done, None);
    assert!(state.drives.is_empty());
}

/// Catches a file whose drives heal being read as quiet because its own flag says no, and a
/// percent under 100 being read as quiet: the reading is healing if any of the three says so.
#[test]
fn file_mode_is_healing_if_the_flag_a_drive_or_the_percent_says_so() {
    for text in [
        format!(
            r#"{{"read_unix_ms": {NOW_MS}, "healing": false, "drives":
                [{{"id": "d-00000000", "state": "ok", "healing": true}}]}}"#
        ),
        format!(
            r#"{{"read_unix_ms": {NOW_MS}, "healing": false, "percent_done": 99.0, "drives": []}}"#
        ),
    ] {
        let (_dir, _host, source) = file_source("tb-cold-state-file-any", &text);
        assert!(source.read().unwrap().unwrap().healing, "{text}");
    }
}

/// Catches a stale file being read as a reading, a limit that is off by one on either side, and
/// a refusal that is not the unreadable path: the file is refused when older than the limit
/// (30 s), not when exactly at it.
#[test]
fn a_stale_file_is_unreadable_and_the_limit_is_exact() {
    let (_dir, host, source) = file_source("tb-cold-state-stale", &status_file(NOW_MS - 30_000));
    assert!(source.read().is_ok(), "exactly at the limit is fresh");
    host.file("/private-mark/heal.json", &status_file(NOW_MS - 30_001));
    let err = source.read().unwrap_err();
    assert!(
        err.to_string().starts_with("cold state: unreadable"),
        "{err}"
    );
    assert!(host.ran().is_empty());
}

/// Catches a file stamped in the future being trusted (a writer whose clock is ahead, or a
/// stamp that never ages): more than 5 s ahead is refused, 5 s ahead is not.
#[test]
fn a_future_file_is_refused() {
    let (_dir, host, source) = file_source("tb-cold-state-future", &status_file(NOW_MS + 5_000));
    assert!(source.read().is_ok(), "5 s ahead is within the skew");
    host.file("/private-mark/heal.json", &status_file(NOW_MS + 5_001));
    let err = source.read().unwrap_err();
    assert!(
        err.to_string().starts_with("cold state: unreadable"),
        "{err}"
    );
    assert!(err.to_string().contains("future"), "{err}");
}

/// Catches a file with no stamp, a file that is not JSON of the schema and a missing file being
/// read: each is unreadable. A file without `healing` is not a heal state either.
#[test]
fn a_missing_or_malformed_file_is_unreadable() {
    let cases = [
        r#"{"healing": false, "drives": []}"#.to_owned(),
        format!(r#"{{"read_unix_ms": {NOW_MS}, "drives": []}}"#),
        format!(r#"{{"read_unix_ms": "{NOW_MS}", "healing": false, "drives": []}}"#),
        r#"{"read_unix_ms": -1, "healing": false, "drives": []}"#.to_owned(),
        r#"{"percent_done": 100.0}"#.to_owned(),
        "[]".to_owned(),
        "not json".to_owned(),
    ];
    for text in cases {
        let (_dir, host, source) = file_source("tb-cold-state-bad", &text);
        let err = source.read().unwrap_err();
        assert!(
            err.to_string().starts_with("cold state: unreadable"),
            "{text}: {err}"
        );
        assert!(host.ran().is_empty());
    }
    let (_dir, host, source) = file_source("tb-cold-state-gone", &status_file(NOW_MS));
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/heal.json"));
    let err = source.read().unwrap_err();
    assert!(
        err.to_string().starts_with("cold state: unreadable"),
        "{err}"
    );
}

/// Catches the bounded read taking another path than the plain one in file mode: the same
/// reading, from the file, with nothing run.
#[tokio::test]
async fn the_bounded_read_in_file_mode_is_the_plain_read() {
    let (_dir, host, source) = file_source("tb-cold-state-file-bounded", &status_file(NOW_MS));
    let state = source
        .read_within(Duration::from_secs(600))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.source, Some(ColdSource::File));
    assert!(host.ran().is_empty());
}

/// Catches a reading that is waited for for ever: `mc admin info` never ends, and the read gives
/// up after its limit with an error (the run stops, as for any reading that cannot be made), the
/// command having been told to stop, and the status file that would have answered is not used.
/// The clock is paused, so the limit passes at once and nothing here waits for it.
#[tokio::test(start_paused = true)]
async fn a_reading_that_never_answers_is_stopped_and_is_unreadable() {
    let (dir, host, config, ops) = base("tb-cold-state-hangs");
    host.hang.store(true, Ordering::SeqCst);
    let host = Arc::new(host);
    let spec = config
        .store(tierstore_bench::role::Role::Cold)
        .unwrap()
        .clone();
    let source = OpsColdState::new(host.clone(), ops, spec, dir.path().to_owned());
    let limit = Duration::from_secs(30);
    let read = source.read_within(limit);
    tokio::pin!(read);
    // Polled once, the read has started the command and the timer. The paused clock does not
    // advance by itself while a thread of the blocking pool works, so the test advances it past
    // the limit by hand.
    tokio::select! {
        biased;
        _ = &mut read => panic!("the read ended before its limit"),
        () = tokio::task::yield_now() => {}
    }
    tokio::time::advance(limit + Duration::from_secs(1)).await;
    let err = read.await.unwrap_err();
    assert!(matches!(err, BenchError::Ops(_)), "{err}");
    assert!(
        err.to_string().starts_with("cold state: unreadable"),
        "{err}"
    );
    assert!(
        host.stopped.load(Ordering::SeqCst),
        "the command was told to stop"
    );
    assert_eq!(host.ran().len(), 1, "only the admin info was started");
}

/// Catches a bounded read that gives a different answer from the plain one when the store
/// answers: same reading, the store asked once.
#[tokio::test]
async fn a_reading_that_answers_in_time_is_the_plain_reading() {
    let (_dir, host, source) = source("tb-cold-state-in-time");
    let read = source
        .read_within(Duration::from_secs(600))
        .await
        .unwrap()
        .unwrap();
    assert!(read.healing);
    assert_eq!(read.percent_done, Some(6.4));
    assert_eq!(read.drives.len(), 2);
    assert_eq!(host.ran().len(), 1);
}

/// The Prometheus text of a store that has served 5 gets and 7 puts, with a series of another
/// name beside them.
const METRICS: &str = "# HELP minio_s3_requests_total Total number of s3 requests\n\
# TYPE minio_s3_requests_total counter\n\
minio_s3_requests_total{api=\"getobject\",server=\"s:9000\"} 5\n\
minio_s3_requests_total{api=\"putobject\",server=\"s:9000\"} 7\n\
minio_s3_requests_errors_total{api=\"putobject\",server=\"s:9000\"} 100\n";

/// A meter over a host that answers the metrics command with `metrics` and the rest with the
/// admin info of a store that is not healing.
fn meter_over(name: &str, metrics: &'static str) -> (Arc<FakeHost>, OpsColdMeter) {
    let (dir, host, config, ops) = base(name);
    host.answer(move |cmd| {
        if cmd.args.iter().any(|a| a == "prometheus") {
            ok(metrics)
        } else {
            ok(&info_json("RELEASE.mark", false))
        }
    });
    host.file("/private-mark/heal.json", r#"{"percent_done": 100.0}"#);
    let host = Arc::new(host);
    let spec = config.store(Role::Cold).unwrap().clone();
    let state = OpsColdState::new(host.clone(), ops, spec, dir.path().to_owned());
    (host, OpsColdMeter::new(state, Duration::ZERO))
}

/// Catches a meter that reads the state but not the counter (or the other way round), that asks
/// for the counter in JSON (the answer is Prometheus text) or with a write, and one whose
/// reading names the store: the counter is the sum of the request series, the state is the
/// admin info's, and the two commands are the read-only ones.
#[tokio::test]
async fn the_meter_reads_the_request_counter_and_the_state_with_read_only_commands() {
    let (host, meter) = meter_over("tb-cold-meter", METRICS);
    let reading = meter.read(Duration::from_secs(600)).await.unwrap();
    assert_eq!(reading.s3_requests, 12);
    assert!(!reading.state.healing);
    assert_eq!(reading.state.drives.len(), 2);
    assert_eq!(reading.state.source, Some(ColdSource::Mc));
    let asked: Vec<String> = host.ran().iter().map(|c| c.args.join(" ")).collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked[0].ends_with("admin prometheus metrics tbops cluster"),
        "{asked:?}"
    );
    assert!(!asked[0].contains("--json"), "{asked:?}");
    assert!(asked[1].ends_with("admin info tbops"), "{asked:?}");
    let text = format!("{reading:?}");
    for private in PRIVATE {
        assert!(!text.contains(private), "{private} in {text}");
    }
}

/// Catches a counter that cannot be read being taken for a quiet store: a metrics command that
/// fails, and an answer with no request series, are errors with a reason, not zero.
#[tokio::test]
async fn a_counter_that_cannot_be_read_is_an_error_with_a_reason() {
    let (host, meter) = meter_over("tb-cold-meter-fails", METRICS);
    host.answer(|_| failed(1, "no route"));
    let err = meter.read(Duration::from_secs(600)).await.unwrap_err();
    assert!(err.contains("mc admin prometheus metrics"), "{err}");
    assert!(err.contains("exited"), "{err}");
    let (_host, meter) = meter_without_series();
    let err = meter.read(Duration::from_secs(600)).await.unwrap_err();
    assert!(err.contains("minio_s3_requests_total"), "{err}");
}

fn meter_without_series() -> (Arc<FakeHost>, OpsColdMeter) {
    meter_over("tb-cold-meter-no-series", "# nothing here\nminio_up 1\n")
}

/// Catches a reading whose second half is skipped when it fails: the state read after the
/// counter (the admin info, the heal status file) and the admin key each have their own error,
/// and none gives a half reading.
#[tokio::test]
async fn a_failing_state_or_key_is_an_error_not_a_half_reading() {
    let (host, meter) = meter_over("tb-cold-meter-info-fails", METRICS);
    host.answer(|cmd| {
        if cmd.args.iter().any(|a| a == "prometheus") {
            ok(METRICS)
        } else {
            failed(1, "no route")
        }
    });
    let err = meter.read(Duration::from_secs(600)).await.unwrap_err();
    assert!(err.contains("mc admin info"), "{err}");

    let (host, meter) = meter_over("tb-cold-meter-heal-file", METRICS);
    host.file("/private-mark/heal.json", "not json");
    let err = meter.read(Duration::from_secs(600)).await.unwrap_err();
    assert!(err.contains("heal status file"), "{err}");

    let (dir, host, config, ops) = base("tb-cold-meter-no-key");
    let host = Arc::new(host);
    let spec = config.store(Role::Cold).unwrap().clone();
    let state = OpsColdState::new(host.clone(), file_mode(&ops), spec, dir.path().to_owned());
    let err = OpsColdMeter::new(state, Duration::ZERO)
        .read(Duration::from_secs(600))
        .await
        .unwrap_err();
    assert!(err.contains("no admin key file"), "{err}");
    assert!(host.ran().is_empty(), "nothing ran without a key");
}
