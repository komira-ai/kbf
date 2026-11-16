use std::path::Path;
use std::process::ExitCode;

use kbf_proto::reapi::Digest;
use kbf_segments::sha256;
use kbf_task_pack::ManifestError;

use super::*;
use crate::out_dir::OutDirError;

/// The REAPI digest of `bytes`.
fn reapi_of(bytes: &[u8]) -> Digest {
    digest::to_reapi(&sha256(bytes))
}

/// The flags of a tree job at `root` for an action nobody has, with the put directory `out`.
fn cli(root: &Path, out: &Path) -> Cli {
    Cli::try_parse_from([
        "kbf-task".to_owned(),
        format!("--tree={}", root.display()),
        "--stage-and-commit".to_owned(),
        format!("--out-dir={}", out.display()),
        "--lease=lease-1".to_owned(),
        format!("--action={}", sha256(b"a")),
    ])
    .expect("flags")
}

#[test]
fn each_failure_class_has_its_own_exit_code() {
    let codes = [
        Failure::Setup(String::new()).exit_code(),
        Failure::Fetch(FetchError::Invalid(String::new())).exit_code(),
        Failure::Run(RunError::Invalid(String::new())).exit_code(),
        Failure::Result(String::new()).exit_code(),
        Failure::Read(std::io::Error::other("x")).exit_code(),
        Failure::Pack(PackError::from(ManifestError::Version(2))).exit_code(),
        Failure::OutDir(OutDirError::Write {
            name: "pack".to_owned(),
            source: std::io::Error::other("x"),
        })
        .exit_code(),
    ];
    assert_eq!(codes, [14, 10, 11, 12, 12, 12, 13]);
}

/// A Command refused for want of a confine has an exit code of its own, so the farm can tell
/// "this system cannot confine" from a Command that could not be started.
#[test]
fn a_run_refused_for_want_of_a_confine_has_its_own_exit_code() {
    let failure = Failure::Run(RunError::NoConfine);
    assert_eq!(failure.exit_code(), EXIT_NO_CONFINE);
    assert_eq!(EXIT_NO_CONFINE, 15);
    assert_ne!(EXIT_NO_CONFINE, EXIT_RUN);
    assert!(
        failure
            .to_string()
            .contains("cannot deny the Command the network")
    );
}

#[test]
fn a_result_digest_that_is_absent_or_malformed_is_refused() {
    let good = reapi_of(b"x");
    assert_eq!(digest_of(Some(&good)).expect("good"), sha256(b"x"));
    assert!(matches!(digest_of(None), Err(Failure::Result(_))));
    let bad = Digest {
        hash: "xyz".to_owned(),
        size_bytes: 1,
    };
    assert!(matches!(digest_of(Some(&bad)), Err(Failure::Result(_))));
}

#[test]
fn the_actions_positive_timeout_replaces_the_default() {
    let default = Duration::from_secs(60);
    let with = |seconds, nanos| Action {
        timeout: Some(prost_types::Duration { seconds, nanos }),
        ..Default::default()
    };
    assert_eq!(deadline(&Action::default(), default), default);
    assert_eq!(deadline(&with(5, 0), default), Duration::from_secs(5));
    assert_eq!(deadline(&with(0, 500), default), Duration::from_nanos(500));
    assert_eq!(deadline(&with(0, 0), default), default);
    assert_eq!(deadline(&with(-1, 0), default), default);
    // An Action cannot outlive the operator's limit.
    assert_eq!(deadline(&with(1_000_000_000, 0), default), default);
}

#[test]
fn the_flags_parse_with_their_defaults() {
    let flags = cli(Path::new("/w"), Path::new("/put"));
    assert_eq!(flags.deadline_ms, 3_600_000);
    assert_eq!(flags.root(), Path::new("/w"));
    assert_eq!(flags.out_dir(), Path::new("/put"));
    assert!(flags.object_format.is_none());
    // The action must be a digest.
    assert!(
        Cli::try_parse_from([
            "kbf-task",
            "--tree",
            "--stage-and-commit",
            "--lease=l",
            "--action=nothex/1"
        ])
        .is_err()
    );
}

/// Catches a lease the frontend would refuse (empty, a `/`, a space, non-ASCII, over 128 bytes)
/// starting the Command: it is a setup failure (exit 14) that names `--lease`.
#[test]
fn a_lease_ingest_would_refuse_is_a_setup_failure() {
    let long = "a".repeat(129);
    for lease in ["", "a/b", "a b", "caf\u{e9}", long.as_str()] {
        let mut flags = cli(Path::new("/w"), Path::new("/put"));
        flags.lease = lease.to_owned();
        let Err(failure) = check_lease(&flags) else {
            panic!("accepted the lease {lease:?}");
        };
        assert_eq!(failure.exit_code(), EXIT_SETUP);
        assert!(failure.to_string().contains("--lease"), "{failure}");
    }
}

/// Catches a valid lease being refused: 1 byte and 128 bytes of `0-9A-Za-z_-` pass.
#[test]
fn a_build_id_is_a_lease() {
    let longest = format!("{}aZ", "aZ09_-".repeat(21));
    assert_eq!(longest.len(), 128);
    for lease in ["0", "Lease_1-b", longest.as_str()] {
        let mut flags = cli(Path::new("/w"), Path::new("/put"));
        flags.lease = lease.to_owned();
        check_lease(&flags).expect(lease);
    }
}

/// Catches a size cap losing its detail on the way to stderr: the failure says `output-too-large`
/// and exits 12.
#[test]
fn a_size_cap_is_exit_12_with_output_too_large() {
    let failure = Failure::Pack(PackError::from(ManifestError::TooLarge { len: 1 }));
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(
        failure.to_string().contains("output-too-large"),
        "{failure}"
    );
}

/// Catches an outcome that is not turned into its exit code: success is 0, and a failure is its
/// class's code.
#[test]
fn an_outcome_is_its_exit_code() {
    let failed = Err(Failure::Setup("no".to_owned()));
    assert_eq!(exit_code(failed), ExitCode::from(EXIT_SETUP));
    let manifest = TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"a"),
        exit_code: 0,
        stdout: None,
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing::default(),
        usage: Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: None,
        objects: None,
    };
    assert_eq!(exit_code(Ok(manifest)), ExitCode::SUCCESS);
}

/// Catches: `run_cli` not building a runtime and running the task on it: a bad lease ends the
/// process with the setup code, and the Command never starts.
#[test]
fn run_cli_runs_the_task_and_returns_its_exit_code() {
    let mut flags = cli(
        Path::new("/nonexistent-root"),
        Path::new("/nonexistent-out"),
    );
    flags.lease = "a/b".to_owned();
    assert_eq!(run_cli(&flags), ExitCode::from(EXIT_SETUP));
    assert_eq!(
        run_cli_with(Ok(()), &flags),
        ExitCode::from(EXIT_SETUP),
        "with the relay and the subreaper set"
    );
}

/// Catches: a runtime that cannot start not being a setup failure, and a process that could not
/// make itself non-dumpable running a Command anyway.
#[test]
fn a_runtime_or_a_protection_that_fails_is_a_setup_failure() {
    let flags = cli(Path::new("/w"), Path::new("/put"));
    let failed = Err(std::io::Error::other("no threads"));
    assert_eq!(start(failed, &flags), ExitCode::from(EXIT_SETUP));
    let refused = Err(std::io::Error::other("no prctl"));
    assert_eq!(run_cli_with(refused, &flags), ExitCode::from(EXIT_SETUP));
}
