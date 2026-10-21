//! M0 of the Mac legs of the isolation suites (T018): a SPIKE that asserts nothing about
//! isolation and writes down what a macOS runner can do, so the suites that follow
//! (`no_residue`, `no_cross_access` on a Mac) are designed on facts.
//!
//! It makes two pool users `kbf-pool-0` and `kbf-pool-1` with `dscl` (uids from
//! [`pool::FIRST_UID`]), starts the real `kbf daemon --driver native --pool-users 2
//! --pool-first-uid U` as root behind the fake front kbf-cli's `tests/macos_daemon.rs`
//! uses, and runs actions through it. Every fact is a line starting `SPIKE`; the table at
//! the end (`SPIKE summary`) answers the questions of the plan (MAC_BLACKBOX_LEGS_PLAN.md,
//! M0 and Q6):
//!
//! - does `launchctl asuser` work for users made by `dscl` (no login, no GUI session)?
//! - does the launchd domain `user/<uid>` exist for them with nobody logged in?
//! - does a root daemon with a pool start on this runner, in a plain `dscl` layout (a home
//!   in a folder, not on the pool's APFS volume)?
//! - does an action run as a pool uid, and what do its `pwd` and environment hold?
//! - do `security`, `pbcopy`, `pbpaste` and `defaults` work for that user, headless?
//! - does the lease directory appear while the action runs, who owns it, and is it gone
//!   when the lease ends?
//! - how long does each step take?
//!
//! What it asserts is that the harness ran: the users were made, the daemon said `Hello`,
//! and every lease it started ended with a `Result`. A probe that fails is a fact, written
//! down. Nothing here can go red because the profile, the clean or a mode is wrong.
//!
//! It runs as root (`cargo xtask sudo-test -p kbf-daemon-test --test mac_pool_spike`), after
//! `cargo build -p kbf-cli --bin kbf --locked` (the daemon is found beside the test binary).
//! It touches only the users, group and folders named in `pool.rs`, and refuses to run when
//! a user or group of those names or ids is not its own (a provisioned pool).

#![cfg(all(target_os = "macos", not(coverage)))]

kbf_testkit::act_test_hook!();

// The fake front kbf-node's tests share; this file uses a part of it.
#[allow(dead_code)]
#[path = "../../../kbf-node/tests/front/mod.rs"]
mod front;
mod node;
mod pool;
mod record;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::{Duration, Instant};

use kbf_proto::worker::daemon_message::Message;

use node::{Daemon, Scratch};
use pool::{FIRST_UID, Pool};
use record::{lines, run, run_quiet, say, timed};

/// How long the daemon may take to say `Hello`: it cleans each slot first.
const HELLO_WITHIN: Duration = Duration::from_secs(120);

/// How long one lease may take to end.
const LEASE_WITHIN: Duration = Duration::from_secs(180);

/// `/bin/launchctl`.
const LAUNCHCTL: &str = "/bin/launchctl";

/// A program the spike has an action (or, as a check, the root test) run, and whether it
/// writes into the working directory (which only a lease's directory lets a pool user do).
struct Probe {
    label: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    writes: bool,
}

const PROBES: &[Probe] = &[
    p("id", "/usr/bin/id", &[], false),
    p("manager-name", LAUNCHCTL, &["managername"], false),
    p("manager-pid", LAUNCHCTL, &["managerpid"], false),
    p("launchctl-list", LAUNCHCTL, &["list"], false),
    p(
        "temp-dir",
        "/usr/bin/getconf",
        &["DARWIN_USER_TEMP_DIR"],
        false,
    ),
    p(
        "cache-dir",
        "/usr/bin/getconf",
        &["DARWIN_USER_CACHE_DIR"],
        false,
    ),
    p(
        "security-list",
        "/usr/bin/security",
        &["list-keychains"],
        false,
    ),
    p(
        "security-default",
        "/usr/bin/security",
        &["default-keychain"],
        false,
    ),
    p(
        "security-find",
        "/usr/bin/security",
        &["find-generic-password", "-s", "kbf-spike-none"],
        false,
    ),
    p(
        "security-create",
        "/usr/bin/security",
        &["create-keychain", "-p", "spike", "spike.keychain-db"],
        true,
    ),
    p("pbpaste", "/usr/bin/pbpaste", &[], false),
    p("pbcopy", "/usr/bin/pbcopy", &[], false),
    p(
        "defaults-read",
        "/usr/bin/defaults",
        &["read", "NSGlobalDomain", "AppleLocale"],
        false,
    ),
    p(
        "defaults-write",
        "/usr/bin/defaults",
        &["write", "com.komira.kbf-spike", "k", "v"],
        true,
    ),
];

const fn p(
    label: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    writes: bool,
) -> Probe {
    Probe {
        label,
        program,
        args,
        writes,
    }
}

/// The M0 spike. Root only: `#[ignore]`d so a plain `cargo test` skips it, run with
/// `--ignored` by `cargo xtask sudo-test`.
#[test]
#[ignore = "needs root: cargo xtask sudo-test -p kbf-daemon-test --test mac_pool_spike"]
fn the_harness_runs_and_the_runner_is_measured() {
    let total = Instant::now();
    assert!(
        rustix::process::geteuid().is_root(),
        "not root: run `cargo xtask sudo-test -p kbf-daemon-test --test mac_pool_spike`"
    );
    // Declared in the order they must drop in reverse: the daemon first, then the users
    // (every process of their uids ended, the records deleted), then the scratch directory.
    let scratch = Scratch::new();
    // The programs the spike runs as pool users start in a directory they can search (the
    // package's directory, where the test starts, is under the runner's home).
    std::env::set_current_dir(scratch.path()).expect("cd into the scratch directory");
    let mut facts: Vec<String> = Vec::new();

    environment();

    let pool = timed("create the pool users", || Pool::make(2));
    for user in &pool.users {
        run(
            "user",
            "/usr/bin/dscl",
            &[
                ".",
                "-read",
                &format!("/Users/{}", user.name),
                "UniqueID",
                "PrimaryGroupID",
                "NFSHomeDirectory",
                "UserShell",
                "IsHidden",
            ],
            10,
        );
        run("user", "/usr/bin/id", &[&user.name], 3);
        run(
            "user",
            "/bin/ls",
            &["-leOd@", &user.home.to_string_lossy()],
            4,
        );
    }

    // The domain of a user nobody has logged in as, before anything has asked launchd for it.
    for user in &pool.users {
        let domain = format!("user/{}", user.uid);
        let ran = run("domain-before-use", LAUNCHCTL, &["print", &domain], 4);
        facts.push(format!(
            "user-domain-exists-before-any-use ({domain}): {}",
            ran.status
        ));
    }

    let kbf = node::kbf();
    say("kbf", kbf.display());
    direct_probes(&pool, &kbf, &mut facts);

    // The daemon.
    let front = front::Front::start(&scratch.tls());
    let started = Instant::now();
    let mut daemon = Daemon::start(&scratch, front.worker.port(), pool.count(), FIRST_UID);
    say("daemon", format!("started, pid {}", daemon.pid()));
    let session =
        catch_unwind(AssertUnwindSafe(|| front.session(HELLO_WITHIN))).unwrap_or_else(|_| {
            say(
                "daemon",
                format!(
                    "no connection in {HELLO_WITHIN:?}; ended: {:?}",
                    daemon.ended()
                ),
            );
            lines("daemon-log", "log", &log_tail(&scratch, 80), 80);
            panic!("the daemon did not connect: it cannot be started as root with a pool here");
        });
    let hello = session.hello();
    say(
        "time daemon start to Hello",
        format!("{:.3?}", started.elapsed()),
    );
    say("hello", format!("node {}", hello.node_id));
    for capability in &hello.capabilities {
        say(
            "hello",
            format!("{} = {}", capability.key, capability.value),
        );
    }
    session.welcome();
    run(
        "daemon-process",
        "/bin/ps",
        &[
            "-o",
            "pid,uid,ruid,command",
            "-p",
            &daemon.pid().to_string(),
        ],
        3,
    );

    let mut lease = Leases {
        front: &front,
        session: &session,
        scratch: &scratch,
        next: 1,
    };

    // Lease 1: who the action is, and the lease directory while it runs.
    let go = scratch.path().join("go");
    let go_text = go.to_string_lossy().into_owned();
    let act = scratch.act();
    let identity = [
        act.as_str(),
        "id",
        ";",
        "pwd",
        ";",
        "env",
        "HOME",
        "unset",
        ";",
        "env",
        "TMPDIR",
        "unset",
        ";",
        "run",
        "/usr/bin/id",
        ";",
        "wait-for",
        go_text.as_str(),
        "60",
        ";",
        "echo",
        "finished",
    ];
    let mut observe = Observe::new(&scratch, &pool, &go);
    let first = lease.run("identity", &identity, &mut daemon, |_| observe.poll());
    let mut ran_as = ran_as_pool_uid(&first, &pool);
    facts.push(format!("action-ran-as-a-pool-uid: {ran_as}"));
    facts.push(format!(
        "lease-dir-appeared-while-the-action-ran: {}",
        observe
            .appeared_after
            .map_or("no".to_owned(), |t| format!("yes, {t:.3?} after Start"))
    ));
    let gone = observe.after_result(lease.next - 1);
    facts.push(format!("lease-dir-gone-after-the-Result: {gone}"));
    after_lease(&pool, &scratch);

    // A lease that failed to start because the daemon made `leases` closed to the pool user:
    // write it down, apply the one change that opens it, and measure again.
    if !first.succeeded() {
        let leases = scratch.leases();
        let mode = std::fs::metadata(&leases)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o7777);
        say(
            "workaround",
            format!(
                "lease 1 did not run to exit 0; {} has mode {mode:?}; opening it to 0711 and running the identity action again",
                leases.display()
            ),
        );
        facts.push(format!(
            "identity-lease-first-try-failed: {} ({})",
            first.summary(),
            leases.display()
        ));
        let _ =
            std::fs::set_permissions(&leases, std::os::unix::fs::PermissionsExt::from_mode(0o711));
        let again = [
            act.as_str(),
            "id",
            ";",
            "pwd",
            ";",
            "env",
            "HOME",
            "unset",
            ";",
            "env",
            "TMPDIR",
            "unset",
        ];
        let second = lease.run("identity-after-opening-leases", &again, &mut daemon, |_| {});
        ran_as = ran_as_pool_uid(&second, &pool);
        facts.push(format!(
            "action-ran-as-a-pool-uid-after-opening-leases: {ran_as}"
        ));
    }

    // One lease per probe: a program that fails ends its action, so each gets its own.
    for probe in PROBES {
        let mut argv = vec![act.as_str(), "run", probe.program];
        argv.extend(probe.args);
        let ran = lease.run(
            &format!("action-{}", probe.label),
            &argv,
            &mut daemon,
            |_| {},
        );
        facts.push(format!("as the action, {}: {}", probe.label, ran.summary()));
    }

    // Two actions at once, if the daemon runs them side by side: how many pool users show.
    let both = lease.at_once(&mut daemon);
    facts.push(format!("two-leases-at-once: {both}"));
    after_lease(&pool, &scratch);

    say("leases-dir", scratch.leases().display());
    run(
        "leases-dir",
        "/bin/ls",
        &["-leOa@", &scratch.leases().to_string_lossy()],
        20,
    );
    lines("daemon-log", "log", &log_tail(&scratch, 60), 60);

    say(
        "summary",
        "what the runner can do (the questions of the plan):",
    );
    for fact in &facts {
        say("summary", fact);
    }
    say("time total", format!("{:.3?}", total.elapsed()));
    say(
        "done",
        "the harness ran: users made, the daemon said Hello, every lease ended with a Result",
    );
}

/// What the machine is.
fn environment() {
    run("env", "/usr/bin/sw_vers", &[], 5);
    run("env", "/usr/bin/uname", &["-m"], 1);
    run("env", "/usr/bin/id", &[], 2);
    run(
        "env",
        "/usr/sbin/sysctl",
        &["-n", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"],
        3,
    );
    run("env", "/usr/bin/stat", &["-f", "%Su", "/dev/console"], 2);
    run("env", LAUNCHCTL, &["managername"], 1);
    run("env", LAUNCHCTL, &["managerpid"], 1);
    run("env", "/usr/bin/who", &[], 5);
    run("env", "/usr/bin/mdutil", &["-s", "/private/var"], 3);
    run("env", "/usr/bin/xcode-select", &["-p"], 2);
}

/// The probes the root test runs itself, for each pool user, the way the daemon starts an
/// action but without the daemon and the sandbox: `launchctl asuser <uid> <kbf> __as-user
/// <uid> <gid> -- <program>`. They answer the launchctl questions even when the daemon
/// does not start.
fn direct_probes(pool: &Pool, kbf: &Path, facts: &mut Vec<String>) {
    let kbf = kbf.to_string_lossy().into_owned();
    for user in &pool.users {
        let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
        // `asuser` alone changes the bootstrap namespace, not the uid.
        let alone = run(
            "asuser-alone",
            LAUNCHCTL,
            &["asuser", &uid, "/usr/bin/id", "-u"],
            3,
        );
        facts.push(format!(
            "launchctl asuser {uid} /usr/bin/id -u: {} {:?}",
            alone.status,
            alone.stdout.trim()
        ));
        for probe in PROBES.iter().filter(|p| !p.writes) {
            let mut args = vec![
                "asuser",
                &uid,
                &kbf,
                "__as-user",
                &uid,
                &gid,
                "--",
                probe.program,
            ];
            args.extend(probe.args);
            let ran = run(&format!("direct-{}", probe.label), LAUNCHCTL, &args, 6);
            if matches!(
                probe.label,
                "id" | "manager-name" | "security-list" | "pbpaste" | "pbcopy"
            ) {
                facts.push(format!(
                    "as {} without the daemon, {}: {}",
                    user.name,
                    probe.label,
                    Leases::summary_of(&ran.status, &ran.stdout, &ran.stderr)
                ));
            }
        }
        let domain = format!("user/{uid}");
        let after = run("domain-after-asuser", LAUNCHCTL, &["print", &domain], 4);
        facts.push(format!(
            "user-domain-exists-after-asuser ({domain}): {}",
            after.status
        ));
        let gui = format!("gui/{uid}");
        let ran = run("domain-gui", LAUNCHCTL, &["print", &gui], 2);
        facts.push(format!("gui-domain-exists ({gui}): {}", ran.status));
    }
}

/// Whether the identity action printed a uid of the pool.
fn ran_as_pool_uid(lease: &Lease, pool: &Pool) -> String {
    let printed = lease
        .stdout
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or_default();
    match printed.parse::<u32>() {
        Ok(uid) => match pool.users.iter().find(|u| u.uid == uid) {
            Some(user) => format!("yes, uid {uid} ({})", user.name),
            None => format!("NO, uid {uid} is not a pool uid"),
        },
        Err(_) => format!("no uid printed ({})", lease.summary()),
    }
}

/// What the machine holds of the pool users after a lease: their processes and domains.
fn after_lease(pool: &Pool, scratch: &Scratch) {
    let ps = run_quiet("/bin/ps", &["-axww", "-o", "pid,uid,ruid,stat,command"]);
    let ours: String = ps
        .stdout
        .lines()
        .filter(|l| {
            let mut w = l.split_whitespace().skip(1);
            let (uid, ruid) = (w.next(), w.next());
            pool.users.iter().any(|u| {
                let u = u.uid.to_string();
                uid == Some(u.as_str()) || ruid == Some(u.as_str())
            }) || l.contains(&*scratch.path().to_string_lossy())
        })
        .map(|l| format!("{l}\n"))
        .collect();
    say(
        "after-lease",
        format!("processes of the pool uids: {}", ours.lines().count()),
    );
    lines("after-lease", "ps", &ours, 10);
    for user in &pool.users {
        let domain = format!("user/{}", user.uid);
        run("after-lease", LAUNCHCTL, &["print", &domain], 3);
    }
}

/// The last `n` lines of the daemon's log.
fn log_tail(scratch: &Scratch, n: usize) -> String {
    let text = std::fs::read_to_string(scratch.log()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(n)..].join("\n")
}

/// The ends of a lease, as the root test sees them.
struct Lease {
    status: i32,
    message: String,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

impl Lease {
    /// The action ran and exited 0.
    fn succeeded(&self) -> bool {
        self.status == 0 && self.exit_code == Some(0)
    }

    fn summary(&self) -> String {
        if self.status != 0 {
            return format!(
                "no result: status {} {:?} ({:.3?})",
                self.status, self.message, self.took
            );
        }
        Leases::summary_of(
            &format!("exit {}", self.exit_code.unwrap_or(-1)),
            &self.stdout,
            &self.stderr,
        )
    }
}

/// Leases through the daemon, one at a time, numbered from 1.
struct Leases<'a> {
    front: &'a front::Front,
    session: &'a front::Session,
    scratch: &'a Scratch,
    next: u64,
}

impl Leases<'_> {
    /// `status, first line of stdout, first line of stderr`.
    fn summary_of(status: &str, stdout: &str, stderr: &str) -> String {
        let first = |text: &str| text.lines().next().unwrap_or_default().trim().to_owned();
        format!("{status}; out {:?}; err {:?}", first(stdout), first(stderr))
    }

    /// Runs `argv` as lease `1.<next>` and writes down how it ended; calls `meanwhile`
    /// every 100 ms until the `Result` comes.
    fn run(
        &mut self,
        what: &str,
        argv: &[&str],
        daemon: &mut Daemon,
        mut meanwhile: impl FnMut(u64),
    ) -> Lease {
        let seq = self.next;
        self.next += 1;
        let action = self.front.action(argv);
        let started = Instant::now();
        self.session.start(1, seq, action);
        let result = self.wait(seq, started, daemon, &mut meanwhile);
        let lease = self.finish(&result, started.elapsed());
        say(
            &format!("lease {what}"),
            format!("1.{seq} {}", lease.summary()),
        );
        say(&format!("time lease {what}"), format!("{:.3?}", lease.took));
        lines(&format!("lease {what}"), "out", &lease.stdout, 12);
        lines(&format!("lease {what}"), "err", &lease.stderr, 12);
        lease
    }

    fn wait(
        &self,
        seq: u64,
        started: Instant,
        daemon: &mut Daemon,
        meanwhile: &mut impl FnMut(u64),
    ) -> kbf_proto::worker::Result {
        loop {
            if let Some(status) = daemon.ended() {
                lines("daemon-log", "log", &log_tail(self.scratch, 80), 80);
                panic!("the daemon ended ({status}) during lease 1.{seq}");
            }
            assert!(
                started.elapsed() < LEASE_WITHIN,
                "no Result for lease 1.{seq} within {LEASE_WITHIN:?}; the daemon's log:\n{}",
                log_tail(self.scratch, 80)
            );
            meanwhile(seq);
            if let Some(Message::Result(result)) = self.session.next(Duration::from_millis(100)) {
                return result;
            }
        }
    }

    fn finish(&self, result: &kbf_proto::worker::Result, took: Duration) -> Lease {
        let status = result.status.clone().unwrap_or_default();
        let (exit_code, stdout, stderr) = match &result.action_result {
            Some(action) => {
                let read = |digest: &Option<kbf_proto::reapi::Digest>, raw: &[u8]| {
                    if raw.is_empty() {
                        String::from_utf8_lossy(&self.front.blob(digest.as_ref())).into_owned()
                    } else {
                        String::from_utf8_lossy(raw).into_owned()
                    }
                };
                (
                    Some(action.exit_code),
                    read(&action.stdout_digest, &action.stdout_raw),
                    read(&action.stderr_digest, &action.stderr_raw),
                )
            }
            None => (None, String::new(), String::new()),
        };
        Lease {
            status: status.code,
            message: status.message,
            exit_code,
            stdout,
            stderr,
            took,
        }
    }

    /// Two actions started together, each holding its lease until the root test lets go;
    /// says how many leases were live at once and which pool uids ran them.
    fn at_once(&mut self, daemon: &mut Daemon) -> String {
        let go = self.scratch.path().join("go-both");
        let (go_text, act) = (go.to_string_lossy().into_owned(), self.scratch.act());
        let argv = [act.as_str(), "id", ";", "wait-for", go_text.as_str(), "60"];
        let (first, second) = (self.next, self.next + 1);
        self.next += 2;
        let started = Instant::now();
        for seq in [first, second] {
            let action = self.front.action(&argv);
            self.session.start(1, seq, action);
        }
        let leases = self.scratch.leases();
        let live = || {
            [first, second]
                .iter()
                .filter(|s| leases.join(format!("lease-1-{s}")).exists())
                .count()
        };
        let mut most = 0;
        let mut seen = None;
        let mut results = Vec::new();
        while results.len() < 2 {
            if let Some(status) = daemon.ended() {
                panic!("the daemon ended ({status}) during the two leases");
            }
            assert!(
                started.elapsed() < LEASE_WITHIN,
                "the two leases did not end within {LEASE_WITHIN:?}; the daemon's log:\n{}",
                log_tail(self.scratch, 80)
            );
            most = most.max(live());
            if most == 2 && seen.is_none() {
                seen = Some(started.elapsed());
                // Let the two actions start before they are released.
                std::thread::sleep(Duration::from_secs(1));
                let ps = run_quiet("/bin/ps", &["-axww", "-o", "pid,uid,ruid,stat,command"]);
                let acts: String = ps
                    .stdout
                    .lines()
                    .filter(|l| l.contains("kbf-act") && l.contains("go-both"))
                    .map(|l| format!("{l}\n"))
                    .collect();
                lines("at-once", "ps", &acts, 6);
                let _ = std::fs::write(&go, "");
            }
            if let Some(Message::Result(result)) = self.session.next(Duration::from_millis(100)) {
                results.push(result);
            }
            if most < 2 && started.elapsed() > Duration::from_secs(45) && !go.exists() {
                // The daemon does not run them side by side: let the first go.
                let _ = std::fs::write(&go, "");
            }
        }
        let leases: Vec<Lease> = results
            .iter()
            .map(|r| self.finish(r, started.elapsed()))
            .collect();
        for lease in &leases {
            say(
                "at-once",
                format!("exit {:?} out {:?}", lease.exit_code, lease.stdout.trim()),
            );
        }
        let uids: Vec<&str> = leases.iter().map(|l| l.stdout.trim()).collect();
        format!(
            "most lease directories at one time: {most}; the actions' uids: {uids:?} (the lease directories of both seen after {})",
            seen.map_or("never".to_owned(), |t| format!("{t:.3?}"))
        )
    }
}

/// What the root test looks at while lease 1 runs: when its directory appears and what it
/// is, the action's process, and the launchd domain of its uid. It lets the action go once
/// it has looked.
struct Observe<'a> {
    scratch: &'a Scratch,
    pool: &'a Pool,
    go: &'a Path,
    begun: Instant,
    appeared_after: Option<Duration>,
    released: bool,
}

impl<'a> Observe<'a> {
    fn new(scratch: &'a Scratch, pool: &'a Pool, go: &'a Path) -> Self {
        Self {
            scratch,
            pool,
            go,
            begun: Instant::now(),
            appeared_after: None,
            released: false,
        }
    }

    fn dir(&self, seq: u64) -> std::path::PathBuf {
        self.scratch.leases().join(format!("lease-1-{seq}"))
    }

    fn poll(&mut self) {
        let dir = self.dir(1);
        if self.appeared_after.is_none() && dir.exists() {
            self.appeared_after = Some(self.begun.elapsed());
            say(
                "lease-dir",
                format!(
                    "{} appeared {:.3?} after Start",
                    dir.display(),
                    self.begun.elapsed()
                ),
            );
            run(
                "lease-dir",
                "/bin/ls",
                &["-leOd@", &dir.to_string_lossy()],
                4,
            );
            run(
                "lease-dir",
                "/bin/ls",
                &["-leOa@", &dir.to_string_lossy()],
                12,
            );
            run(
                "lease-dir",
                "/bin/ls",
                &["-leOd@", &self.scratch.leases().to_string_lossy()],
                2,
            );
            run(
                "lease-dir",
                "/bin/ls",
                &["-leOd@", &self.scratch.data().to_string_lossy()],
                2,
            );
        }
        let Some(appeared) = self.appeared_after else {
            return;
        };
        if self.released {
            return;
        }
        let ps = run_quiet("/bin/ps", &["-axww", "-o", "pid,uid,ruid,stat,command"]);
        let acting: Vec<&str> = ps
            .stdout
            .lines()
            .filter(|l| l.contains("kbf-act") && l.contains("wait-for"))
            .collect();
        let late = self.begun.elapsed() > appeared + Duration::from_secs(30);
        if acting.is_empty() && !late {
            return;
        }
        self.released = true;
        say(
            "lease-live",
            format!(
                "{} kbf-act process(es) waiting after {:.3?}",
                acting.len(),
                self.begun.elapsed()
            ),
        );
        for line in &acting {
            say("lease-live", line);
        }
        for user in &self.pool.users {
            let domain = format!("user/{}", user.uid);
            run("lease-live", LAUNCHCTL, &["print", &domain], 12);
            let uid = user.uid.to_string();
            run(
                "lease-live",
                "/bin/ps",
                &["-ww", "-U", &uid, "-o", "pid,ppid,uid,ruid,stat,command"],
                12,
            );
        }
        run(
            "lease-live",
            "/bin/ls",
            &["-leOd@", &self.scratch.leases().to_string_lossy()],
            2,
        );
        let _ = std::fs::write(self.go, "");
    }

    /// Waits for lease `seq`'s directory to go, up to 30 s, and says how long it took.
    fn after_result(&self, seq: u64) -> String {
        let dir = self.dir(seq);
        let started = Instant::now();
        while dir.exists() && started.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = if dir.exists() {
            run(
                "lease-dir",
                "/bin/ls",
                &["-leOaR@", &dir.to_string_lossy()],
                20,
            );
            format!("NO, {} is still there 30 s after the Result", dir.display())
        } else {
            format!("yes, {:.3?} after the Result", started.elapsed())
        };
        say("lease-dir", &text);
        text
    }
}
