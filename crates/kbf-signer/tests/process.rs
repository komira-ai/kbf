//! The `kbf-signer` binary: flags, startup, and the hardening of the running process.

mod support;

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kbf_ca::{Algorithm, Signer};
use kbf_signer::{Route, SocketSigner};
use support::{euid, make_ca, scratch};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn signer(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kbf-signer"));
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd
}

// Catches: a signer that does not clear its dumpable flag before it loads the key (the
// key readable by any process of the account through /proc/<pid>/mem), a binary whose
// flags are not wired to the server (wrong socket, uid or directory), and a socket that
// does not serve a client. For a process that is not dumpable the kernel makes
// /proc/<pid> belong to root; the check is skipped when the tests themselves run as root,
// where it could not fail, and the in-process test of `harden` covers the flag then.
#[test]
fn the_binary_serves_and_is_not_dumpable() {
    let ca = make_ca("process");
    let socket = ca.scratch.join("run").join("sock");
    let child = signer(&[
        "--ca-dir",
        support::path_str(&ca.dir),
        "--socket",
        support::path_str(&socket),
        "--frontend-uid",
        &euid().to_string(),
    ])
    .spawn()
    .expect("spawn");
    let pid = child.id();
    let _running = Running(child);

    let deadline = Instant::now() + Duration::from_secs(60);
    let client = loop {
        match SocketSigner::connect(&socket, Route::Node) {
            Ok(client) => break client,
            Err(e) => assert!(Instant::now() < deadline, "the signer never came up: {e}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(client.public_key(), ca.spki.as_slice());
    let signature = client
        .sign(
            &support::node_tbs(&ca, kbf_types::Role::Daemon, None),
            Algorithm::EcdsaP256Sha256,
        )
        .expect("signed by the process");
    assert!(!signature.is_empty());

    // The kernel refuses a same-account reader of a process that is not dumpable; as root
    // the read is allowed anyway, so the check is skipped there (the in-process test of
    // `harden` covers the flag).
    if euid() != 0 {
        let read = std::fs::read(format!("/proc/{pid}/environ"));
        assert!(
            read.is_err(),
            "a not-dumpable process's environ was readable"
        );
    }
}

// Catches: a start that fails silently or with the key in its message: a missing CA
// directory, a lifetime outside the policy and a flag left out each exit non-zero with a
// message naming the program, and nothing that looks like key material.
#[test]
fn a_bad_start_exits_nonzero_and_says_why() {
    let scratch = scratch("bad_start");
    let socket = scratch.join("run").join("sock");
    let mut child = signer(&[
        "--ca-dir",
        support::path_str(&scratch.join("no-ca")),
        "--socket",
        support::path_str(&socket),
        "--frontend-uid",
        "0",
    ])
    .spawn()
    .expect("spawn");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read");
    assert!(!child.wait().expect("wait").success());
    assert!(stderr.starts_with("kbf-signer: "), "{stderr}");
    assert!(!stderr.contains("PRIVATE KEY"));

    let ca = make_ca("bad_lifetime");
    let out = signer(&[
        "--ca-dir",
        support::path_str(&ca.dir),
        "--socket",
        support::path_str(&socket),
        "--frontend-uid",
        "0",
        "--lifetime-secs",
        "5",
    ])
    .output()
    .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("lifetime"));

    let out = signer(&["--ca-dir", "x"]).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "clap refuses a missing flag");
}
