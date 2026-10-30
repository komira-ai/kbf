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

/// The control of the test above: the same connect, run as a Command allowed the network, is not
/// refused by the sandbox (the answer is a timeout, an unreachable network or a connection, never
/// `EPERM`), so `EPERM` there is the denial and not the address.
#[tokio::test]
async fn an_allowed_command_is_not_refused_the_same_connect() {
    let ran = probe_run("connect-allowed", Network::Allowed, &[], &["tcp-outside"]).await;
    assert_ne!(ran.exit_code, EPERM, "{ran:?}");
    assert_eq!(ran.layers, Layers::Open);
}

/// Catches a profile that denies more than the network: a denied Command still uses loopback and
/// Unix sockets, which builds and their tools need.
#[tokio::test]
async fn a_denied_command_still_reaches_loopback_and_unix_sockets() {
    // `::1` is loopback too; a runner without IPv6 answers `EADDRNOTAVAIL` or `EAFNOSUPPORT`.
    let v6 = probe_run("loopback6", Network::Denied, &[], &["tcp-loopback6"]).await;
    assert!(
        [0, libc::EADDRNOTAVAIL, libc::EAFNOSUPPORT].contains(&v6.exit_code),
        "{v6:?}"
    );
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
    let direct = probe_run("link-open", Network::Denied, &[], &["read", read]).await;
    assert_eq!(direct.exit_code, 0, "{direct:?}");
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

/// Runs `mode <file> <dir>/moved` with `file` denied (or not, for the control); the exit code.
async fn move_then_read(name: &str, mode: &str, file: &Path, denied: bool) -> i32 {
    let moved = file.with_file_name(format!("{name}-moved"));
    let unreadable = if denied {
        vec![file.to_owned()]
    } else {
        Vec::new()
    };
    let args = [
        mode,
        file.to_str().expect("utf-8"),
        moved.to_str().expect("utf-8"),
    ];
    probe_run(name, Network::Denied, &unreadable, &args)
        .await
        .exit_code
}

/// Catches a credential read through a hard link the Command makes to it: the sandbox names a
/// file by path, so a second name for the same file must not be readable. The same link and read of
/// a file that is not denied works.
#[tokio::test]
async fn a_denied_file_is_refused_through_a_hard_link_made_by_the_command() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-hardlink");
    let (secret, other) = (dir.join("secret"), dir.join("other"));
    std::fs::write(&secret, "secret").expect("file");
    std::fs::write(&other, "other").expect("file");
    assert_eq!(
        move_then_read("hl-control", "link-read", &other, false).await,
        0
    );
    assert_ne!(
        move_then_read("hl-denied", "link-read", &secret, true).await,
        0
    );
}

/// Catches a credential read after the Command renames it: the file, under its new name, must stay
/// unreadable. The same rename and read of a file that is not denied works.
#[tokio::test]
async fn a_denied_file_cannot_be_renamed_and_read_by_the_command() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-rename");
    let (secret, other) = (dir.join("secret"), dir.join("other"));
    std::fs::write(&secret, "secret").expect("file");
    std::fs::write(&other, "other").expect("file");
    assert_eq!(
        move_then_read("rn-control", "rename-read", &other, false).await,
        0
    );
    assert_ne!(
        move_then_read("rn-denied", "rename-read", &secret, true).await,
        0
    );
}

/// Catches a credential read after the Command renames the directory that holds it: the file, now
/// under another path, must stay unreadable. The same rename and read with nothing denied works.
#[tokio::test]
async fn a_denied_file_cannot_be_read_after_its_directory_is_renamed() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-dirmove");
    for (name, denied) in [("dm-control", false), ("dm-denied", true)] {
        let held = dir.join(format!("{name}-dir"));
        std::fs::create_dir(&held).expect("dir");
        let secret = held.join("secret");
        std::fs::write(&secret, "secret").expect("file");
        let moved = dir.join(format!("{name}-moved"));
        let unreadable = if denied {
            vec![secret.clone()]
        } else {
            Vec::new()
        };
        let args = [
            "rename-dir-read",
            held.to_str().expect("utf-8"),
            moved.to_str().expect("utf-8"),
            "secret",
        ];
        let ran = probe_run(name, Network::Denied, &unreadable, &args).await;
        if denied {
            assert_ne!(ran.exit_code, 0, "{ran:?}");
        } else {
            assert_eq!(ran.exit_code, 0, "{ran:?}");
        }
    }
}

/// Catches a credential read under the same name in other letter case: the volume ignores case, and
/// the sandbox must too. The control reads the same file by another case with nothing denied.
#[tokio::test]
async fn a_denied_file_is_refused_under_another_letter_case() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-seatbelt-case");
    let secret = dir.join("secret-file");
    std::fs::write(&secret, "secret").expect("file");
    let shouted = dir.join("SECRET-FILE");
    let read = shouted.to_str().expect("utf-8");
    let open = probe_run("case-open", Network::Denied, &[], &["read", read]).await;
    assert_eq!(open.exit_code, 0, "the volume ignores case: {open:?}");
    let ran = probe_run(
        "case",
        Network::Denied,
        std::slice::from_ref(&secret),
        &["read", read],
    )
    .await;
    assert_eq!(ran.exit_code, EPERM, "{ran:?}");
}
