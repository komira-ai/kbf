//! `kbf-guest`: runs one command per boot for the host, inside a VM guest.
//!
//! ```text
//! kbf-guest serve --unix-socket <path> --token-file <inputs>/kbf-guest.token \
//!   --inputs <inputs share> --outputs <outputs share>
//! kbf-guest session
//! ```
//!
//! `session` prints the launchd session type and exits; it is how CI checks what a
//! LaunchAgent and a LaunchDaemon see. Every subcommand refuses to run as root except
//! `probe-guest` and `probe-ready`, which record what each user can do in a guest
//! (src/probe.rs), and `mount-share`, the root mount daemon's bounded virtiofs mount
//! (src/mount.rs), and `probe-diag`, which says what a root process sees of the share
//! and which write step is refused (src/diag.rs), and `shutdown-listen`, the root daemon
//! that shuts the guest down when the host asks over virtio-socket (src/shutdown.rs), and
//! `sim`, the root daemon that runs the iOS simulator probe (src/sim.rs), and `mkuser`, the
//! root daemon that creates the guest's user once (src/mkuser.rs).

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_guest::diag;
use kbf_guest::frame::Kind;
use kbf_guest::mkuser;
use kbf_guest::mount;
use kbf_guest::probe;
use kbf_guest::server::{Agent, Config, HELLO_TIMEOUT};
use kbf_guest::session;
use kbf_guest::shutdown;
use kbf_guest::sim;
use kbf_guest::wire::TOKEN_LEN;

/// The agent inside a VM guest: runs one command per boot for the host.
#[derive(Debug, Parser)]
#[command(name = "kbf-guest", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve the host on a Unix socket.
    Serve {
        /// The socket to create; it must not exist.
        #[arg(long)]
        unix_socket: PathBuf,
        /// The per-boot token, 64 hex digits, written by the host.
        #[arg(long)]
        token_file: PathBuf,
        /// The inputs share; the command's working directory is inside it.
        #[arg(long)]
        inputs: PathBuf,
        /// The outputs share; the command's stdout and stderr go there.
        #[arg(long)]
        outputs: PathBuf,
    },
    /// Print the launchd session type (`Aqua`, `Background`, `System`; empty off
    /// macOS) and exit.
    Session,
    /// Write what this process can do in the guest (vsock, console ports, a virtiofs
    /// mount, the automount share) to a JSON file. Unlike the other subcommands this
    /// runs as any user, root included.
    ProbeGuest(probe::GuestArgs),
    /// Write a marker file once the guest has reached this point (the share mounted, a
    /// process running). Runs as any user, root included.
    ProbeReady(probe::ReadyArgs),
    /// Mount a virtiofs tag at a mount point, trying a bounded number of times; logs each
    /// failure to stderr and exits non-zero when none worked. Runs as any user, root
    /// included (the guest's mount daemon is root).
    MountShare(mount::MountArgs),
    /// Print, as JSON on stdout, what the process is and sees of the mounted share and
    /// which file operation on it is refused. Runs as any user, root included.
    ProbeDiag(diag::DiagArgs),
    /// Listen on a virtio-socket port for the host's `shutdown` command and shut the guest
    /// down cleanly (`/sbin/shutdown -h now`). Runs as root, as a LaunchDaemon; exits 0
    /// after the shutdown was started and non-zero when the listener fails.
    ShutdownListen {
        /// The guest vsock port to listen on.
        #[arg(long, default_value_t = shutdown::PORT)]
        port: u32,
    },
    /// Run the iOS simulator probe (Xcode first launch, a runtime added, a device created,
    /// booted and photographed) and send one frame per step to the host. Runs as root, as a
    /// LaunchDaemon, once per boot; the steps that need a user session run as the console
    /// user through `launchctl asuser`.
    Sim(sim::SimArgs),
    /// Create the guest's one user (a standard account with auto-login, no sleep or lock),
    /// send the host one frame per step, and power the guest off. Runs as root, as a
    /// LaunchDaemon, once per boot; it does nothing when the user is recorded as ready.
    Mkuser(mkuser::MkUserArgs),
}

/// The hosts the root daemons of the simulator setup run against (`None`: not a macOS guest).
#[derive(Clone, Copy, Default)]
struct Guests<'a> {
    sim: Option<&'a dyn sim::SimHost>,
    mkuser: Option<&'a dyn mkuser::MkUserHost>,
}

fn main() -> ExitCode {
    run(Cli::parse(), session::refuse_root_here())
}

/// Runs the subcommand `cli` names, unless `root` is the refusal to run as root
/// ([`session::refuse_root_here`]): then it says why on stderr and exits 2, with
/// nothing run and nothing created.
fn run(cli: Cli, root: Result<(), String>) -> ExitCode {
    run_with(
        cli,
        root,
        real_probe_host().as_deref(),
        real_mounter().as_deref(),
        real_diagnoser().as_deref(),
        real_shutdown_daemon().as_deref(),
        Guests {
            sim: real_sim_host().as_deref(),
            mkuser: real_mkuser_host().as_deref(),
        },
    )
}

/// [`run`] with the host the probe subcommands ask (`None`: not a macOS guest).
fn run_with(
    cli: Cli,
    root: Result<(), String>,
    host: Option<&dyn probe::Host>,
    mounter: Option<&dyn mount::Mounter>,
    diagnoser: Option<&dyn diag::DiagHost>,
    daemon: Option<&dyn shutdown::Daemon>,
    guests: Guests<'_>,
) -> ExitCode {
    if !matches!(
        cli.command,
        Cmd::ProbeGuest(_)
            | Cmd::ProbeReady(_)
            | Cmd::MountShare(_)
            | Cmd::ProbeDiag(_)
            | Cmd::ShutdownListen { .. }
            | Cmd::Sim(_)
            | Cmd::Mkuser(_)
    ) && let Err(e) = root
    {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match cli.command {
        Cmd::Session => {
            println!("{}", session::manager_name());
            ExitCode::SUCCESS
        }
        Cmd::Serve {
            unix_socket,
            token_file,
            inputs,
            outputs,
        } => match serve(&unix_socket, &token_file, inputs, outputs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kbf-guest: {e}");
                ExitCode::FAILURE
            }
        },
        Cmd::ProbeGuest(args) => probe_cmd(host, &args.out, Kind::Result, args.vsock_port, |h| {
            probe::probe_guest(h, &args)
        }),
        Cmd::ProbeReady(args) => probe_cmd(host, &args.out, Kind::Ready, args.vsock_port, |h| {
            probe::probe_ready(h, &args)
        }),
        Cmd::MountShare(args) => mount_cmd(mounter, &args),
        Cmd::ProbeDiag(args) => diag_cmd(diagnoser, &args),
        Cmd::ShutdownListen { port } => shutdown_cmd(daemon, port),
        Cmd::Sim(args) => sim_cmd(guests.sim, &args),
        Cmd::Mkuser(args) => mkuser_cmd(guests.mkuser, &args),
    }
}

/// Runs the iOS simulator probe with `host` (`None`: not a macOS guest, refuse with 2).
/// Exits 0 when the run ended and the host got its summary, whatever the steps' verdicts
/// (the host reads them from the frames), 1 when it was not root or the summary never
/// arrived.
fn sim_cmd(host: Option<&dyn sim::SimHost>, args: &sim::SimArgs) -> ExitCode {
    let Some(host) = host else {
        eprintln!("kbf-guest: sim runs inside a macOS guest");
        return ExitCode::from(2);
    };
    let summary = sim::run(host, &sim::Config::from(args), &mut std::io::stderr());
    let sent = !summary.undelivered.iter().any(|n| n == sim::SUMMARY_NAME);
    if summary.steps.is_empty() || !sent {
        eprintln!(
            "kbf-guest: sim: no run to report ({})",
            summary
                .note
                .as_deref()
                .unwrap_or("the summary frame did not reach the host")
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Creates the guest's user with `host` (`None`: not a macOS guest, refuse with 2). Exits 0
/// when the host got the summary and the user is ready (made now, or before), 1 when a step
/// failed, the run could not start or the summary never arrived.
fn mkuser_cmd(host: Option<&dyn mkuser::MkUserHost>, args: &mkuser::MkUserArgs) -> ExitCode {
    let Some(host) = host else {
        eprintln!("kbf-guest: mkuser runs inside a macOS guest");
        return ExitCode::from(2);
    };
    let done = mkuser::run(
        host,
        &mkuser::Config::from(args),
        &args.name,
        &mut std::io::stderr(),
    );
    let sent = !done
        .summary
        .undelivered
        .iter()
        .any(|n| n == mkuser::SUMMARY_NAME);
    if done.summary.ok && sent {
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "kbf-guest: mkuser: the user is not ready ({})",
        done.summary
            .note
            .as_deref()
            .unwrap_or("see the step frames the host received")
    );
    ExitCode::FAILURE
}

/// The host `mkuser` uses: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_mkuser_host() -> Option<Box<dyn mkuser::MkUserHost>> {
    Some(Box::new(kbf_guest::mkuser_host::RealMkUserHost::new()))
}

#[cfg(not(target_os = "macos"))]
fn real_mkuser_host() -> Option<Box<dyn mkuser::MkUserHost>> {
    None
}

/// The host `sim` uses: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_sim_host() -> Option<Box<dyn sim::SimHost>> {
    Some(Box::new(kbf_guest::sim_host::RealSimHost::new()))
}

#[cfg(not(target_os = "macos"))]
fn real_sim_host() -> Option<Box<dyn sim::SimHost>> {
    None
}

/// Runs the shutdown listener with `daemon` (`None`: not a macOS guest, refuse with 2).
fn shutdown_cmd(daemon: Option<&dyn shutdown::Daemon>, port: u32) -> ExitCode {
    let Some(daemon) = daemon else {
        eprintln!("kbf-guest: shutdown-listen runs inside a macOS guest");
        return ExitCode::from(2);
    };
    match daemon.run(port, &mut std::io::stderr()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-guest: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The daemon `shutdown-listen` runs: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_shutdown_daemon() -> Option<Box<dyn shutdown::Daemon>> {
    Some(Box::new(kbf_guest::shutdown_host::SystemDaemon))
}

#[cfg(not(target_os = "macos"))]
fn real_shutdown_daemon() -> Option<Box<dyn shutdown::Daemon>> {
    None
}

/// Diagnoses the share with `diagnoser` (`None`: not a macOS guest, refuse with 2).
fn diag_cmd(diagnoser: Option<&dyn diag::DiagHost>, args: &diag::DiagArgs) -> ExitCode {
    let Some(diagnoser) = diagnoser else {
        eprintln!("kbf-guest: probe-diag runs inside a macOS guest");
        return ExitCode::from(2);
    };
    let report = diag::diagnose(diagnoser, args);
    diag::emit(&report, &args.share, &mut std::io::stdout());
    diag::deliver(diagnoser, args.vsock_port, &report, &mut std::io::stderr());
    ExitCode::SUCCESS
}

/// The diagnoser `probe-diag` uses: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_diagnoser() -> Option<Box<dyn diag::DiagHost>> {
    Some(Box::new(kbf_guest::probe_host::SystemHost))
}

#[cfg(not(target_os = "macos"))]
fn real_diagnoser() -> Option<Box<dyn diag::DiagHost>> {
    None
}

/// Mounts the share `args` names with `mounter` (`None`: not a macOS guest, refuse with 2).
fn mount_cmd(mounter: Option<&dyn mount::Mounter>, args: &mount::MountArgs) -> ExitCode {
    let Some(mounter) = mounter else {
        eprintln!("kbf-guest: mount-share runs inside a macOS guest");
        return ExitCode::from(2);
    };
    match mount::mount_share(mounter, args, &mut std::io::stderr()) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-guest: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The mounter `mount-share` uses: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_mounter() -> Option<Box<dyn mount::Mounter>> {
    Some(Box::new(kbf_guest::probe_host::SystemHost))
}

#[cfg(not(target_os = "macos"))]
fn real_mounter() -> Option<Box<dyn mount::Mounter>> {
    None
}

/// The host the probes ask: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_probe_host() -> Option<Box<dyn probe::Host>> {
    Some(Box::new(kbf_guest::probe_host::SystemHost))
}

#[cfg(not(target_os = "macos"))]
fn real_probe_host() -> Option<Box<dyn probe::Host>> {
    None
}

/// Runs a probe against `host` and delivers its answer as `kind`: over vsock `port` first,
/// into `out` in the share when that fails (every attempt is logged on stderr); `None` for
/// the host means this is not a macOS guest: refuse with 2.
fn probe_cmd(
    host: Option<&dyn probe::Host>,
    out: &std::path::Path,
    kind: Kind,
    port: u32,
    run: impl FnOnce(&dyn probe::Host) -> Result<serde_json::Value, String>,
) -> ExitCode {
    let Some(host) = host else {
        eprintln!("kbf-guest: the probe subcommands run inside a macOS guest");
        return ExitCode::from(2);
    };
    eprintln!("{}", probe::started_line(host, out));
    let written = run(host).and_then(|v| {
        probe::deliver(host, port, kind, out, &v, &mut std::io::stderr())
            .map_err(|e| format!("write {}: {e}", out.display()))
    });
    match written {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-guest: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(
    socket: &std::path::Path,
    token_file: &std::path::Path,
    inputs: PathBuf,
    outputs: PathBuf,
) -> Result<(), String> {
    let text = std::fs::read_to_string(token_file)
        .map_err(|e| format!("reading {}: {e}", token_file.display()))?;
    let token: [u8; TOKEN_LEN] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            format!(
                "{} does not hold {} hex digits",
                token_file.display(),
                TOKEN_LEN * 2
            )
        })?;
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("binding {}: {e}", socket.display()))?;
    let mut agent = Agent::new(Config {
        token,
        inputs,
        outputs,
        session: session::manager_name(),
        hello_timeout: HELLO_TIMEOUT,
    });
    let served = agent.serve(&listener);
    served.map_err(|e| format!("accepting: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command line of `kbf-guest` with `args`.
    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("kbf-guest").chain(args.iter().copied()))
    }

    /// A `serve` whose token file does not exist, so that it fails (exit 1) if it runs
    /// at all.
    const SERVE_WITHOUT_A_TOKEN: [&str; 5] = [
        "serve",
        "--unix-socket=/nonexistent/kbf-guest.sock",
        "--token-file=/nonexistent/kbf-guest.token",
        "--inputs=/nonexistent/inputs",
        "--outputs=/nonexistent/outputs",
    ];

    /// Catches a root process that goes on to run its subcommand (defects: the refusal
    /// dropped, or answered with another status): given the refusal, `session` and
    /// `serve` both exit 2, where without it `session` exits 0 and a `serve` with no
    /// token file exits 1, so the status shows the subcommand did not run.
    #[test]
    fn a_process_that_is_root_exits_2_before_running_its_subcommand() {
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let serve = &SERVE_WITHOUT_A_TOKEN;
        assert_eq!(run(cli(&["session"]), refusal()), ExitCode::from(2));
        assert_eq!(run(cli(serve), refusal()), ExitCode::from(2));
        assert_eq!(run(cli(&["session"]), Ok(())), ExitCode::SUCCESS);
        assert_eq!(run(cli(serve), Ok(())), ExitCode::FAILURE);
    }

    fn h() -> H {
        H(std::cell::Cell::new(false), false)
    }

    /// A host whose vsock acknowledges every frame.
    fn h_acking() -> H {
        H(std::cell::Cell::new(false), true)
    }

    /// A host that answers every question; the flag is whether its vsock works.
    struct H(std::cell::Cell<bool>, bool);

    impl probe::Host for H {
        fn ids(&self) -> (u32, u32) {
            (1, 1)
        }
        fn session(&self) -> String {
            "System".to_owned()
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            self.0.get()
        }
        fn process_running(&self, _: &str) -> bool {
            true
        }
        fn vsock(&self, _: u32) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn send_frame(
            &self,
            _: u32,
            _: &kbf_guest::frame::Frame,
        ) -> Result<(), kbf_guest::frame::SendError> {
            if self.1 {
                Ok(())
            } else {
                Err(kbf_guest::frame::SendError::Unavailable("no device".into()))
            }
        }
        fn console(&self, _: &str) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn mount_tag(&self, _: &str, _: &std::path::Path) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn automount(&self, _: &std::path::Path) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn pause(&self, _: std::time::Duration) {
            self.0.set(true);
        }
    }

    /// Catches a probe whose answer is not written, and a failed probe or write that
    /// exits 0.
    #[test]
    fn probe_cmd_writes_the_answer_or_fails() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-ok");
        let out = dir.join("r.json");
        let ok = |_: &dyn probe::Host| Ok(serde_json::json!({"k": 1}));
        let go = |host: Option<&dyn probe::Host>,
                  out: &std::path::Path,
                  run: fn(&dyn probe::Host) -> Result<serde_json::Value, String>| {
            probe_cmd(host, out, Kind::Result, 5000, run)
        };
        assert_eq!(go(Some(&h()), &out, ok), ExitCode::SUCCESS);
        assert!(out.exists());
        assert_eq!(
            go(Some(&h()), &out, |_| Err("no".into())),
            ExitCode::FAILURE
        );
        assert_eq!(go(Some(&h()), &dir.join("x/r.json"), ok), ExitCode::FAILURE);
        assert_eq!(go(None, &out, ok), ExitCode::from(2));
    }

    /// Catches a probe that writes into the share although the host acknowledged its
    /// answer over vsock (both markers and results), and one that exits non-zero then.
    #[test]
    fn probes_deliver_over_vsock_and_leave_the_share_alone() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-vsock");
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let guest = dir.join("g.json");
        let ready = dir.join("r.json");
        let g = ["probe-guest", "--out", guest.to_str().expect("p")];
        let r = ["probe-ready", "--out", ready.to_str().expect("p")];
        for args in [&g[..], &r[..]] {
            assert_eq!(
                run_with(
                    cli(args),
                    refusal(),
                    Some(&h_acking()),
                    None,
                    None,
                    None,
                    Guests::default()
                ),
                ExitCode::SUCCESS
            );
        }
        assert!(!guest.exists());
        assert!(!ready.exists());
    }

    /// Catches a probe subcommand refused as root (it must run as any user: the root
    /// control arm of the mount question is root), one that does not reach its probe, and
    /// one that runs where there is no guest.
    #[test]
    fn probes_run_as_root_and_write_their_answer_to_the_file_asked() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-run");
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let guest = dir.join("g.json");
        let ready = dir.join("r.json");
        let g = [
            "probe-guest",
            "--out",
            guest.to_str().expect("p"),
            "--wait-path",
            "/w",
        ];
        assert_eq!(
            run_with(
                cli(&g),
                refusal(),
                Some(&h()),
                None,
                None,
                None,
                Guests::default()
            ),
            ExitCode::SUCCESS
        );
        let r = [
            "probe-ready",
            "--out",
            ready.to_str().expect("p"),
            "--wait-process",
            "p",
        ];
        assert_eq!(
            run_with(
                cli(&r),
                refusal(),
                Some(&h()),
                None,
                None,
                None,
                Guests::default()
            ),
            ExitCode::SUCCESS
        );
        let text = std::fs::read_to_string(&guest).expect("guest result");
        assert!(text.contains("\"uid\": 1"), "{text}");
        assert!(
            std::fs::read_to_string(&ready)
                .expect("ready result")
                .contains("\"session\"")
        );
        assert_eq!(
            run_with(
                cli(&r),
                refusal(),
                None,
                None,
                None,
                None,
                Guests::default()
            ),
            ExitCode::from(2)
        );
    }

    /// A mounter that works or fails as told.
    struct M(bool);

    impl mount::Mounter for M {
        fn mount(&self, _: &str, _: &std::path::Path) -> Result<(), String> {
            if self.0 { Ok(()) } else { Err("busy".into()) }
        }
        fn pause(&self, _: std::time::Duration) {}
    }

    /// Catches `mount-share` refused as root (the mount daemon is root), a failed mount
    /// that exits 0 (the ready marker would then never come and nothing would say why),
    /// and a run where there is no guest that does not exit 2.
    #[test]
    fn mount_share_runs_as_root_and_fails_loud() {
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let args = [
            "mount-share",
            "--tag=t",
            "--mountpoint=/m",
            "--attempts=2",
            "--delay-ms=0",
        ];
        let go = |m: Option<&dyn mount::Mounter>| {
            run_with(
                cli(&args),
                refusal(),
                None,
                m,
                None,
                None,
                Guests::default(),
            )
        };
        assert_eq!(go(Some(&M(true))), ExitCode::SUCCESS);
        assert_eq!(go(Some(&M(false))), ExitCode::FAILURE);
        assert_eq!(go(None), ExitCode::from(2));
    }

    /// A diagnoser that answers everything with null.
    struct D;

    impl diag::DiagHost for D {
        fn run(&self, _: &str, _: &[&str]) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn ids(&self) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            true
        }
        fn pause(&self, _: std::time::Duration) {}
        fn vsock_round_trip(&self, _: u32) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn mount_cycle(
            &self,
            _: &str,
            _: &std::path::Path,
            _: &dyn Fn(&std::path::Path) -> serde_json::Value,
        ) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn send_frame(
            &self,
            _: u32,
            _: &kbf_guest::frame::Frame,
        ) -> Result<(), kbf_guest::frame::SendError> {
            Ok(())
        }
    }

    /// Catches `probe-diag` refused as root (it is run by a root daemon), a diagnosis that
    /// fails the process (the report is the answer, a refused write is not an error), and
    /// a run where there is no guest that does not exit 2.
    #[test]
    fn probe_diag_runs_as_root_and_always_reports() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-diag");
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let share = format!("--share={}", dir.join("none").display());
        let control = format!("--control-dir={}", dir.join("none").display());
        let args = ["probe-diag", share.as_str(), control.as_str()];
        let go = |d: Option<&dyn diag::DiagHost>| {
            run_with(
                cli(&args),
                refusal(),
                None,
                None,
                d,
                None,
                Guests::default(),
            )
        };
        assert_eq!(go(Some(&D)), ExitCode::SUCCESS);
        assert_eq!(go(None), ExitCode::from(2));
    }

    /// A shutdown daemon that works or fails as told, recording the port it was given.
    struct Dm(Option<&'static str>, std::cell::Cell<u32>);

    impl shutdown::Daemon for Dm {
        fn run(&self, port: u32, _: &mut dyn std::io::Write) -> Result<(), String> {
            self.1.set(port);
            self.0.map_or(Ok(()), |e| Err(e.to_owned()))
        }
    }

    /// Catches `shutdown-listen` refused as root (it is a root daemon: `shutdown` needs
    /// root), a listener failure that exits 0 (launchd would not restart it and the host
    /// would never get an ack), the port flag ignored, and a run where there is no guest
    /// that does not exit 2.
    #[test]
    fn shutdown_listen_runs_as_root_on_its_port_and_fails_loud() {
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let go = |args: &[&str], d: Option<&dyn shutdown::Daemon>| {
            run_with(cli(args), refusal(), None, None, None, d, Guests::default())
        };
        let ok = Dm(None, std::cell::Cell::new(0));
        assert_eq!(go(&["shutdown-listen"], Some(&ok)), ExitCode::SUCCESS);
        assert_eq!(ok.1.get(), 5001);
        assert_eq!(
            go(&["shutdown-listen", "--port=6000"], Some(&ok)),
            ExitCode::SUCCESS
        );
        assert_eq!(ok.1.get(), 6000);
        let bad = Dm(Some("bind failed"), std::cell::Cell::new(0));
        assert_eq!(go(&["shutdown-listen"], Some(&bad)), ExitCode::FAILURE);
        assert_eq!(go(&["shutdown-listen"], None), ExitCode::from(2));
    }

    /// A simulator host with nothing staged: every command succeeds, the screenshot is a
    /// few bytes, and the vsock acknowledges (or not).
    struct S {
        root: bool,
        up: bool,
    }

    impl sim::SimHost for S {
        fn ready_user(&self) -> Option<String> {
            None
        }
        fn wanted_user(&self) -> sim::Wanted {
            sim::Wanted::No
        }
        fn is_root(&self) -> bool {
            self.root
        }
        fn runtime_images(&self) -> Vec<String> {
            Vec::new()
        }
        fn app_bundles(&self) -> Vec<String> {
            Vec::new()
        }
        fn console_user(&self) -> Option<sim::ConsoleUser> {
            Some(sim::ConsoleUser {
                uid: 501,
                name: "u".to_owned(),
            })
        }
        fn prepare_out_dir(&self) -> Result<(), String> {
            Ok(())
        }
        fn read_screenshot(&self, _: usize) -> Result<Vec<u8>, String> {
            Ok(vec![1, 2, 3])
        }
        fn run(&self, argv: &[String], _: std::time::Duration) -> sim::Outcome {
            let line = argv.join(" ");
            let stdout = if line.contains("list runtimes") {
                r#"{"runtimes":[{"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-27-0","version":"27.0","isAvailable":true}]}"#
            } else if line.contains("list devicetypes") {
                r#"{"devicetypes":[{"identifier":"t","name":"iPhone 16"}]}"#
            } else if line.contains("simctl create") {
                "6F3A1B2C-0D4E-4F5A-8B6C-7D8E9F0A1B2C\n"
            } else {
                ""
            };
            sim::Outcome {
                ended: sim::Ended::Exited(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
                cut: false,
                ms: 1,
            }
        }
        fn send(
            &self,
            _: u32,
            _: &kbf_guest::frame::Frame,
        ) -> Result<(), kbf_guest::frame::SendError> {
            if self.up {
                Ok(())
            } else {
                Err(kbf_guest::frame::SendError::Unavailable("no".into()))
            }
        }
        fn pause(&self, _: std::time::Duration) {}
    }

    /// Catches `sim` being refused as root (it is a root daemon, so the refusal that guards
    /// the agent must not apply), run off macOS, or reporting success without a summary the
    /// host received or when it was not root.
    #[test]
    fn sim_runs_as_root_and_exits_by_whether_the_host_got_its_summary() {
        let root = || Err("kbf-guest refuses to run as root".to_owned());
        let go = |host: Option<&dyn sim::SimHost>| {
            run_with(
                cli(&["sim"]),
                root(),
                None,
                None,
                None,
                None,
                Guests {
                    sim: host,
                    mkuser: None,
                },
            )
        };
        assert_eq!(
            go(Some(&S {
                root: true,
                up: true
            })),
            ExitCode::SUCCESS
        );
        assert_eq!(
            go(Some(&S {
                root: true,
                up: false
            })),
            ExitCode::FAILURE
        );
        assert_eq!(
            go(Some(&S {
                root: false,
                up: true
            })),
            ExitCode::FAILURE
        );
        assert_eq!(go(None), ExitCode::from(2));
    }
    /// A `mkuser` host on which every command works.
    struct Mk {
        root: bool,
        up: bool,
        ready: Option<String>,
    }

    impl mkuser::MkUserHost for Mk {
        fn is_root(&self) -> bool {
            self.root
        }
        fn user_exists(&self, _: &str) -> bool {
            false
        }
        fn ready_user(&self) -> Option<String> {
            self.ready.clone()
        }
        fn random_bytes(&self, n: usize) -> Result<Vec<u8>, String> {
            Ok(vec![7; n])
        }
        fn write_file(&self, _: &str, _: &[u8], _: u32) -> Result<(), String> {
            Ok(())
        }
        fn give_to_user(&self, _: &str, _: &str, _: u32, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn run(&self, argv: &[String], _: std::time::Duration) -> sim::Outcome {
            let ioreg = argv.first().is_some_and(|a| a.ends_with("/ioreg"));
            sim::Outcome {
                ended: sim::Ended::Exited(0),
                stdout: if ioreg {
                    b"\"IOPlatformUUID\" = \"1A2B3C4D-0000-4EEF-8123-ABCDEF012345\"\n".to_vec()
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
                cut: false,
                ms: 1,
            }
        }
        fn stream(
            &self,
            _: &[String],
            _: std::time::Duration,
            _: &mut dyn FnMut(&[u8]),
        ) -> sim::Streamed {
            sim::Streamed {
                ended: sim::Ended::Exited(0),
                ms: 1,
            }
        }
        fn send(
            &self,
            _: u32,
            _: &kbf_guest::frame::Frame,
        ) -> Result<(), kbf_guest::frame::SendError> {
            if self.up {
                Ok(())
            } else {
                Err(kbf_guest::frame::SendError::Unavailable("no".into()))
            }
        }
        fn pause(&self, _: std::time::Duration) {}
    }

    /// Catches `mkuser` refused as root (it is a root daemon), run off macOS without the
    /// refusal, a run with no `--name`, and an exit status that does not follow whether the
    /// user is ready and the host got the summary (launchd and the host both read it).
    #[test]
    fn mkuser_runs_as_root_and_exits_by_whether_the_user_is_ready_and_reported() {
        let root = || Err("kbf-guest refuses to run as root".to_owned());
        let go = |args: &[&str], host: Option<&dyn mkuser::MkUserHost>| {
            run_with(
                cli(args),
                root(),
                None,
                None,
                None,
                None,
                Guests {
                    sim: None,
                    mkuser: host,
                },
            )
        };
        let args = ["mkuser", "--name", "kbfuser"];
        let m = |root, up, ready: Option<&str>| Mk {
            root,
            up,
            ready: ready.map(str::to_owned),
        };
        assert_eq!(go(&args, Some(&m(true, true, None))), ExitCode::SUCCESS);
        assert_eq!(
            go(&args, Some(&m(true, true, Some("kbfuser")))),
            ExitCode::SUCCESS
        );
        assert_eq!(go(&args, Some(&m(true, false, None))), ExitCode::FAILURE);
        assert_eq!(go(&args, Some(&m(false, true, None))), ExitCode::FAILURE);
        assert_eq!(
            go(&["mkuser", "--name", "Root"], Some(&m(true, true, None))),
            ExitCode::FAILURE
        );
        assert_eq!(go(&args, None), ExitCode::from(2));
    }
}
