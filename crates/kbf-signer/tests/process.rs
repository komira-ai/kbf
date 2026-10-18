//! The `kbf-signer` binary: flags, startup, and the hardening of the running process.

mod support;

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
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
            &support::node_tbs_at(&ca, kbf_types::Role::Daemon, None, support::real_now()),
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

// Catches: a start that fails silently or with the key in its message: a CA directory that
// is not a directory, a lifetime outside the policy and a flag left out each exit non-zero with a
// message naming the program, and nothing that looks like key material.
#[test]
fn a_bad_start_exits_nonzero_and_says_why() {
    let scratch = scratch("bad_start");
    let socket = scratch.join("run").join("sock");
    // A CA directory that is a file: a missing one is made now, so it cannot stand here.
    std::fs::write(scratch.join("not-a-dir"), b"x").expect("write");
    let mut child = signer(&[
        "--ca-dir",
        support::path_str(&scratch.join("not-a-dir")),
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

/// Starts the binary on `ca_dir` and `socket`, serving the test's own account.
fn start(ca_dir: &std::path::Path, socket: &std::path::Path) -> Running {
    Running(
        signer(&[
            "--ca-dir",
            support::path_str(ca_dir),
            "--socket",
            support::path_str(socket),
            "--frontend-uid",
            &euid().to_string(),
        ])
        .spawn()
        .expect("spawn"),
    )
}

/// The signer's public key once it serves on `socket`.
fn wait_for_key(running: &mut Running, socket: &std::path::Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = running.0.try_wait().expect("try_wait") {
            let mut said = String::new();
            if let Some(mut stderr) = running.0.stderr.take() {
                let _ = stderr.read_to_string(&mut said);
            }
            panic!("the signer exited ({status}): {said}");
        }
        match SocketSigner::connect(socket, Route::Node) {
            Ok(client) => return client.public_key().to_vec(),
            Err(e) => assert!(Instant::now() < deadline, "the signer never came up: {e}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// Catches: a binary that only loads (a fresh install has no CA: the signer owns ca.key, so
// it must make the CA at first start), a created CA that a restart replaces (a new root per
// restart orphans every certificate: same key and same pin), and a directory made with
// anything but exactly ca.key and ca.pem. Both a missing directory and an empty one are
// fresh.
#[test]
fn the_binary_makes_the_ca_on_a_fresh_directory_and_a_restart_loads_it() {
    let scratch = scratch("fresh_start");
    std::fs::create_dir(scratch.join("empty")).expect("dir");
    std::fs::set_permissions(
        scratch.join("empty"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("chmod");
    for fresh in ["missing", "empty"] {
        let ca_dir = scratch.join(fresh);
        let socket = scratch.join(format!("run-{fresh}")).join("sock");
        let mut first = start(&ca_dir, &socket);
        let key = wait_for_key(&mut first, &socket);
        assert_eq!(names(&ca_dir), ["ca.key", "ca.pem"], "{fresh}");
        let pin = kbf_ca::Authority::load_dir(&ca_dir, kbf_ca::Policy::default())
            .expect("loads")
            .pin();
        drop(first);

        let mut second = start(&ca_dir, &socket);
        assert_eq!(
            wait_for_key(&mut second, &socket),
            key,
            "{fresh}: the same CA key"
        );
        let again = kbf_ca::Authority::load_dir(&ca_dir, kbf_ca::Policy::default())
            .expect("loads")
            .pin();
        assert_eq!(pin, again, "{fresh}: the same pin");
        assert_eq!(names(&ca_dir), ["ca.key", "ca.pem"], "{fresh}");
    }
}

// Catches: a start that completes a half-made CA (a lone ca.key given a new root, or any
// stray file tolerated), and one that creates the socket's directory before it knows the
// CA is fit. It exits non-zero with a message and writes nothing.
#[test]
fn the_binary_refuses_a_half_filled_directory_and_writes_nothing() {
    let ca = make_ca("half_filled");
    let socket = ca.scratch.join("run").join("sock");
    for (name, keep) in [("only_key", "ca.key"), ("only_pem", "ca.pem")] {
        let dir = ca.scratch.join(name);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        std::fs::copy(ca.dir.join(keep), dir.join(keep)).expect("copy");
        let out = signer(&[
            "--ca-dir",
            support::path_str(&dir),
            "--socket",
            support::path_str(&socket),
            "--frontend-uid",
            &euid().to_string(),
        ])
        .output()
        .expect("run");
        assert!(!out.status.success(), "{name}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let said = format!("has only {keep}: ");
        assert!(stderr.contains(&said), "{name}: {stderr}");
        assert_eq!(names(&dir), [keep], "{name}: nothing written");
        assert!(!socket.parent().expect("dir").exists(), "{name}");
    }
}
