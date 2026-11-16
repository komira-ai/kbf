//! The Command of an action has no network unless the action asks (`kbf-network=allow`).
//!
//! Each case runs the `socket_probe` helper as a real Command and reads its exit code, which is
//! 0 for a call that worked and the errno for one that did not (`src/bin/socket_probe.rs`).
//! Nothing here needs a privilege, a network, or a clock.

#![cfg(target_os = "linux")]

use std::time::Duration;

use clap::Parser as _;
use kbf_proto::reapi::Command;
use kbf_task::network::{Layers, Network};
use kbf_task::run::{Ran, Run, run};
use kbf_task::task::{Cli, EXIT_RUN};
use kbf_testkit::dir::TestDir;

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
            unreadable: &[],
            hidden: &[],
            relay: None,
            job: None,
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

const EAFNOSUPPORT: i32 = 97;
const EOPNOTSUPP: i32 = 95;

/// Whether the namespace layer applied to a run that was to have no network. Any outcome but the
/// two a denied run can have fails the test: no case passes with a layer it does not know.
fn netns_applied(layers: &Layers) -> bool {
    match layers {
        Layers::SeccompAndNetns => true,
        Layers::SeccompOnly { .. } | Layers::SeccompOnlyForRoot => false,
        other => panic!("a denied run reported {other:?}"),
    }
}

/// The layers a denied run has on this host.
async fn layers_here() -> Layers {
    let layers = probe_run("layers-here", Network::Denied, &["unix"])
        .await
        .0
        .layers;
    netns_applied(&layers);
    layers
}

/// The exit code `socket(AF_INET)` has under `layers`: made in a namespace of the Command's own
/// (reaching `lo` only), refused by the strict filter otherwise.
fn inet_exit(layers: &Layers) -> i32 {
    if netns_applied(layers) { 0 } else { EPERM }
}

/// Catches: the test infrastructure not seeing which layer applied, and a run with the property
/// reporting anything but open.
#[tokio::test]
async fn the_layer_that_applied_is_always_known() {
    let first = layers_here().await;
    assert_eq!(layers_here().await, first);
    let (ran, _) = probe_run("layers-allowed", Network::Allowed, &["unix"]).await;
    assert_eq!(ran.layers, Layers::Open);
}

/// Catches: a family outside `AF_UNIX` getting a socket it should not. Netlink and packet
/// sockets read and (with a privilege) change the host's network and are refused with `EPERM`
/// in both modes. IPv4 and IPv6 are refused with `EPERM` by the strict filter, and made in a
/// namespace of the Command's own (reaching `lo` only); a `socketpair` of the internet family
/// is then refused by the kernel itself (`EOPNOTSUPP`), not by the filter.
#[tokio::test]
async fn by_default_no_socket_but_a_unix_one_can_be_made() {
    for mode in ["netlink", "packet"] {
        let (ran, _) = probe_run("denied", Network::Denied, &[mode]).await;
        netns_applied(&ran.layers);
        assert_eq!(ran.exit_code, EPERM, "{mode}");
    }
    let (ran, _) = probe_run("denied-inet", Network::Denied, &["inet"]).await;
    assert_eq!(ran.exit_code, inet_exit(&ran.layers), "inet");
    let netns = netns_applied(&ran.layers);
    let (ran, _) = probe_run("denied-inet6", Network::Denied, &["inet6"]).await;
    // A host without IPv6 answers `EAFNOSUPPORT` itself.
    let inet6: &[i32] = if netns { &[0, EAFNOSUPPORT] } else { &[EPERM] };
    assert!(inet6.contains(&ran.exit_code), "inet6: {}", ran.exit_code);
    let (ran, _) = probe_run("denied-pair", Network::Denied, &["pair-inet"]).await;
    assert_eq!(
        ran.exit_code,
        if netns { EOPNOTSUPP } else { EPERM },
        "pair-inet"
    );
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

/// Catches: the confinement being applied to the Command only and not inherited by what it
/// starts: a child and a grandchild see the same filter (and namespace) as the Command does.
#[tokio::test]
async fn a_process_the_command_starts_is_confined_too() {
    let (ran, _) = probe_run("child-inet", Network::Denied, &["child", "inet"]).await;
    assert_eq!(ran.exit_code, inet_exit(&ran.layers));
    let netns = netns_applied(&ran.layers);
    let (ran, _) = probe_run("child-nest", Network::Denied, &["child", "child", "inet6"]).await;
    let inet6: &[i32] = if netns { &[0, EAFNOSUPPORT] } else { &[EPERM] };
    assert!(inet6.contains(&ran.exit_code), "{}", ran.exit_code);
    for mode in ["netlink", "packet"] {
        let (ran, _) = probe_run("child-no", Network::Denied, &["child", "child", mode]).await;
        assert_eq!(ran.exit_code, EPERM, "{mode}");
    }
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

const ENETUNREACH: i32 = 101;

/// What a Command sees when the namespace layer is active: only `lo`, its own ids, loopback TCP
/// that works, no route anywhere else, and netlink and io_uring still refused.
async fn assert_namespace_layer() {
    let (ran, out) = probe_run("netns-ifaces", Network::Denied, &["ifaces"]).await;
    assert_eq!(ran.layers, Layers::SeccompAndNetns);
    assert_eq!(out.lines().collect::<Vec<_>>(), ["lo"], "{out:?}");
    let (_, ids) = probe_run("netns-ids", Network::Denied, &["ids"]).await;
    // SAFETY: no arguments.
    let own = format!("{} {}\n", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    assert_eq!(ids, own);
    assert_eq!(
        probe_exit("netns-lo", Network::Denied, &["tcp-loopback"]).await,
        0
    );
    assert_eq!(
        probe_exit("netns-inet", Network::Denied, &["inet"]).await,
        0
    );
    assert_eq!(
        probe_exit("netns-out", Network::Denied, &["tcp-outside"]).await,
        ENETUNREACH
    );
    for mode in ["netlink", "packet", "uring"] {
        assert_eq!(
            probe_exit("netns-no", Network::Denied, &[mode]).await,
            EPERM,
            "{mode}"
        );
    }
    assert_eq!(
        probe_exit("netns-child", Network::Denied, &["child", "tcp-outside"]).await,
        ENETUNREACH
    );
}

/// Catches: the namespace layer leaving the Command a host interface, changing the user it runs
/// as, or opening more than loopback. Where the host does not let an unprivileged user namespace
/// keep its capabilities (AppArmor's `apparmor_restrict_unprivileged_userns`) the layer reports
/// the step that failed and the strict filter applies: this case then checks that, and says it
/// skipped the rest. `netns_layer_is_active_strict` is the case that fails instead.
#[tokio::test]
async fn the_command_has_a_namespace_of_its_own_where_the_host_allows_one() {
    let (ran, _) = probe_run("netns-probe", Network::Denied, &["unix"]).await;
    match ran.layers {
        Layers::SeccompAndNetns => assert_namespace_layer().await,
        Layers::SeccompOnlyForRoot => {
            eprintln!("skipped: the Command runs as root, which gets no namespace")
        }
        Layers::SeccompOnly { step, errno } => {
            eprintln!("skipped: this host refuses the namespace layer ({step} {errno})");
            assert_ne!(errno, 0);
            // The strict filter applies, with no inet at all.
            for mode in ["inet", "inet6", "tcp-loopback"] {
                assert_ne!(
                    probe_exit("netns-failed", Network::Denied, &[mode]).await,
                    0,
                    "{mode}"
                );
            }
        }
        other => panic!("{other:?}"),
    }
}

/// The same checks, failing when the layer is not active. Run it where the namespace is
/// expected (a host or container that lets an unprivileged user namespace keep its
/// capabilities):
///
/// `cargo test -p kbf-task --test network -- --ignored netns_layer_is_active_strict`
#[tokio::test]
#[ignore = "needs a host that allows unprivileged user namespaces; run with --ignored"]
async fn netns_layer_is_active_strict() {
    assert_namespace_layer().await;
}

/// Catches: the layers not being named for a run, in either direction.
#[tokio::test]
async fn the_run_says_which_layers_confined_it() {
    let (ran, _) = probe_run("layers-denied", Network::Denied, &["unix"]).await;
    let line = ran.layers.to_string();
    assert!(
        line == "netns (loopback only) + seccomp"
            || line.starts_with("seccomp only, no inet (netns: "),
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
        .args([deny, protect, env!("CARGO_BIN_EXE_kbf-task"), probe()])
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
    // The error the run returned is a failure to start, which the tool exits 11 for.
    assert!(text.ends_with(&format!("code={EXIT_RUN}\n")), "{text}");
}

/// Catches: a namespace step that fails failing the run, or the Command then running without the
/// filter: with `unshare` refused the Command still runs, under the filter alone, and the
/// layers and the sandbox say so.
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
    assert_eq!(
        text,
        "layers=seccomp only, no inet (netns: unshare 1)\nsandbox=incomplete (unshare 1)\nexit=0\n"
    );
    assert!(marker.exists());
    for (n, mode) in ["inet", "inet6", "tcp-loopback", "netlink"]
        .into_iter()
        .enumerate()
    {
        let (_, text) = under(&format!("unshare-inet{n}"), "unshare", "no", &[mode]).await;
        assert!(
            text.contains("netns: unshare 1)\n") && text.contains("exit=1\n"),
            "{mode}: {text}"
        );
    }
}

/// Catches: a refused mount failing the run, or taking the network namespace with it: with
/// `mount` refused by an outer filter the Command still runs, and the sandbox line names the
/// step the host refused. Where the namespace steps succeed (or, for root, are not made) that is
/// the first mount step, `private`, and the network layer is whole; where the host refuses one of
/// them first (an unprivileged user namespace without its capabilities), that step is named.
#[tokio::test]
async fn a_refused_mount_leaves_the_run_and_the_network_layer_and_says_so() {
    let marker_dir = scratch("mount-marker");
    let marker = marker_dir.join("marker");
    let (_, text) = under(
        "mount",
        "mount",
        "no",
        &["touch", marker.to_str().expect("utf-8")],
    )
    .await;
    assert!(text.ends_with("exit=0\n"), "{text}");
    assert!(marker.exists(), "the Command did not run: {text}");
    // Root makes no user namespace, so its first refused step is the mount too.
    let namespaces_made = text.starts_with("layers=netns (loopback only) + seccomp\n")
        || text.starts_with("layers=seccomp only, no inet (netns: skipped for uid 0)\n");
    if namespaces_made {
        assert!(text.contains("sandbox=incomplete (private 1)\n"), "{text}");
    } else {
        eprintln!("this host refuses a namespace step first: {text}");
        assert!(text.contains("sandbox=incomplete ("), "{text}");
        assert!(!text.contains("sandbox=incomplete (private"), "{text}");
    }
}

/// Catches: a Command a signal ended reported as anything but 128 plus the signal where the run
/// made no PID namespace, so the signal reaches the tool as itself rather than as an exit code
/// of the namespace's first process.
#[tokio::test]
async fn without_a_namespace_a_signal_is_reported_as_128_plus_its_number() {
    let (_, text) = under("unshare-signal", "unshare", "no", &["signal"]).await;
    assert!(text.contains("sandbox=incomplete (unshare 1)\n"), "{text}");
    assert!(text.ends_with("exit=143\n"), "{text}");
}

/// Catches: the tool being open to the Command that runs beside it as the same user: the process
/// that starts the Command, once made non-dumpable, shows root-owned in `/proc` and refuses
/// `pidfd_getfd` of the socket it holds. Without it the process is visible as the user's.
/// The `hidden` check (ownership of `/proc/<ppid>/status`, which the kernel makes root's for a
/// non-dumpable process) is the primary assertion and fails without protection. The `steal`
/// check is kept as a second view, but where Yama's `ptrace_scope` is 1 it passes even without
/// protection, because a descendant may not trace its ancestor anyway.
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

/// Catches: `main` not making the tool non-dumpable before it does anything else (the Command it
/// starts runs as the same user and could otherwise take its sockets). The real binary is
/// started over a tree whose Action file is a FIFO, so it stops at the first read of it, which
/// is after `main`'s first instruction; opening the FIFO for writing returns exactly then. Its
/// `/proc/<pid>/status` is then root's, not ours. As root that comparison says nothing, so the
/// case ends there.
#[test]
fn the_binary_makes_itself_non_dumpable_before_reading_anything() {
    use std::os::unix::fs::MetadataExt as _;

    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root, whose files look the same either way");
        return;
    }
    let dir = scratch("binary-dumpable");
    std::fs::create_dir_all(dir.join("root/.kbf")).expect("root");
    std::fs::create_dir(dir.join("out")).expect("out");
    let fifo = dir.join("root/.kbf/action");
    let path = std::ffi::CString::new(fifo.to_str().expect("utf-8")).expect("no NUL");
    // SAFETY: `path` is NUL-terminated.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg(format!("--tree={}", dir.join("root").display()))
        .arg("--stage-and-commit")
        .arg(format!("--out-dir={}", dir.join("out").display()))
        .args([
            "--lease",
            "lease-1",
            "--action",
            &format!("{}/0", "0".repeat(64)),
        ])
        .spawn()
        .expect("kbf-task starts");
    // Returns when the binary opens the FIFO to read the token.
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("the binary reads the Action file");
    let owner = std::fs::metadata(format!("/proc/{}/status", child.id()))
        .expect("status")
        .uid();
    drop(writer);
    let status = child.wait().expect("exit");
    // SAFETY: no arguments.
    assert_ne!(owner, unsafe { libc::geteuid() }, "the binary is dumpable");
    // An empty Action is not the blob its digest names: a fetch failure.
    assert_eq!(status.code(), Some(10));
}

/// Catches: a tool that could not make itself non-dumpable going on to run the Command, which
/// could then take its sockets. It stops with the setup exit code before anything is read.
#[test]
fn a_tool_that_cannot_protect_itself_stops_with_the_setup_exit_code() {
    let cli = Cli::try_parse_from([
        "kbf-task",
        "--tree",
        "--stage-and-commit",
        "--lease=lease-1",
        &format!("--action={}/0", "0".repeat(64)),
    ])
    .expect("flags");
    let refused = kbf_task::task::run_cli_with(Err(std::io::Error::other("no")), &cli);
    assert_eq!(
        format!("{refused:?}"),
        format!(
            "{:?}",
            std::process::ExitCode::from(kbf_task::task::EXIT_SETUP)
        )
    );
}
