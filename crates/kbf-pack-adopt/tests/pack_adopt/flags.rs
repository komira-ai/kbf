//! The flags: their defaults, what is passed on to the run, and everything the tool refuses
//! before it reads or writes anything.

use std::io;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use kbf_pack::{AdoptError, AdoptMode};
use kbf_pack_adopt::{Args, DRY_RUN_SAMPLE, Error, Mode, Plan};
use kbf_testkit::dir::TestDir;

use crate::support::{SOCKET_URL, says, write_file, write_secret};

/// A store address that is parsed and never connected to.
const ENDPOINT: &str = "http://127.0.0.1:1";

/// The flags with the source at `endpoint` in `bucket`, the index at `index` and the
/// credentials file at `keys`, and `more`.
fn parse_with(endpoint: &str, bucket: &str, index: &str, keys: &Path, more: &[&str]) -> Args {
    let keys = keys.to_str().expect("a UTF-8 path");
    let mut argv = vec![
        "kbf-pack-adopt",
        "--source-store-endpoint",
        endpoint,
        "--source-store-bucket",
        bucket,
        "--pack-index",
        index,
        "--source-store-credentials-file",
        keys,
    ];
    argv.extend_from_slice(more);
    Args::try_parse_from(argv).expect("the flags parse")
}

/// The flags with the bucket `kbf-fake`.
fn parse(endpoint: &str, index: &str, keys: &Path, more: &[&str]) -> Args {
    parse_with(endpoint, "kbf-fake", index, keys, more)
}

/// The checked flags, with the usual source and index.
fn plan(keys: &Path, more: &[&str]) -> Result<Plan, Error> {
    parse(ENDPOINT, SOCKET_URL, keys, more).plan()
}

/// The words of a refusal that is one line and ends the run with status 2.
fn refusal(checked: Result<Plan, Error>) -> String {
    let Err(error) = checked else {
        panic!("the flags were accepted");
    };
    assert_eq!(error.code(), 2);
    let said = error.to_string();
    assert!(!said.contains('\n'), "{said}");
    said
}

/// A directory with a credentials file of mode 0600 in it.
fn with_keys(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let keys = dir.path().join("keys.json");
    write_secret(&keys, 0o600);
    (dir, keys)
}

/// Asserts that the flags left out are a dry run over `kbf/` in `us-east-1`, with a sample of
/// twenty blobs, four footers read at once, no limit, no pace and no files, and that the store
/// is named by its endpoint and its bucket.
#[test]
fn the_defaults_are_a_dry_run_over_kbf_with_a_sample_of_twenty() {
    let (_dir, keys) = with_keys("flags-defaults");
    let parsed = parse(ENDPOINT, SOCKET_URL, &keys, &[]);
    assert_eq!(parsed.mode, Mode::DryRun);
    assert_eq!(parsed.source_prefix, "kbf/");
    assert_eq!(parsed.source_store_region, "us-east-1");
    assert_eq!(parsed.footer_concurrency.get(), 4);
    assert_eq!(parsed.pack_store_bucket, None);

    let planned = parsed.plan().unwrap();

    assert_eq!(DRY_RUN_SAMPLE, 20);
    assert_eq!(planned.adopt.mode, AdoptMode::DryRun);
    assert_eq!(planned.adopt.verify_sample, 20);
    assert_eq!(planned.adopt.prefix.as_str(), "kbf/");
    assert_eq!(planned.adopt.source, "http://127.0.0.1:1/kbf-fake");
    assert_eq!(planned.adopt.reads.concurrency.get(), 4);
    assert_eq!(planned.adopt.limit_segments, None);
    assert_eq!(planned.adopt.segments_per_second, None);
    assert!(!planned.adopt.fail_on_conflict);
    assert_eq!(planned.checkpoint_file, None);
    assert_eq!(planned.report_json, None);
}

/// Asserts that an apply reads no sample unless asked, that a sample asked for is used in
/// either mode, and that an empty prefix and a deep one are taken.
#[test]
fn an_apply_has_no_sample_unless_one_is_asked_for() {
    let (_dir, keys) = with_keys("flags-sample");
    let apply = ["--mode", "apply", "--pack-store-bucket", "kbf-fake"];
    let mut asked = apply.to_vec();
    asked.extend(["--verify-sample", "5"]);

    let plain = plan(&keys, &apply).unwrap();
    let sampled = plan(&keys, &asked).unwrap();
    let off = plan(&keys, &["--verify-sample", "0"]).unwrap();
    let empty = plan(&keys, &["--source-prefix", ""]).unwrap();
    let deep = plan(&keys, &["--source-prefix", "a/b/"]).unwrap();

    assert_eq!(plain.adopt.mode, AdoptMode::Apply);
    assert_eq!(plain.adopt.verify_sample, 0);
    assert_eq!(sampled.adopt.verify_sample, 5);
    assert_eq!(off.adopt.verify_sample, 0);
    assert_eq!(empty.adopt.prefix.as_str(), "");
    assert_eq!(deep.adopt.prefix.as_str(), "a/b/");
}

/// Asserts that the limit, the pace, the footer reads in flight, the conflict flag and the two
/// files are passed on to the run.
#[test]
fn the_limits_and_the_files_are_passed_on() {
    let (_dir, keys) = with_keys("flags-passed");
    let more = [
        "--segments-per-second",
        "3",
        "--limit-segments",
        "7",
        "--footer-concurrency",
        "9",
        "--fail-on-conflict",
        "--checkpoint-file",
        "checkpoint.json",
        "--report-json",
        "report.json",
    ];

    let planned = plan(&keys, &more).unwrap();

    let pace = planned.adopt.segments_per_second;
    assert_eq!(pace.map(|rate| rate.get()), Some(3));
    assert_eq!(planned.adopt.limit_segments, Some(7));
    assert_eq!(planned.adopt.reads.concurrency.get(), 9);
    assert!(planned.adopt.fail_on_conflict);
    let checkpoint = planned.checkpoint_file.as_deref();
    assert_eq!(checkpoint, Some(Path::new("checkpoint.json")));
    let report = planned.report_json.as_deref();
    assert_eq!(report, Some(Path::new("report.json")));
}

/// Asserts that 64 footer reads at once are taken and 65 are refused, with a line that names
/// the flag, so that the flag cannot turn a page of the listing into a flood of requests.
#[test]
fn the_footer_reads_in_flight_are_capped() {
    let (_dir, keys) = with_keys("flags-footers");

    let most = plan(&keys, &["--footer-concurrency", "64"]).unwrap();
    let too_many = refusal(plan(&keys, &["--footer-concurrency", "65"]));

    assert_eq!(most.adopt.reads.concurrency.get(), 64);
    says(&too_many, "--footer-concurrency");
    says(&too_many, "at most 64");
}

/// Asserts that a `file://` source is refused first, before the credentials file is looked at,
/// since a local store evicts what a pack would point at.
#[test]
fn a_file_source_is_refused_before_anything_else() {
    let absent = Path::new("absent-keys.json");
    let args = parse("file:///var/lib/members", SOCKET_URL, absent, &[]);

    let said = refusal(args.plan());

    says(&said, "file:// source is refused");
}

/// Asserts that an apply is refused without the pack bucket and with a pack bucket that is not
/// the source bucket, each with a line that names the flag, and that a dry run needs neither.
#[test]
fn an_apply_is_refused_unless_the_pack_bucket_is_the_source_bucket() {
    let (_dir, keys) = with_keys("flags-bucket");

    let missing = refusal(plan(&keys, &["--mode", "apply"]));
    let other = ["--mode", "apply", "--pack-store-bucket", "kbf-packs"];
    let differs = refusal(plan(&keys, &other));

    says(&missing, "needs --pack-store-bucket");
    says(&differs, "kbf-packs is not --source-store-bucket");
    assert!(plan(&keys, &["--pack-store-bucket", "kbf-packs"]).is_ok());
}

/// Asserts that a prefix that does not end in a slash, and one that is no key prefix, are
/// refused.
#[test]
fn a_prefix_must_be_empty_or_end_in_a_slash() {
    let (_dir, keys) = with_keys("flags-prefix");

    let bare = refusal(plan(&keys, &["--source-prefix", "kbf"]));
    let root = refusal(plan(&keys, &["--source-prefix", "/"]));

    says(&bare, "end in /");
    says(&root, "--source-prefix:");
}

/// Asserts that an index URL that is not a Postgres URL, names no host or carries a password is
/// refused, and that the refusal of the password does not repeat it.
#[test]
fn the_index_url_must_be_a_postgres_url_with_no_password() {
    let (_dir, keys) = with_keys("flags-index");
    let check = |url: &str| refusal(parse(ENDPOINT, url, &keys, &[]).plan());

    let not_postgres = check("mysql://host/db");
    let no_host = check("postgres:///kbf");
    let password = check("postgres://kbf:hunter2@host/kbf");

    says(&not_postgres, "not a postgres:// URL");
    says(&no_host, "not a postgres:// URL");
    says(&password, "no password");
    assert!(!password.contains("hunter2"), "{password}");
    let named = parse(ENDPOINT, "postgres://kbf@host/kbf", &keys, &[]);
    assert!(named.plan().is_ok());
}

/// Asserts that a credentials file that is missing, readable by others, not JSON or not the key
/// pair is refused with a line that names the file and not the secret, and that a file only its
/// owner can read, even read-only, is taken.
#[test]
fn the_credentials_file_must_be_the_owners_json_key_pair() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "flags-credentials");
    let file = |name: &str, text: &str, mode: u32| {
        let path = dir.path().join(name);
        write_file(&path, text, mode);
        path
    };
    let secret = r#"{"access_key_id": "k", "secret_access_key": "SECRETVALUE"}"#;
    let open = file("open.json", secret, 0o644);
    let group = file("group.json", secret, 0o640);
    let prose = file("prose.json", "not json", 0o600);
    let partial = file("partial.json", r#"{"access_key_id": "k"}"#, 0o600);
    let read_only = file("read-only.json", secret, 0o400);

    let said_open = refusal(plan(&open, &[]));
    let said_group = refusal(plan(&group, &[]));
    let said_prose = refusal(plan(&prose, &[]));
    let said_partial = refusal(plan(&partial, &[]));
    let said_absent = refusal(plan(&dir.path().join("absent.json"), &[]));

    says(&said_open, "mode 0644 lets others read a secret");
    says(&said_open, "open.json");
    assert!(!said_open.contains("SECRETVALUE"), "{said_open}");
    says(&said_group, "mode 0640 lets others read a secret");
    says(&said_prose, "not JSON with access_key_id");
    says(&said_partial, "not JSON with access_key_id");
    says(&said_absent, "absent.json");
    assert!(plan(&read_only, &[]).is_ok());
}

/// Asserts that an endpoint the store client does not take (TLS is not built yet) and a bucket
/// name that is not one are refused, each with the store client's own words.
#[test]
fn an_endpoint_or_a_bucket_the_store_cannot_use_is_refused() {
    let (_dir, keys) = with_keys("flags-store");

    let tls = refusal(parse("https://host", SOCKET_URL, &keys, &[]).plan());
    let small = refusal(parse_with(ENDPOINT, "X", SOCKET_URL, &keys, &[]).plan());

    says(&tls, "TLS");
    says(&small, "bucket name");
}

/// Asserts that a failed call is status 1 and a checkpoint of another run is status 2, and that
/// a failure that is not a checkpoint's is status 1.
#[test]
fn a_refusal_is_status_2_and_a_failure_is_status_1() {
    let io_error = Error::from(io::Error::other("disk full"));
    let mismatch = Error::from(AdoptError::ResumeMismatch);
    let conflict = AdoptError::Conflict {
        key: "kbf/k".to_owned(),
    };

    assert_eq!(io_error.code(), 1);
    assert_eq!(mismatch.code(), 2);
    assert_eq!(Error::from(conflict).code(), 1);
    assert_eq!(io_error.to_string(), "failed: disk full");
    assert!(mismatch.to_string().starts_with("refused: "));
}
