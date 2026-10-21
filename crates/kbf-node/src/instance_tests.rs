//! The daemon's claim on its scratch directory (issue #22): `<--data-dir>/leases/daemon-instance`
//! names the daemon process that runs there, so the next one can tell the server which
//! process's runs its drivers killed before its `Hello`. It is written only once every
//! runtime has been built (each runs its start-up sweep in its constructor), and never
//! by a daemon that did not start.

use std::path::Path;

use clap::Parser as _;

use super::tests::{scratch, tls_files};
use super::{Cli, native_config, native_with, node};

/// The flags of a daemon in `dir`, its leases under `dir/leases`, with `extra`.
fn cli(dir: &Path, extra: &[String]) -> Cli {
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let flags = [
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=node-1".to_owned(),
        flag("data-dir", dir),
    ];
    Cli::try_parse_from(flags.iter().chain(extra)).expect("flags")
}

/// The claim in `dir`'s scratch directory, as written.
fn claimed(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("leases/daemon-instance")).ok()
}

/// A claim of `id`, as a daemon writes it.
fn claim_of(id: &str) -> Option<String> {
    Some(format!("{id}\n"))
}

/// Catches a daemon that leaves no claim (its successor could name no predecessor,
/// and the server would wait the handover grace for every lease it held), and one that
/// keeps its predecessor's: each daemon that starts writes its own instance id.
#[tokio::test]
async fn each_daemon_that_starts_claims_the_scratch_directory() {
    let dir = scratch("instance-claim");
    tls_files(&dir);
    let cli = cli(&dir, &["--driver=fake".to_owned()]);
    let xcrun = dir.join("no-xcrun");
    let (first, _) = node(&cli, || native_config(&cli), &xcrun).expect("started");
    assert_eq!(claimed(&dir), claim_of(first.instance_id()));
    let (second, _) = node(&cli, || native_config(&cli), &xcrun).expect("started");
    assert_ne!(second.instance_id(), first.instance_id());
    assert_eq!(claimed(&dir), claim_of(second.instance_id()));
}

/// Catches the claim written before every runtime is built (the mutant "claim first"):
/// a daemon that fails to start after its first driver's sweep would leave its own id,
/// and the next daemon would name a process that never ran a lease as the one whose
/// runs it killed, while the runs of the one before are taken as alive. The failing
/// driver here is the whole-machine one, after the native one: off macOS it is refused,
/// on macOS it finds no helper.
#[tokio::test]
async fn a_daemon_that_does_not_start_keeps_the_claim_it_found() {
    let dir = scratch("instance-kept");
    tls_files(&dir);
    std::fs::create_dir_all(dir.join("leases")).expect("scratch");
    std::fs::write(dir.join("leases/daemon-instance"), "the-one-before\n").expect("claim");
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let cli = cli(
        &dir,
        &[
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("xcode-apps", &dir.join("Applications")),
            flag("mac-session-socket", &dir.join("no-helper")),
            "--driver=native".to_owned(),
            "--driver=native-whole-machine".to_owned(),
        ],
    );
    let config = native_config(&cli).expect("the native configuration");
    let error = native_with(&cli, config, &dir.join("no-xcrun"))
        .err()
        .expect("started");
    assert!(error.to_string().contains("whole-machine"), "{error}");
    assert_eq!(claimed(&dir).as_deref(), Some("the-one-before\n"));
    // The native runtime's Xcode survey may still be writing under it: not removed
    // here (the next run of this test binary's pid would).
    drop(dir);
}

/// Catches a claim that cannot be read stopping the start (a stray directory would take
/// the node out of the farm): the daemon starts, naming no predecessor, so the server
/// falls back to the handover grace.
#[tokio::test]
async fn a_daemon_whose_claim_cannot_be_read_starts_naming_none() {
    let dir = scratch("instance-unreadable");
    tls_files(&dir);
    std::fs::create_dir_all(dir.join("leases/daemon-instance")).expect("in the way");
    let cli = cli(&dir, &["--driver=fake".to_owned()]);
    let started = node(&cli, || native_config(&cli), &dir.join("no-xcrun"));
    assert!(started.is_ok(), "the start failed on its claim");
    assert!(dir.join("leases/daemon-instance").is_dir());
}
