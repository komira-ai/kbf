//! The flags of the binary, the settings made of them, and the reconciler's loop.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::{CommandFactory, Parser};
use kbf_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::selftest_tests::{Flaw, Flawed};
use crate::server::{AccessKey, Drained};
use crate::{
    Backend, Check, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, Reconciler,
    ReconcilerConfig, SelfTestError, SelfTestFailure, TestClock,
};

use super::{
    Cli, InvalidFlags, S3Flags, SkippedCheck, build, cut_note, exit_status, open_tier,
    parse_duration, parse_skip, probe_run_id, probe_tiers, read_key, reconcile, share, tier,
};

const MEBIBYTE: u64 = 1 << 20;

/// The JSON form of a key file with the id `JSONID` and the secret `json-secret`.
const JSON_KEY: &str = r#"{"access_key_id": "JSONID", "secret_access_key": "json-secret"}"#;

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
    assert_eq!(cli.usage_stale_after, Duration::from_secs(120));
    assert_eq!(cli.reconcile_interval, Duration::from_secs(5));
    assert_eq!(cli.max_request_bytes, 256 << 20);
    assert_eq!(cli.max_concurrent_puts, 32);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(15));
    assert_eq!(cli.put_min_bytes_per_sec, 1024);
    assert_eq!(cli.put_rate_grace, Duration::from_secs(30));
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(60));
    let s3_flags = [
        cli.hot_endpoint.is_some(),
        cli.hot_region.is_some(),
        cli.hot_credentials_file.is_some(),
        cli.cold_endpoint.is_some(),
        cli.cold_region.is_some(),
        cli.cold_credentials_file.is_some(),
    ];
    assert!(!s3_flags.contains(&true), "no S3 flag by default");
    assert!(cli.read_only_key_file.is_empty());
    assert!(cli.presign_key_file.is_empty());
    assert!(cli.gc_key_file.is_empty());
    assert!(cli.scoped_key_file.is_empty());
    assert_eq!(cli.presign_max_expires, Duration::from_secs(900));
    assert!(
        cli.skip_backend_check.is_empty(),
        "no check is skipped by default"
    );
    assert_eq!(cli.backend_check_wait, Duration::from_secs(300));
}

/// Asserts that `--read-only-key-file` can be given any number of times, and that the files are
/// kept in the order of the flags.
#[test]
fn the_read_only_key_flag_is_repeatable() {
    let mut args = flags("key", &[]);
    let repeated = [
        "--read-only-key-file",
        "first",
        "--read-only-key-file",
        "second",
    ];
    args.extend(repeated.map(str::to_owned));
    let cli = Cli::try_parse_from(args).unwrap();
    let files = [PathBuf::from("first"), PathBuf::from("second")];
    assert_eq!(cli.read_only_key_file, files);
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
        ("--usage-stale-after", "45s"),
        ("--reconcile-interval", "1s"),
        ("--max-request-bytes", "10"),
        ("--max-concurrent-puts", "3"),
        ("--put-permit-wait", "4s"),
        ("--put-min-bytes-per-sec", "5"),
        ("--put-rate-grace", "2s"),
        ("--body-idle-timeout", "7s"),
        ("--s3-request-timeout", "90s"),
        ("--backend-check-wait", "2m"),
        ("--hot-endpoint", "http://127.0.0.1:9000"),
        ("--hot-region", "eu-west-1"),
        ("--hot-credentials-file", "hot-key"),
        ("--cold-endpoint", "http://127.0.0.1:9001"),
        ("--cold-region", "eu-west-2"),
        ("--cold-credentials-file", "cold-key"),
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
    assert_eq!(cli.usage_stale_after, Duration::from_secs(45));
    assert_eq!(cli.reconcile_interval, Duration::from_secs(1));
    assert_eq!(cli.max_request_bytes, 10);
    assert_eq!(cli.max_concurrent_puts, 3);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(4));
    assert_eq!(cli.put_min_bytes_per_sec, 5);
    assert_eq!(cli.put_rate_grace, Duration::from_secs(2));
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(7));
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(90));
    assert_eq!(cli.backend_check_wait, Duration::from_secs(120));
    let hot = cli.hot_s3();
    assert_eq!(hot.endpoint, Some("http://127.0.0.1:9000"));
    assert_eq!(hot.region, Some("eu-west-1"));
    assert_eq!(hot.credentials, Some(Path::new("hot-key")));
    let cold = cli.cold_s3();
    assert_eq!(cold.endpoint, Some("http://127.0.0.1:9001"));
    assert_eq!(cold.region, Some("eu-west-2"));
    assert_eq!(cold.credentials, Some(Path::new("cold-key")));
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
        "--config",
        "--print-config",
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
        "--read-only-key-file",
        "--drain-lb-wait",
        "--drain-deadline",
        "--journal-linger",
        "--journal-retention",
        "--copy-parallel",
        "--lease-ttl",
        "--reconcile-interval",
        "--max-request-bytes",
        "--body-idle-timeout",
        "--s3-request-timeout",
        "--hot-endpoint",
        "--hot-region",
        "--hot-credentials-file",
        "--cold-endpoint",
        "--cold-region",
        "--cold-credentials-file",
        "--skip-backend-check",
        "--backend-check-wait",
    ];
    for name in names {
        assert!(help.contains(name), "{name}");
    }
    let version = Cli::try_parse_from(["tierstore", "--version"]);
    let shown = version.unwrap_err().to_string();
    assert!(shown.contains(env!("CARGO_PKG_VERSION")), "{shown}");
}

/// Asserts that a storage node's file reaches the built server settings: the front of the file
/// of `GOLDEN` (a `put_permit_wait` of 25 s, not the 15 s a `ServerConfig` has by itself), given
/// a key file that exists, makes a server whose wait for a put permit is 25 s.
#[test]
fn the_put_permit_wait_of_a_file_reaches_the_built_server() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-permit-wait-file");
    let from_file = crate::server::config::from_text(
        crate::server::config_testing::GOLDEN,
        crate::server::config_testing::DEFAULTS,
    )
    .unwrap();
    let mut front = from_file.front();
    assert_eq!(front.put_permit_wait, Duration::from_secs(25));
    front.access_key_file = key_file(&dir, "key", "AKID\nsecret\n", 0o600);
    front.read_only_key_file = Vec::new();
    front.presign_key_file = Vec::new();
    front.gc_key_file = Vec::new();
    front.scoped_key_file = Vec::new();
    let server = crate::server::front::server_config(&front, Arc::new(crate::SystemClock))
        .expect("a server");
    assert_eq!(server.put_permit_wait, Duration::from_secs(25));
}

/// Asserts that percents are shares of a whole.
#[test]
fn a_percent_is_a_share() {
    assert_eq!(share(80), 0.8);
    assert_eq!(share(95), 0.95);
    assert_eq!(share(100), 1.0);
}

/// Asserts that the settings are made from the flags: the key file's key and secret, the
/// read-only keys of their files (one in each form) in the order of the flags, the bucket and
/// region, the limits, the water marks as shares, the id of the proxy as the holder of the
/// lease, and the durations, the lengths of the two parts of the drain (given values that are not
/// the defaults) among them.
#[test]
fn the_settings_are_made_from_the_flags() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-build");
    let mut cli = cli(&dir);
    cli.region = "eu-west-1".to_owned();
    cli.drain_lb_wait = Duration::from_secs(7);
    cli.drain_deadline = Duration::from_secs(11);
    cli.put_permit_wait = Duration::from_secs(4);
    let lines = key_file(&dir, "reader-lines", "READ1\nread-secret-1\n", 0o600);
    let json = key_file(&dir, "reader-json", JSON_KEY, 0o400);
    cli.read_only_key_file = vec![lines, json];
    let built = build(&cli).expect("built");
    let readers = [
        AccessKey::new("READ1", "read-secret-1"),
        AccessKey::new("JSONID", "json-secret"),
    ];
    assert_eq!(built.server.read_only_keys, readers);
    assert_eq!(built.server.bucket, "tier-bucket");
    assert_eq!(built.server.region, "eu-west-1");
    assert_eq!(built.server.access_key_id, "AKID");
    assert_eq!(built.server.secret_access_key, "secret");
    assert_eq!(built.server.max_request_bytes, 256 << 20);
    assert_eq!(built.server.max_concurrent_puts, 32);
    assert_eq!(built.server.put_permit_wait, Duration::from_secs(4));
    assert_eq!(built.server.put_min_bytes_per_sec, 1024);
    assert_eq!(built.server.put_rate_grace, Duration::from_secs(30));
    assert_eq!(built.server.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(built.server.drain_lb_wait, Duration::from_secs(7));
    assert_eq!(built.server.drain_deadline, Duration::from_secs(11));
    assert_eq!(built.server.usage_stale_after, Duration::from_secs(120));
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
/// called like a probe path, an idle timeout of zero, a request timeout of zero for the S3
/// tiers, a tier that cannot be opened, and a key file, of the full key or of a read-only one,
/// that cannot be used. None of them is `InvalidFlags`, the refusal that exits with status 2.
#[test]
fn flags_that_do_not_fit_together_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-refused");
    let refused = |change: &dyn Fn(&mut Cli)| {
        let mut cli = cli(&dir);
        change(&mut cli);
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_none(), "{why}");
        why.to_string()
    };
    assert!(refused(&|cli| cli.low_water = 80).contains("--low-water"));
    assert!(refused(&|cli| cli.high_water = 96).contains("--high-water"));
    let interval = refused(&|cli| cli.reconcile_interval = cli.lease_ttl);
    assert!(interval.contains("--reconcile-interval"), "{interval}");
    let stale = refused(&|cli| cli.usage_stale_after = cli.lease_ttl);
    assert!(stale.contains("--usage-stale-after"), "{stale}");
    assert!(refused(&|cli| cli.bucket = "readyz".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "healthz".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "usagez".to_owned()).contains("probe path"));
    let idle = refused(&|cli| cli.body_idle_timeout = Duration::ZERO);
    assert!(idle.contains("--body-idle-timeout"), "{idle}");
    let wait = refused(&|cli| cli.s3_request_timeout = Duration::ZERO);
    assert!(wait.contains("--s3-request-timeout"), "{wait}");
    let hot = refused(&|cli| cli.hot = "s3://hot".to_owned());
    assert!(hot.contains("--hot"), "{hot}");
    let cold = refused(&|cli| cli.cold = "/var/cold".to_owned());
    assert!(cold.contains("--cold"), "{cold}");
    let missing = dir.path().join("missing");
    let key = refused(&|cli| cli.access_key_file = missing.clone());
    assert!(key.contains("--access-key-file"), "{key}");
    let reader = refused(&|cli| cli.read_only_key_file = vec![missing.clone()]);
    assert!(reader.contains("--read-only-key-file"), "{reader}");
    assert!(reader.contains("missing"), "{reader}");
}

/// Asserts that two keys with one id are refused with a message that names the id and the two
/// flags and not the secrets, and as flags that cannot go together (exit status 2): a read-only
/// key with the id of the full key, and two read-only keys with one id. The start is refused
/// before a tier is opened: a directory tier is not made.
#[test]
fn two_keys_with_one_id_are_invalid_flags_refused_before_a_tier_is_opened() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-twice");
    let same = key_file(&dir, "same", "AKID\nanother-secret\n", 0o600);
    let reader = key_file(&dir, "reader", "READ1\nread-secret-1\n", 0o600);
    let again = key_file(&dir, "again", JSON_KEY, 0o600);
    let json_twice = key_file(&dir, "twice", JSON_KEY, 0o600);
    let made = dir.path().join("made");
    let cases = [
        vec![same],
        vec![reader.clone(), again, json_twice],
        vec![reader.clone(), reader],
    ];
    for files in cases {
        let mut cli = cli(&dir);
        cli.hot = format!("dir:{}", made.display());
        cli.read_only_key_file = files;
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_some(), "{why}");
        let text = why.to_string();
        assert!(text.contains("used twice"), "{text}");
        assert!(text.contains("--access-key-file"), "{text}");
        assert!(text.contains("--read-only-key-file"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!made.exists());
    }
}

/// Asserts that `--scoped-key-file` puts the keys of its files on the server, in the order of the
/// flags; that a file that is not a valid scoped key is refused with a message that names the flag
/// (as a bad key file is, not as invalid flags); and that a scoped key whose id is the full
/// key's, a read-only key's, a GC key's or another scoped key's is invalid flags (status 2).
#[test]
fn scoped_key_files_are_built_into_the_server_and_their_ids_must_differ() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-flag");
    let text = |id: &str| {
        format!(
            "{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"s3cret\", \
             \"read_prefixes\": [\"cas/\"], \"write_prefixes\": [\"cas/f/a/\"], \
             \"delete_prefixes\": []}}"
        )
    };
    let one = key_file(&dir, "one", &text("SC1"), 0o600);
    let two = key_file(&dir, "two", &text("SC2"), 0o600);
    let mut ok = cli(&dir);
    ok.scoped_key_file = vec![one.clone(), two.clone()];
    let built = build(&ok).expect("built");
    let ids: Vec<&str> = built
        .server
        .scoped_keys
        .iter()
        .map(|key| key.id())
        .collect();
    assert_eq!(ids, ["SC1", "SC2"]);
    assert_eq!(built.server.scoped_keys[0].write_prefixes(), ["cas/f/a/"]);

    let empty = key_file(
        &dir,
        "empty",
        &text("SC3")
            .replace("[\"cas/\"]", "[]")
            .replace("[\"cas/f/a/\"]", "[]"),
        0o600,
    );
    let mut bad = cli(&dir);
    bad.scoped_key_file = vec![empty];
    let why = build(&bad).err().expect("refused");
    assert!(why.to_string().starts_with("--scoped-key-file "), "{why}");
    assert!(why.downcast_ref::<InvalidFlags>().is_none(), "{why}");

    let full = key_file(&dir, "full", &text("AKID"), 0o600);
    let reader = key_file(&dir, "reader", "SC1\nread-secret\n", 0o600);
    let gc = key_file(
        &dir,
        "gc",
        r#"{"access_key_id": "SC1", "secret_access_key": "g", "prefix": "cas/b/"}"#,
        0o600,
    );
    for (scoped, setup) in [
        (vec![full], 0),
        (vec![one.clone()], 1),
        (vec![one.clone()], 2),
        (vec![one.clone(), one], 0),
    ] {
        let mut cli = cli(&dir);
        cli.scoped_key_file = scoped;
        match setup {
            1 => cli.read_only_key_file = vec![reader.clone()],
            2 => cli.gc_key_file = vec![gc.clone()],
            _ => {}
        }
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_some(), "{why}");
        let text = why.to_string();
        assert!(
            text.contains("used twice") && !text.contains("s3cret"),
            "{text}"
        );
    }
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

/// Asserts that a tier `s3:<bucket>` is built from the flags of its tier, each tier from its
/// own: the settings are made without a request, the region is not needed, and the two tiers
/// have a key file of their own, the hot one in the JSON form and the cold one in the two-line
/// form, and then the other way round.
#[test]
fn an_s3_tier_is_built_from_the_flags_of_its_tier() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-s3-tiers");
    let hot_key = key_file(&dir, "hot-key", JSON_KEY, 0o600);
    let cold_key = key_file(&dir, "cold-key", "COLDID\ncold-secret\n", 0o600);
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.hot_endpoint = Some("http://127.0.0.1:9000".to_owned());
    cli.hot_credentials_file = Some(hot_key.clone());
    cli.cold = "s3:cold-bucket".to_owned();
    cli.cold_endpoint = Some("http://127.0.0.1:9001".to_owned());
    cli.cold_region = Some("eu-west-1".to_owned());
    cli.cold_credentials_file = Some(cold_key.clone());
    build(&cli).expect("built");
    cli.hot_credentials_file = Some(cold_key);
    cli.cold_credentials_file = Some(hot_key);
    build(&cli).expect("built the other way round");
}

/// Asserts that an S3 tier that its flags do not describe is refused with a message that names
/// the flag: no endpoint, no credentials file, a file that is missing, that others can read or
/// that lacks a line, an endpoint that is not valid (TLS is not built), and a bucket name that
/// is not one; and that the flags of an S3 tier are refused for a tier of another kind.
#[test]
fn an_s3_tier_that_its_flags_do_not_describe_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-s3-refused");
    let good = key_file(&dir, "good", "ID\nsecret\n", 0o600);
    let open = key_file(&dir, "open", "ID\nsecret\n", 0o644);
    let incomplete = key_file(&dir, "incomplete", "ID\n", 0o600);
    let typo = r#"{"access_key_id": "ID", "secret_acess_key": "secret"}"#;
    let misspelled = key_file(&dir, "misspelled", typo, 0o600);
    let missing = dir.path().join("missing");
    let http = Some("http://127.0.0.1:9000");
    let timeout = Duration::from_secs(5);
    let refused = |spec: &str, endpoint: Option<&str>, credentials: Option<&Path>| {
        let flags = S3Flags {
            endpoint,
            region: None,
            credentials,
        };
        let opened = open_tier("--cold", spec, &flags, timeout);
        opened.err().expect("refused")
    };
    let no_endpoint = refused("s3:bucket", None, Some(&good));
    assert!(no_endpoint.contains("--cold-endpoint"), "{no_endpoint}");
    assert!(no_endpoint.contains("s3:bucket"), "{no_endpoint}");
    let no_key = refused("s3:bucket", http, None);
    assert!(no_key.contains("--cold-credentials-file"), "{no_key}");
    let gone = refused("s3:bucket", http, Some(&missing));
    assert!(gone.contains("--cold-credentials-file"), "{gone}");
    assert!(gone.contains("missing"), "{gone}");
    let others = refused("s3:bucket", http, Some(&open));
    assert!(others.contains("others can read it"), "{others}");
    let short = refused("s3:bucket", http, Some(&incomplete));
    assert!(short.contains("on the second"), "{short}");
    let typed = refused("s3:bucket", http, Some(&misspelled));
    assert!(typed.contains("--cold-credentials-file"), "{typed}");
    assert!(typed.contains("access_key_id"), "{typed}");
    let tls = refused("s3:bucket", Some("https://127.0.0.1:9000"), Some(&good));
    assert!(tls.contains("TLS"), "{tls}");
    assert!(tls.contains("--cold"), "{tls}");
    let name = refused("s3:Not_A_Bucket", http, Some(&good));
    assert!(name.contains("bucket name"), "{name}");

    let given = [
        (http, None, None),
        (None, Some("eu-west-1"), None),
        (None, None, Some(good.as_path())),
    ];
    for (endpoint, region, credentials) in given {
        let flags = S3Flags {
            endpoint,
            region,
            credentials,
        };
        let opened = open_tier("--hot", "memory", &flags, timeout);
        let why = opened.err().unwrap();
        assert!(why.contains("--hot"), "{why}");
        assert!(why.contains("s3:<bucket>"), "{why}");
    }
    let none = S3Flags {
        endpoint: None,
        region: None,
        credentials: None,
    };
    assert!(open_tier("--hot", "memory", &none, timeout).is_ok());
    let unknown = open_tier("--hot", "tape", &none, timeout).err().unwrap();
    assert!(unknown.contains("s3:<bucket>"), "{unknown}");
}

/// Asserts that the key file is read as an id on the first line and a secret on the second,
/// trimmed, with either line ending, or as JSON; that others may not read it; and that a file
/// with a missing or empty line, a missing file and a directory are refused with the flag's
/// name.
#[test]
fn the_key_file_is_an_id_and_a_secret_that_only_its_owner_reads() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-key");
    let want = Ok(("AKID".to_owned(), "secret".to_owned()));
    let spaced = key_file(&dir, "spaced", "  AKID \n secret \n", 0o600);
    assert_eq!(read_key(&spaced), want);
    let json = r#"{"access_key_id": "AKID", "secret_access_key": "secret"}"#;
    let from_json = key_file(&dir, "json", json, 0o600);
    assert_eq!(read_key(&from_json), want);
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

/// Asserts that `--skip-backend-check` takes `multipart-conditional` and a reason, may be given
/// more than once, trims the reason and keeps a `=` inside it; that a value with no `=` or an
/// unknown check is refused with a message that lists the checks; that a blank reason is refused
/// with a message that says a reason is needed; and that every check but
/// `multipart-conditional` is refused as a usage error (exit status 2), whatever its reason.
#[test]
fn only_the_multipart_check_can_be_skipped_and_with_a_reason() {
    let mut args = flags("key", &[]);
    args.extend(
        [
            "--skip-backend-check",
            "multipart-conditional= store ignores it on completion ",
            "--skip-backend-check",
            "multipart-conditional=a=b",
        ]
        .map(str::to_owned),
    );
    let cli = Cli::try_parse_from(args).unwrap();
    let want = [
        SkippedCheck {
            check: Check::MultipartConditional,
            reason: "store ignores it on completion".to_owned(),
        },
        SkippedCheck {
            check: Check::MultipartConditional,
            reason: "a=b".to_owned(),
        },
    ];
    assert_eq!(cli.skip_backend_check, want);

    for text in ["range", "ranges=because", "=because"] {
        let why = parse_skip(text).unwrap_err();
        assert!(why.contains("CHECK=REASON"), "{text}: {why}");
        assert!(why.contains("multipart-conditional"), "{text}: {why}");
    }
    for text in ["multipart-conditional=", "multipart-conditional=   "] {
        let why = parse_skip(text).unwrap_err();
        assert!(why.contains("needs a reason"), "{text}: {why}");
    }
    for check in Check::ALL
        .into_iter()
        .filter(|c| *c != Check::MultipartConditional)
    {
        let value = format!("{check}=the store cannot do it");
        let why = parse_skip(&value).unwrap_err();
        assert!(why.contains("cannot be skipped"), "{value}: {why}");
        assert!(why.contains(check.name()), "{value}: {why}");
        let refused = Cli::try_parse_from(flags("key", &[("--skip-backend-check", &value)]));
        assert_eq!(refused.unwrap_err().exit_code(), 2, "{value}");
    }
}

/// Asserts that the id of a start's probe keys has the proxy id, the time and the process, with
/// every character a key cannot carry in the proxy id replaced by an underscore.
#[test]
fn the_probe_run_id_is_made_of_the_proxy_the_time_and_the_process() {
    assert_eq!(probe_run_id("p1", 1234, 77), "p1-1234-77");
    assert_eq!(probe_run_id("a/b c.d-e", 5, 6), "a_b_c_d-e-5-6");
    assert_ne!(probe_run_id("p1", 1, 7), probe_run_id("p1", 2, 7));
}

/// Asserts that a tier that answered the self-test wrongly is status 3, a tier that could not be
/// reached status 1 (a restart may mend it), flags that cannot go together status 2 and any
/// other failure status 1.
#[test]
fn a_wrong_answer_has_its_own_exit_status_and_an_unreachable_tier_has_not() {
    let failure = SelfTestFailure {
        backend: "hot tier memory".to_owned(),
        check: Check::Range,
        observed: "x".to_owned(),
    };
    let wrong: super::Error = Box::new(SelfTestError::Violation(failure));
    let down = SelfTestError::Unreachable {
        backend: "hot tier memory".to_owned(),
        error: "connection refused".to_owned(),
    };
    let down: super::Error = Box::new(down);
    let invalid: super::Error = Box::new(InvalidFlags("x".to_owned()));
    let other: super::Error = "another reason".into();
    assert_eq!(exit_status(&wrong), 3);
    assert_eq!(exit_status(&down), 1);
    assert_eq!(exit_status(&invalid), 2);
    assert_eq!(exit_status(&other), 1);
}

/// Asserts that the two tiers are tested before a start: tiers that are correct pass; a hot tier
/// that fails is named as the hot tier with its flag value and the check; a cold tier that fails
/// is named as the cold tier; the failure is the error a start gives, so that its exit status is
/// 3; and a check the flags skip is not run, so a tier that fails only that one passes.
#[tokio::test]
async fn the_tiers_are_tested_before_a_start_and_a_failure_names_the_tier() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-probe-tiers");
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.cold = "dir:/cold".to_owned();
    let good = Flawed::new(Flaw::None);
    let now = Duration::ZERO;
    probe_tiers(&cli, &good, &good, now).await.unwrap();

    let broken = Flawed::new(Flaw::OverwritesOnCreate);
    let why = probe_tiers(&cli, &broken, &good, now).await.unwrap_err();
    assert_eq!(exit_status(&why), 3);
    let text = why.to_string();
    assert!(text.contains("hot tier s3:hot-bucket"), "{text}");
    assert!(text.contains("create-only"), "{text}");

    let why = probe_tiers(&cli, &good, &broken, now).await.unwrap_err();
    let text = why.to_string();
    assert!(text.contains("cold tier dir:/cold"), "{text}");
    assert!(text.contains("create-only"), "{text}");

    let no_multipart = Flawed::new(Flaw::NoMultipart);
    probe_tiers(&cli, &no_multipart, &good, now).await.unwrap();
    let why = probe_tiers(&cli, &good, &no_multipart, now)
        .await
        .unwrap_err();
    assert!(why.to_string().contains("multipart-conditional"), "{why}");

    cli.skip_backend_check = vec![SkippedCheck {
        check: Check::MultipartConditional,
        reason: "test".to_owned(),
    }];
    probe_tiers(&cli, &good, &no_multipart, now).await.unwrap();
    assert!(good.inner.list("_tiering/").await.unwrap().is_empty());
}

/// Asserts that a tier that cannot be reached, hot or cold, is not a wrong answer: the start
/// fails with `tier unreachable: <tier>: <error>` and exit status 1, which the unit restarts; and
/// that a full hot tier, a state it is designed to be in, and a full cold tier, start.
#[tokio::test]
async fn an_unreachable_tier_is_status_1_and_a_full_tier_starts() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-probe-down");
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.cold = "s3:cold-bucket".to_owned();
    let good = Flawed::new(Flaw::None);
    let now = Duration::ZERO;
    let down = Flawed::new(Flaw::Down);
    for (hot, cold, tier) in [
        (&down, &good, "hot tier s3:hot-bucket"),
        (&good, &down, "cold tier s3:cold-bucket"),
    ] {
        let why = probe_tiers(&cli, hot, cold, now).await.unwrap_err();
        assert_eq!(exit_status(&why), 1, "{why}");
        let text = why.to_string();
        assert!(
            text.starts_with(&format!("tier unreachable: {tier}: ")),
            "{text}"
        );
        assert!(text.contains("connection refused"), "{text}");
    }

    let full = Flawed::new(Flaw::Full);
    probe_tiers(&cli, &full, &good, now).await.unwrap();
    probe_tiers(&cli, &good, &full, now).await.unwrap();
    let wrong = Flawed::new(Flaw::IgnoresIfMatch);
    let why = probe_tiers(&cli, &full, &wrong, now).await.unwrap_err();
    assert_eq!(exit_status(&why), 3, "{why}");
    // A 501 from the store (the S3 backend's other error) is a contract gap: status 3.
    let not_implemented = Flawed::new(Flaw::RejectsConditionalHeader);
    let why = probe_tiers(&cli, &not_implemented, &good, now)
        .await
        .unwrap_err();
    assert_eq!(exit_status(&why), 3, "{why}");
}

/// Asserts that the report of a drain names the requests that the deadline cut, and that a drain
/// that cut none reports nothing.
#[test]
fn a_drain_that_cut_requests_reports_them_and_one_that_cut_none_says_nothing() {
    assert_eq!(cut_note(Drained { abandoned: 0 }), "");
    assert_eq!(
        cut_note(Drained { abandoned: 3 }),
        "tierstore: 3 requests were cut at the drain deadline\n"
    );
}
