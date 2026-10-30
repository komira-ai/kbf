//! What each error of the tool says: the words an operator reads when a verb stops.

use std::path::PathBuf;
use std::sync::Arc;

use tierstore_bench::config::{ConfigError, Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::requestlog::RequestLog;
use tierstore_bench::role::Role;
use tierstore_bench::s3::{BenchS3, Timeouts};

/// Catches an error whose message lost its lead words, or a variant whose conversion from the
/// error it wraps is missing: every variant is made, from its source where it has one, and its
/// text starts as the operator's notes expect.
#[test]
fn every_error_says_what_stopped() {
    let store = BenchS3::new(
        Role::Cold,
        &StoreSpec {
            endpoint: "https://not-plain-http.example:9000".to_owned(),
            bucket: "bench-test".to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: "k".to_owned(),
            secret_access_key: Secret::new("s"),
        },
        Timeouts::default(),
        Arc::new(RequestLog::to_writer(std::io::sink())),
    );
    let store = match store {
        Err(e) => e,
        Ok(_) => panic!("an https endpoint is not accepted"),
    };
    let key = BenchError::from(kbf_objstore::ObjectKey::new("a b").unwrap_err());
    let cases: [(BenchError, &str); 9] = [
        (BenchError::NotYet("report"), "report: not yet implemented"),
        (ConfigError::CapOrder.into(), "byte caps must satisfy"),
        (store, "store configuration"),
        (key, "object key"),
        (BenchError::Request("bad range"), "bad range"),
        (
            BenchError::Io {
                path: PathBuf::from("out/x"),
                source: std::io::Error::other("denied"),
            },
            "out/x: denied",
        ),
        (
            BenchError::Log(std::io::Error::other("disk full")),
            "request log: disk full",
        ),
        (
            BenchError::Halted("stop-file".to_owned()),
            "halted: stop-file",
        ),
        (BenchError::Result("odd".to_owned()), "result file: odd"),
    ];
    for (error, want) in &cases {
        assert!(error.to_string().starts_with(want), "{error} / {want}");
    }
}

/// Catches a request log that cannot be opened being made out of nothing: a path in a directory
/// that is not there is an io error naming it.
#[test]
fn a_request_log_in_a_directory_that_is_not_there_is_an_io_error() {
    let path = std::path::Path::new("/definitely/not/here/requests.jsonl");
    let err = RequestLog::create(path).err().expect("an error");
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("requests.jsonl"), "{err}");
}

/// Catches a digest of a file that is not there being printed as if it were one: the line is an
/// io error naming the file, and the digest of a file is `tierstore-bench sha256 <hex>`.
#[test]
fn the_digest_of_a_missing_file_is_an_error_and_of_a_file_is_its_sha256() {
    let missing = std::path::Path::new("/definitely/not/here");
    let err = tierstore_bench::cli::digest_line(missing).unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("/definitely/not/here"), "{err}");

    let dir =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-digest");
    let file = dir.path().join("abc");
    std::fs::write(&file, "abc").unwrap();
    assert_eq!(
        tierstore_bench::cli::digest_line(&file).unwrap(),
        "tierstore-bench sha256 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
