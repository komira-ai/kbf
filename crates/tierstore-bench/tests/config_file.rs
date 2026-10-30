//! `RunConfig::load`: the run file as a file.

use clap::Parser;
use kbf_testkit::dir::TestDir;
use tierstore_bench::config::{ConfigArgs, ConfigError, RunConfig};
use tierstore_bench::role::Role;

#[derive(Parser)]
struct Wrap {
    #[command(flatten)]
    args: ConfigArgs,
}

fn args(run_file: &std::path::Path) -> ConfigArgs {
    let file = run_file.to_str().unwrap();
    Wrap::try_parse_from(["t", "--result-dir", "out", "--run-file", file])
        .unwrap()
        .args
}

/// Catches a run file that is not read from the path the flag names, and an absent flag that
/// fails instead of giving a run with no stores.
#[test]
fn a_run_file_is_read_from_its_path() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-runfile");
    let path = dir.path().join("run.json");
    std::fs::write(
        &path,
        r#"{"stores": {"instance": {"endpoint": "http://store.example:9000", "bucket": "test-bucket",
            "access_key_id": "id", "secret_access_key": "secret"}}}"#,
    )
    .unwrap();
    let cfg = RunConfig::load(&args(&path)).unwrap();
    let spec = cfg.store(Role::Instance).unwrap();
    assert_eq!(spec.bucket, "test-bucket");
    assert!(cfg.store(Role::Cold).is_err());

    let none = Wrap::try_parse_from(["t", "--result-dir", "out"])
        .unwrap()
        .args;
    assert!(RunConfig::load(&none).unwrap().stores.is_empty());
}

/// Catches a missing or malformed run file being treated as an empty one.
#[test]
fn a_missing_or_malformed_run_file_is_an_error_naming_the_path() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-runfile-bad");
    let missing = dir.path().join("none.json");
    let err = RunConfig::load(&args(&missing)).unwrap_err();
    assert!(matches!(err, ConfigError::RunFileRead { .. }), "{err}");
    assert!(err.to_string().contains("none.json"));

    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{").unwrap();
    let err = RunConfig::load(&args(&bad)).unwrap_err();
    assert!(matches!(err, ConfigError::RunFileParse { .. }), "{err}");
}
