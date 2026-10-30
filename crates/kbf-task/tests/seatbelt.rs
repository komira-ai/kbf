//! On macOS the Command runs under a `sandbox-exec` profile that `kbf-task` builds: no network for a
//! Command denied it, and no read of the files that hold a credential. Each case runs the
//! `socket_probe` helper as a real Command, without the test-only `unconfined-for-tests` feature,
//! and reads its exit code (0, or the errno).

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_proto::reapi::Command;
use kbf_task::network::{Layers, Network};
use kbf_task::run::{Ran, Run, run};
use kbf_testkit::dir::TestDir;

const EPERM: i32 = 1;

fn probe() -> &'static str {
    env!("CARGO_BIN_EXE_socket_probe")
}

/// Runs the probe with `args` under `network`, with `unreadable` denied; how the run ended.
async fn probe_run(name: &str, network: Network, unreadable: &[PathBuf], args: &[&str]) -> Ran {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-seatbelt-{name}"));
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("root");
    let command = Command {
        arguments: std::iter::once(probe())
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        ..Command::default()
    };
    let ran = run(
        &command,
        &Run {
            root: &root,
            private: &dir.join("private"),
            stdout: &dir.join("stdout"),
            stderr: &dir.join("stderr"),
            deadline: Duration::from_secs(60),
            network,
            unreadable,
        },
    )
    .await
    .expect("the probe runs");
    assert!(!ran.timed_out, "{ran:?}");
    ran
}

/// Catches a denied Command that can connect out: `connect` to an address outside the node (no
/// reply is needed) is refused by the sandbox with `EPERM`, and the run says which layer did it.
#[tokio::test]
async fn a_denied_command_cannot_make_a_tcp_connection() {
    let ran = probe_run("connect", Network::Denied, &[], &["tcp-outside"]).await;
    assert_eq!(ran.exit_code, EPERM, "{ran:?}");
    assert_eq!(ran.layers, Layers::Seatbelt);
}

/// Catches a profile that denies more than the network: a denied Command still uses loopback and
/// Unix sockets, which builds and their tools need.
#[tokio::test]
async fn a_denied_command_still_reaches_loopback_and_unix_sockets() {
    for mode in ["tcp-loopback", "unix"] {
        let ran = probe_run("loopback", Network::Denied, &[], &[mode]).await;
        assert_eq!(ran.exit_code, 0, "{mode}: {ran:?}");
    }
}

/// Catches a credential file the Command can read. The same read succeeds without the deny, so the
/// profile, not the file, is what refuses it. A name with a space, a parenthesis and a closing
/// parenthesis is denied too; the read of it by this test process, which is not sandboxed, works.
#[tokio::test]
async fn a_denied_file_cannot_be_read_by_the_command() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-files");
    let grant = dir.join("grant-token");
    let urls = dir.join("put urls (1)");
    for file in [&grant, &urls] {
        std::fs::write(file, "secret").expect("file");
    }
    let path = |p: &Path| p.to_str().expect("utf-8").to_owned();
    for file in [&grant, &urls] {
        let open = probe_run("read-open", Network::Denied, &[], &["read", &path(file)]).await;
        assert_eq!(open.exit_code, 0, "{file:?}: {open:?}");
        let denied = [grant.clone(), urls.clone()];
        let ran = probe_run(
            "read-denied",
            Network::Denied,
            &denied,
            &["read", &path(file)],
        )
        .await;
        assert_eq!(ran.exit_code, EPERM, "{file:?}: {ran:?}");
    }
    assert_eq!(
        std::fs::read(&grant).expect("this process reads it"),
        b"secret"
    );
}

/// Catches a file named by a link being left readable: the sandbox compares real paths, so the
/// deny must follow the link, and a Command that reads through the link is refused.
#[tokio::test]
async fn a_denied_file_is_refused_through_a_link_to_it() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-link");
    let grant = dir.join("grant-token");
    std::fs::write(&grant, "secret").expect("file");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&grant, &link).expect("link");
    let read = link.to_str().expect("utf-8");
    let ran = probe_run("link", Network::Denied, &[link.clone()], &["read", read]).await;
    assert_eq!(ran.exit_code, EPERM, "{ran:?}");
}

/// Catches a Command that is allowed the network not running, or being given the denial: it runs
/// (a file it makes shows that), the run reports no layer, a connect is not refused by the sandbox,
/// and a credential file stays unreadable to it all the same.
#[tokio::test]
async fn an_allowed_command_runs_and_is_not_denied_the_network() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-allowed");
    let made = dir.join("made");
    let ran = probe_run(
        "allowed",
        Network::Allowed,
        &[],
        &["touch", made.to_str().expect("utf-8")],
    )
    .await;
    assert_eq!(ran.exit_code, 0, "{ran:?}");
    assert_eq!(ran.layers, Layers::Open);
    assert!(made.exists());
    let loopback = probe_run("allowed-lo", Network::Allowed, &[], &["tcp-loopback"]).await;
    assert_eq!(loopback.exit_code, 0, "{loopback:?}");

    let grant = dir.join("grant-token");
    std::fs::write(&grant, "secret").expect("file");
    let read = grant.to_str().expect("utf-8");
    let ran = probe_run(
        "allowed-read",
        Network::Allowed,
        std::slice::from_ref(&grant),
        &["read", read],
    )
    .await;
    assert_eq!(ran.exit_code, EPERM, "{ran:?}");
}
