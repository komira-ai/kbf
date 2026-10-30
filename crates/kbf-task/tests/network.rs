//! The Command of an action has no network unless the action asks (`kbf-network=allow`).
//!
//! Each case runs the `socket_probe` helper as a real Command and reads its exit code, which is
//! 0 for a call that worked and the errno for one that did not (`src/bin/socket_probe.rs`).
//! Nothing here needs a privilege, a network, or a clock.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser as _;
use kbf_proto::reapi::{Action, Command, Digest, Directory, Platform, platform::Property};
use kbf_segments::sha256;
use kbf_task::digest::to_reapi;
use kbf_task::fetch::{BlobSource, FetchError, label};
use kbf_task::network::{Layers, Network};
use kbf_task::run::{Ran, Run, run};
use kbf_task::task::{Cli, EXIT_RUN, Failure, PackSink, execute};
use kbf_task_pack::read_manifest;
use kbf_testkit::dir::TestDir;
use prost::Message as _;

const EPERM: i32 = 1;
/// A process the filter kills with `SIGSYS` (31) is reported as 128 plus the signal.
#[cfg(target_arch = "x86_64")]
const KILLED_BY_SIGSYS: i32 = 128 + 31;

fn probe() -> &'static str {
    env!("CARGO_BIN_EXE_socket_probe")
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-net-{name}"))
}

fn command(args: &[&str]) -> Command {
    Command {
        arguments: std::iter::once(probe())
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        ..Command::default()
    }
}

/// Runs the probe with `args` under `network`; its exit code and standard output.
async fn probe_run(name: &str, network: Network, args: &[&str]) -> (Ran, String) {
    let dir = scratch(name);
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("root");
    let ran: Ran = run(
        &command(args),
        &Run {
            root: &root,
            private: &dir.join("private"),
            stdout: &dir.join("stdout"),
            stderr: &dir.join("stderr"),
            deadline: Duration::from_secs(60),
            network,
        },
    )
    .await
    .expect("the probe runs");
    assert!(!ran.timed_out);
    let out = std::fs::read_to_string(dir.join("stdout")).expect("stdout");
    (ran, out)
}

/// Runs the probe with `args` under `network` and returns its exit code.
async fn probe_exit(name: &str, network: Network, args: &[&str]) -> i32 {
    probe_run(name, network, args).await.0.exit_code
}

/// Catches: a family outside `AF_UNIX` getting a socket by default. IPv4 and IPv6 are the
/// network; netlink and packet sockets read and (with a privilege) change the host's; both
/// `socket` and `socketpair` are refused, and the answer is `EPERM`.
#[tokio::test]
async fn by_default_no_socket_but_a_unix_one_can_be_made() {
    for mode in ["inet", "inet6", "netlink", "packet", "pair-inet"] {
        assert_eq!(
            probe_exit("denied", Network::Denied, &[mode]).await,
            EPERM,
            "{mode}"
        );
    }
}

/// Catches: the filter refusing every socket, which would break the local sockets build tools
/// use between their own processes.
#[tokio::test]
async fn by_default_unix_sockets_still_work() {
    for mode in ["unix", "pair-unix"] {
        assert_eq!(
            probe_exit("unix", Network::Denied, &[mode]).await,
            0,
            "{mode}"
        );
    }
}

/// Catches: a ring being a way round the socket rule (`IORING_OP_SOCKET`). Where the kernel
/// already forbids rings the answer is the same `EPERM`, so this case shows the rule only on
/// a kernel that allows them.
#[tokio::test]
async fn by_default_an_io_uring_cannot_be_set_up() {
    assert_eq!(
        probe_exit("uring", Network::Denied, &["uring"]).await,
        EPERM
    );
}

/// Catches: the filter being installed in the Command only and not inherited by what it starts.
#[tokio::test]
async fn a_process_the_command_starts_is_confined_too() {
    assert_eq!(
        probe_exit("child-inet", Network::Denied, &["child", "inet"]).await,
        EPERM
    );
    assert_eq!(
        probe_exit("child-nest", Network::Denied, &["child", "child", "inet6"]).await,
        EPERM
    );
    assert_eq!(
        probe_exit("child-unix", Network::Denied, &["child", "unix"]).await,
        0
    );
}

/// Catches: the x32 number of `socket` (the system call number with bit 30 set) reaching the
/// kernel's `socket` unfiltered. Without the filter the call fails with `ENOSYS` or makes a
/// socket; with it the process is killed.
#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn an_x32_system_call_number_kills_the_process() {
    assert_eq!(
        probe_exit("x32", Network::Denied, &["x32"]).await,
        KILLED_BY_SIGSYS
    );
}

/// Catches: the opt-in leaving the filter installed, or installing it in a descendant.
#[tokio::test]
async fn an_allowed_command_and_its_children_can_make_internet_sockets() {
    for args in [&["inet"][..], &["pair-unix"], &["child", "inet"]] {
        assert_eq!(
            probe_exit("allowed", Network::Allowed, args).await,
            0,
            "{args:?}"
        );
    }
}

/// Blobs in memory by digest, and a sink that keeps the packs.
#[derive(Default)]
struct Mem(HashMap<String, Vec<u8>>);

impl Mem {
    fn add(&mut self, bytes: Vec<u8>) -> Digest {
        let digest = to_reapi(&sha256(&bytes));
        self.0.insert(label(&digest), bytes);
        digest
    }
}

impl BlobSource for Mem {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.0
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| FetchError::Source {
                digest: label(digest),
                why: "absent".to_owned(),
            })
    }
}

#[derive(Default)]
struct Kept(Mutex<Vec<Vec<u8>>>);

impl PackSink for Kept {
    fn store(&self, body: &[u8]) -> Result<(), kbf_task::put::PutError> {
        self.0.lock().expect("lock").push(body.to_vec());
        Ok(())
    }
}

fn properties(pairs: &[(&str, &str)]) -> Platform {
    Platform {
        properties: pairs
            .iter()
            .map(|(name, value)| Property {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

/// An Action running the probe with `args`, its platform `platform` (on the Action) or, when
/// `on_command`, on the Command.
#[allow(deprecated)] // `Command::platform`
fn action(mem: &mut Mem, args: &[&str], platform: Option<Platform>, on_command: bool) -> Digest {
    let root = mem.add(Directory::default().encode_to_vec());
    let mut command = command(args);
    let mut action_platform = None;
    if on_command {
        command.platform = platform;
    } else {
        action_platform = platform;
    }
    let command = mem.add(command.encode_to_vec());
    mem.add(
        Action {
            command_digest: Some(command),
            input_root_digest: Some(root),
            platform: action_platform,
            ..Action::default()
        }
        .encode_to_vec(),
    )
}

fn cli(work: &Path, action: &Digest) -> Cli {
    Cli::try_parse_from([
        "kbf-task",
        "--pack-url-file",
        "/nonexistent-url",
        "--grant-url",
        "https://grant.example",
        "--grant-token-file",
        "/nonexistent",
        "--lease",
        "lease-1",
        "--action",
        &format!("{}/{}", action.hash, action.size_bytes),
        "--work-dir",
        work.to_str().expect("utf-8"),
    ])
    .expect("flags")
}

/// The exit code the manifest of the one pack `execute` stored holds.
async fn manifest_exit(
    name: &str,
    args: &[&str],
    platform: Option<Platform>,
    on_command: bool,
) -> i32 {
    let mut mem = Mem::default();
    let digest = action(&mut mem, args, platform, on_command);
    let dir = scratch(name);
    let sink = Kept::default();
    execute(&cli(&dir, &digest), &mem, &sink)
        .await
        .expect("task");
    let stored = sink.0.lock().expect("lock");
    assert_eq!(stored.len(), 1);
    read_manifest(&stored[0]).expect("manifest").exit_code
}

/// Catches: the property not reaching the filter decision through the whole task: without it an
/// IPv4 socket is refused, with `kbf-network=allow` (on the Action, on the Command when the
/// Action has none, and in any case of the name) it is made.
#[tokio::test]
async fn the_task_runs_the_command_open_only_for_the_allow_property() {
    let allow = || Some(properties(&[("kbf-network", "allow")]));
    assert_eq!(manifest_exit("t-none", &["inet"], None, false).await, EPERM);
    assert_eq!(manifest_exit("t-act", &["inet"], allow(), false).await, 0);
    assert_eq!(manifest_exit("t-cmd", &["inet"], allow(), true).await, 0);
    let shouted = Some(properties(&[("KBF-Network", "allow")]));
    assert_eq!(manifest_exit("t-case", &["inet"], shouted, false).await, 0);
    let other = Some(properties(&[("Pool", "big")]));
    assert_eq!(
        manifest_exit("t-other", &["inet"], other, false).await,
        EPERM
    );
}

/// Catches: a value the property does not take (including `Allow`, `true`, `1` and the empty
/// string), or the property twice, running the Command. The action fails as a run-start
/// (exit 11) and nothing is stored.
#[tokio::test]
async fn any_other_value_or_a_repeat_is_refused_as_a_run_start() {
    let cases = [
        vec![("kbf-network", "Allow")],
        vec![("kbf-network", "true")],
        vec![("kbf-network", "")],
        vec![("kbf-network", "deny")],
        vec![("kbf-network", "allow"), ("KBF-NETWORK", "allow")],
    ];
    for (n, pairs) in cases.iter().enumerate() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["inet"], Some(properties(pairs)), false);
        let dir = scratch(&format!("refuse{n}"));
        let sink = Kept::default();
        let failure = execute(&cli(&dir, &digest), &mem, &sink)
            .await
            .expect_err("refused");
        assert!(matches!(failure, Failure::Run(_)), "{pairs:?}: {failure}");
        assert_eq!(failure.exit_code(), EXIT_RUN, "{pairs:?}");
        assert!(failure.to_string().contains("kbf-network"), "{failure}");
        assert!(sink.0.lock().expect("lock").is_empty());
    }
}

/// Catches: a 32-bit system call (`int 0x80`, here `getpid`) getting past a filter that compares
/// system call numbers only: its numbers are another table, where `socketcall` is the one
/// socket entry. Under the filter the process is killed; without it the call is served.
#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn a_32_bit_system_call_kills_the_process_unless_the_network_is_allowed() {
    assert_eq!(
        probe_exit("int80-denied", Network::Denied, &["int80"]).await,
        KILLED_BY_SIGSYS
    );
    assert_eq!(
        probe_exit("int80-open", Network::Allowed, &["int80"]).await,
        0
    );
}

/// Catches: the network namespace layer leaving the Command a host interface, or changing the
/// user it runs as (which would change who owns what it writes). Where the host does not let an
/// unprivileged user namespace keep its capabilities (AppArmor's
/// `apparmor_restrict_unprivileged_userns`), the layer reports the step that failed and this
/// case checks that report instead.
#[tokio::test]
async fn the_command_has_a_namespace_of_its_own_with_loopback_only_and_its_own_ids() {
    let (ran, out) = probe_run("netns-ifaces", Network::Denied, &["ifaces"]).await;
    let (_, ids) = probe_run("netns-ids", Network::Denied, &["ids"]).await;
    // SAFETY: no arguments.
    let own = format!("{} {}\n", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    match ran.layers {
        Layers::SeccompAndNetns => {
            assert_eq!(out.lines().collect::<Vec<_>>(), ["lo"], "{out:?}");
            assert_eq!(ids, own);
        }
        Layers::SeccompOnly { step, errno } => {
            eprintln!("skipped: this host refuses the namespace layer ({step} {errno})");
            assert_ne!(errno, 0);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches: the layers not being named for a run, in either direction.
#[tokio::test]
async fn the_run_says_which_layers_confined_it() {
    let (ran, _) = probe_run("layers-denied", Network::Denied, &["unix"]).await;
    let line = ran.layers.to_string();
    assert!(
        line == "seccomp+netns" || line.starts_with("seccomp only (netns: "),
        "{line}"
    );
    let (ran, _) = probe_run("layers-open", Network::Allowed, &["unix"]).await;
    assert_eq!(ran.layers, Layers::Open);
}

/// What `run_under` printed, `key=value` lines.
async fn under(name: &str, deny: &str, protect: &str, args: &[&str]) -> (TestDir, String) {
    let dir = scratch(name);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_run_under"))
        .arg(dir.join("work"))
        .args([deny, protect, probe()])
        .args(args)
        .output()
        .expect("run_under starts");
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(
        out.status.success(),
        "{text} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, text)
}

/// Catches: a filter that cannot be installed leaving the Command to run with the network
/// (`let _ = install(..)`): the outer filter answers the `seccomp` call with `EPERM`, the run
/// must not start, and the program must never have run.
#[tokio::test]
async fn a_filter_that_cannot_be_installed_fails_the_run_and_the_command_never_runs() {
    let marker_dir = scratch("failclosed-marker");
    let marker = marker_dir.join("marker");
    let (_, text) = under(
        "failclosed",
        "seccomp",
        "no",
        &["touch", marker.to_str().expect("utf-8")],
    )
    .await;
    assert!(text.starts_with("error=cannot start "), "{text}");
    assert!(!text.contains("exit="), "{text}");
    assert!(!marker.exists(), "the Command ran");
    // The same run maps to the run-start exit code.
    let failure = Failure::Run(kbf_task::run::RunError::Invalid(text));
    assert_eq!(failure.exit_code(), EXIT_RUN);
}

/// Catches: a namespace step that fails failing the run, or the Command then running without the
/// filter: with `unshare` refused the Command still runs, under the filter alone, and the
/// layers say so.
#[tokio::test]
async fn a_namespace_that_cannot_be_made_leaves_the_filter_and_says_so() {
    let marker_dir = scratch("unshare-marker");
    let marker = marker_dir.join("marker");
    let (_, text) = under(
        "unshare",
        "unshare",
        "no",
        &["touch", marker.to_str().expect("utf-8")],
    )
    .await;
    assert_eq!(text, "layers=seccomp only (netns: unshare 1)\nexit=0\n");
    assert!(marker.exists());
    let (_, text) = under("unshare-inet", "unshare", "no", &["inet"]).await;
    assert_eq!(text, "layers=seccomp only (netns: unshare 1)\nexit=1\n");
}

/// Catches: the tool being open to the Command that runs beside it as the same user: the process
/// that starts the Command, once made non-dumpable, shows root-owned in `/proc` and refuses
/// `pidfd_getfd` of the socket it holds. Without it the process is visible as the user's.
#[tokio::test]
async fn the_tool_is_non_dumpable_so_the_command_cannot_take_its_sockets() {
    let (_, text) = under("protect-hidden", "unshare", "yes", &["hidden", "{PID}"]).await;
    assert!(text.ends_with("exit=0\n"), "{text}");
    let (_, text) = under(
        "protect-steal",
        "unshare",
        "yes",
        &["steal", "{PID}", "{FD}"],
    )
    .await;
    assert!(text.ends_with("exit=1\n"), "{text}");
    let (_, text) = under("unprotected-hidden", "unshare", "no", &["hidden", "{PID}"]).await;
    assert!(text.ends_with("exit=1\n"), "{text}");
}
