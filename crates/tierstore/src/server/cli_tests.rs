//! The flags of the binary, the settings made of them, and the reconciler's loop.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::{CommandFactory, Parser};
use kbf_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::{
    Backend, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, Reconciler, ReconcilerConfig,
    TestClock,
};

use super::{Cli, build, parse_duration, read_key, reconcile, share, tier};

const MEBIBYTE: u64 = 1 << 20;

/// The flags every start needs, with `overrides` in place of the flags they name or added.
fn flags(key_file: &str, overrides: &[(&str, &str)]) -> Vec<String> {
    let owned = |(flag, value): (&str, &str)| (flag.to_owned(), value.to_owned());
    let mut pairs: Vec<(String, String)> = [
        ("--listen", "127.0.0.1:0"),
        ("--proxy-id", "p1"),
        ("--hot", "memory"),
        ("--cold", "memory"),
        ("--hot-bytes", "1000"),
        ("--bucket", "tier-bucket"),
        ("--access-key-file", key_file),
    ]
    .into_iter()
    .map(owned)
    .collect();
    for (flag, value) in overrides {
        let taken = pairs.iter_mut().find(|(name, _)| name == flag);
        match taken {
            Some(slot) => slot.1 = (*value).to_owned(),
            None => pairs.push(owned((*flag, *value))),
        }
    }
    let mut args = vec!["tierstore".to_owned()];
    for (flag, value) in pairs {
        args.push(flag);
        args.push(value);
    }
    args
}

/// A key file in `dir`, called `name`, with this text and mode.
fn key_file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// The flags of a start with a good key file in `dir`.
fn cli(dir: &TestDir) -> Cli {
    let path = key_file(dir, "key", "AKID\nsecret\n", 0o600);
    Cli::try_parse_from(flags(path.to_str().unwrap(), &[])).unwrap()
}

/// Asserts that a duration is a whole number with a unit of `ms`, `s`, `m` or `h`, and that a
/// number without a unit, a unit without a number, other units, fractions, signs, white space
/// and numbers too large are refused with a message that says what is expected.
#[test]
fn a_duration_is_a_number_and_a_unit() {
    let valid = [
        ("0s", 0),
        ("250ms", 250),
        ("10s", 10_000),
        ("15m", 900_000),
        ("2h", 7_200_000),
        ("007ms", 7),
    ];
    for (text, millis) in valid {
        let want = Duration::from_millis(millis);
        assert_eq!(parse_duration(text), Ok(want), "{text}");
    }
    let invalid = [
        "",
        "10",
        "ms",
        "10x",
        "1.5s",
        "-5s",
        " 5s",
        "18446744073709551615h",
        "99999999999999999999999s",
    ];
    for text in invalid {
        let why = parse_duration(text).unwrap_err();
        assert!(why.contains("is not a duration"), "{text}: {why}");
    }
}

/// Asserts that the flags that have defaults have them: the water marks at 80, 70 and 95
/// percent, a drain of 10 seconds and 15 minutes, a journal linger of 20 milliseconds, and so on.
#[test]
fn the_flags_have_their_defaults() {
    let cli = Cli::try_parse_from(flags("key", &[])).unwrap();
    assert_eq!(cli.listen.to_string(), "127.0.0.1:0");
    assert_eq!(cli.proxy_id, "p1");
    assert_eq!(cli.hot, "memory");
    assert_eq!(cli.cold, "memory");
    assert_eq!(cli.hot_bytes, 1000);
    assert_eq!(cli.bucket, "tier-bucket");
    assert_eq!(cli.region, "us-east-1");
    assert_eq!((cli.high_water, cli.low_water, cli.critical), (80, 70, 95));
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(10));
    assert_eq!(cli.drain_deadline, Duration::from_secs(900));
    assert_eq!(cli.journal_linger, Duration::from_millis(20));
    assert_eq!(cli.journal_retention, Duration::from_secs(24 * 3600));
    assert_eq!(cli.copy_parallel, 16);
    assert_eq!(cli.lease_ttl, Duration::from_secs(30));
    assert_eq!(cli.reconcile_interval, Duration::from_secs(5));
    assert_eq!(cli.max_request_bytes, 256 << 20);
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(30));
}

/// Asserts that every flag that has a default can be given, and is read.
#[test]
fn the_flags_can_all_be_given() {
    let given = [
        ("--region", "eu-west-1"),
        ("--high-water", "90"),
        ("--low-water", "50"),
        ("--critical", "100"),
        ("--drain-lb-wait", "2s"),
        ("--drain-deadline", "3m"),
        ("--journal-linger", "5ms"),
        ("--journal-retention", "2h"),
        ("--copy-parallel", "4"),
        ("--lease-ttl", "20s"),
        ("--reconcile-interval", "1s"),
        ("--max-request-bytes", "10"),
        ("--body-idle-timeout", "7s"),
    ];
    let cli = Cli::try_parse_from(flags("key", &given)).unwrap();
    assert_eq!(cli.region, "eu-west-1");
    let marks = (cli.high_water, cli.low_water, cli.critical);
    assert_eq!(marks, (90, 50, 100));
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(cli.drain_deadline, Duration::from_secs(180));
    assert_eq!(cli.journal_linger, Duration::from_millis(5));
    assert_eq!(cli.journal_retention, Duration::from_secs(7200));
    assert_eq!(cli.copy_parallel, 4);
    assert_eq!(cli.lease_ttl, Duration::from_secs(20));
    assert_eq!(cli.reconcile_interval, Duration::from_secs(1));
    assert_eq!(cli.max_request_bytes, 10);
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(7));
}

/// Asserts that flags that are not valid are refused when they are read: one that is not known,
/// a water mark of zero or over 100, a size or a count of zero, a duration that is not one, an
/// address that is not one, and a flag that is missing.
#[test]
fn flags_that_are_not_valid_are_refused() {
    let parse = |overrides: &[(&str, &str)]| Cli::try_parse_from(flags("key", overrides));
    assert!(parse(&[]).is_ok());
    let bad = [
        ("--no-such-flag", "1"),
        ("--high-water", "0"),
        ("--low-water", "101"),
        ("--critical", "x"),
        ("--hot-bytes", "0"),
        ("--max-request-bytes", "0"),
        ("--copy-parallel", "0"),
        ("--drain-lb-wait", "soon"),
        ("--listen", "nowhere"),
    ];
    for (flag, value) in bad {
        assert!(parse(&[(flag, value)]).is_err(), "{flag} {value}");
    }
    let mut args = flags("key", &[]);
    let at = args.iter().position(|arg| arg == "--bucket").unwrap();
    args.remove(at + 1);
    args.remove(at);
    assert!(Cli::try_parse_from(args).is_err());
}

/// Asserts that the help names every flag, and that the version names the package version.
#[test]
fn the_help_names_every_flag_and_the_version_the_package() {
    let help = Cli::command().render_long_help().to_string();
    let names = [
        "--listen",
        "--proxy-id",
        "--hot",
        "--cold",
        "--hot-bytes",
        "--high-water",
        "--low-water",
        "--critical",
        "--bucket",
        "--region",
        "--access-key-file",
        "--drain-lb-wait",
        "--drain-deadline",
        "--journal-linger",
        "--journal-retention",
        "--copy-parallel",
        "--lease-ttl",
        "--reconcile-interval",
        "--max-request-bytes",
        "--body-idle-timeout",
    ];
    for name in names {
        assert!(help.contains(name), "{name}");
    }
    let version = Cli::try_parse_from(["tierstore", "--version"]);
    let shown = version.unwrap_err().to_string();
    assert!(shown.contains(env!("CARGO_PKG_VERSION")), "{shown}");
}

/// Asserts that percents are shares of a whole.
#[test]
fn a_percent_is_a_share() {
    assert_eq!(share(80), 0.8);
    assert_eq!(share(95), 0.95);
    assert_eq!(share(100), 1.0);
}

/// Asserts that the settings are made from the flags: the key file's key and secret, the
/// bucket and region, the limits, the water marks as shares, the id of the proxy as the holder
/// of the lease, and the durations.
#[test]
fn the_settings_are_made_from_the_flags() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-build");
    let mut cli = cli(&dir);
    cli.region = "eu-west-1".to_owned();
    let built = build(&cli).expect("built");
    assert_eq!(built.server.bucket, "tier-bucket");
    assert_eq!(built.server.region, "eu-west-1");
    assert_eq!(built.server.access_key_id, "AKID");
    assert_eq!(built.server.secret_access_key, "secret");
    assert_eq!(built.server.max_request_bytes, 256 << 20);
    assert_eq!(built.server.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(built.server.drain_lb_wait, Duration::from_secs(10));
    assert_eq!(built.server.drain_deadline, Duration::from_secs(900));
    assert_eq!(built.proxy.proxy_id, "p1");
    assert_eq!(built.proxy.hot_budget, 1000);
    assert_eq!(built.proxy.critical, 0.95);
    assert_eq!(built.proxy.linger, Duration::from_millis(20));
    assert_eq!(built.reconciler.holder, "p1");
    assert_eq!(built.reconciler.hot_budget, 1000);
    assert_eq!(built.reconciler.high_water, 0.8);
    assert_eq!(built.reconciler.low_water, 0.7);
    assert_eq!(built.reconciler.parallel, 16);
    assert_eq!(built.reconciler.lease_ttl, Duration::from_secs(30));
    assert_eq!(built.reconciler.retention, Duration::from_secs(24 * 3600));
}

/// Asserts that flags that do not fit together are refused with a message that names them: the
/// water marks out of order, a reconcile interval that is not shorter than the lease, a bucket
/// called like a probe path, an idle timeout of zero, a tier that cannot be opened, and a key
/// file that cannot be used.
#[test]
fn flags_that_do_not_fit_together_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-refused");
    let refused = |change: &dyn Fn(&mut Cli)| {
        let mut cli = cli(&dir);
        change(&mut cli);
        build(&cli).err().expect("refused")
    };
    assert!(refused(&|cli| cli.low_water = 80).contains("--low-water"));
    assert!(refused(&|cli| cli.high_water = 96).contains("--high-water"));
    let interval = refused(&|cli| cli.reconcile_interval = cli.lease_ttl);
    assert!(interval.contains("--reconcile-interval"), "{interval}");
    assert!(refused(&|cli| cli.bucket = "readyz".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "healthz".to_owned()).contains("probe path"));
    let idle = refused(&|cli| cli.body_idle_timeout = Duration::ZERO);
    assert!(idle.contains("--body-idle-timeout"), "{idle}");
    let hot = refused(&|cli| cli.hot = "s3://hot".to_owned());
    assert!(hot.contains("--hot"), "{hot}");
    let cold = refused(&|cli| cli.cold = "/var/cold".to_owned());
    assert!(cold.contains("--cold"), "{cold}");
    let missing = dir.path().join("missing");
    let key = refused(&|cli| cli.access_key_file = missing.clone());
    assert!(key.contains("--access-key-file"), "{key}");
}

/// Asserts that a memory tier and a directory tier open, the directory being made, and that any
/// other kind, and a directory that has no name or cannot be made, is refused with the flag's
/// name and the kind it was given.
#[test]
fn only_a_memory_or_a_directory_tier_opens() {
    assert!(tier("--hot", "memory").is_ok());
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-tiers");
    let made = dir.path().join("made/here");
    assert!(tier("--hot", &format!("dir:{}", made.display())).is_ok());
    assert!(made.join(".uploads").is_dir());
    let why = tier("--cold", "s3://bucket").err().unwrap();
    assert!(why.contains("--cold"), "{why}");
    assert!(why.contains("s3://bucket"), "{why}");
    let unnamed = tier("--cold", "dir:").err().unwrap();
    assert!(unnamed.contains("--cold"), "{unnamed}");
    let file = key_file(&dir, "file", "x", 0o600);
    let under = format!("dir:{}/inside", file.display());
    let blocked = tier("--hot", &under).err().unwrap();
    assert!(blocked.contains("--hot"), "{blocked}");
}

/// Asserts that the key file is read as an id on the first line and a secret on the second,
/// trimmed, with either line ending; that others may not read it; and that a file with a
/// missing or empty line, a missing file and a directory are refused with the flag's name.
#[test]
fn the_key_file_is_an_id_and_a_secret_that_only_its_owner_reads() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-key");
    let want = Ok(("AKID".to_owned(), "secret".to_owned()));
    let spaced = key_file(&dir, "spaced", "  AKID \n secret \n", 0o600);
    assert_eq!(read_key(&spaced), want);
    let crlf = key_file(&dir, "crlf", "AKID\r\nsecret\r\n", 0o600);
    assert_eq!(read_key(&crlf), want);
    let open = key_file(&dir, "open", "AKID\nsecret\n", 0o644);
    let why = read_key(&open).unwrap_err();
    assert!(why.contains("others can read it"), "{why}");
    assert!(why.contains("open"), "{why}");
    let incomplete = [
        ("one", "AKID\n"),
        ("first", "\nsecret\n"),
        ("second", "AKID\n\n"),
    ];
    for (name, text) in incomplete {
        let path = key_file(&dir, name, text, 0o600);
        let why = read_key(&path).unwrap_err();
        assert!(why.contains("--access-key-file"), "{why}");
        assert!(why.contains("on the second"), "{why}");
    }
    let missing = read_key(&dir.path().join("missing")).unwrap_err();
    assert!(missing.contains("--access-key-file"), "{missing}");
    let folder = dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, Permissions::from_mode(0o700)).unwrap();
    let unreadable = read_key(&folder).unwrap_err();
    assert!(unreadable.contains("--access-key-file"), "{unreadable}");
}

/// Asserts that the reconciler's loop works at once and then at every interval, tries again at
/// the next interval a copy that failed, and gives its lease up when it is halted: the copy that
/// failed in the first pass is made in the second, and the lease is gone after the halt.
#[tokio::test(start_paused = true)]
async fn the_reconciler_works_every_interval_until_halted() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.fail_next(Op::Put, 1);
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(cold_mem.get("k", None).await.is_err());
        tokio::time::sleep(Duration::from_secs(5)).await;
        let copied = cold_mem.get("k", None).await.unwrap();
        assert_eq!(copied, Bytes::from_static(b"v"));
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(hot.get("_tiering/lease", None).await.is_err());
}

/// Asserts that a halt cuts a pass that is still running and gives the lease up at once: a copy
/// to a cold tier that takes an hour is not waited for, the object is not copied, and the lease
/// is gone.
#[tokio::test(start_paused = true)]
async fn a_halt_cuts_the_pass_that_is_running() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    let both = async { tokio::join!(working, watching) };
    let done = tokio::time::timeout(Duration::from_secs(60), both).await;
    assert!(done.is_ok());
    assert!(hot.get("_tiering/lease", None).await.is_err());
    assert!(cold_mem.get("k", None).await.is_err());
}
