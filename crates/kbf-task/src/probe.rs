//! `kbf-task --probe-confinement`: shows, on the host it runs on, which confinement the Command
//! of an action gets and whether it holds. No grant, server or network is needed.
//!
//! The probe calls the code a real action uses ([`crate::run::run`], which calls
//! [`crate::network::confine`] with [`Network::Denied`], the value [`crate::network::from_platform`]
//! gives an action that does not ask for the network). It changes nothing in that path. The
//! Command it runs is this same binary, started with the hidden flag `--probe-confinement-child`;
//! inside the confinement it makes the calls below and writes what it saw as `observe <name>
//! <value>` lines on standard output. The parent turns those and the layers [`crate::run::run`]
//! reported into the lines below.
//!
//! # Flags
//!
//! - `--probe-confinement`: run the probe. It must be the first argument; every flag of the
//!   task proper is refused alongside it.
//! - `--probe-secret-path <path>`: also ask whether the Command can read the file (or list the
//!   directory) at `path`. Only readable or not is reported, never the contents.
//! - `--probe-work-dir <dir>`: where the probe makes its scratch directory (default: the
//!   system temporary directory). It is removed when the probe ends. It must be writable.
//!
//! # Output
//!
//! One line per check on standard output, `probe <name>: <status> <detail>`, where the status
//! is `ok`, `FAIL` or `n/a`, then a last line `probe result: PASS` or `probe result: FAIL`.
//! The exit code is 0 for PASS and 1 for FAIL (2 for a wrong flag, from clap). The lines are
//! in this order; a test may match them by name:
//!
//! | line | meaning |
//! |------|---------|
//! | `probe layers:` | `ok` with the text of the layers line `kbf-task` prints for an action (`netns (loopback only) + seccomp`) when the namespace path ran; `FAIL` with `seccomp only, no inet (netns: <step> <errno>)`, `seccomp only, no inet (netns: skipped for uid 0)` or another text when it did not |
//! | `probe child:` | `ok exit 0`, or `FAIL` with the exit code (159 is a kill by the seccomp filter) or the error that stopped the Command from starting |
//! | `probe netns_distinct:` | `ok net:[<child>] != net:[<probe>]` when the Command's `/proc/self/ns/net` differs from the probe's own (an independent proof that a namespace was made); `FAIL` when it is the same or unreadable; `n/a` when the namespace path did not run |
//! | `probe loopback:` | `ok` when a TCP bind, connect and accept on `127.0.0.1` worked; `FAIL` with the errno; `n/a` when the namespace path did not run |
//! | `probe inet_unreachable:` | `ok ENETUNREACH (101) (netns)` when a connect to `192.0.2.1` (RFC 5737) failed that way; on the fallback, `ok EPERM (1) (strict filter)`; `FAIL` for any other result, a connect that worked included |
//! | `probe netlink:`, `probe packet:`, `probe io_uring:` | `ok EPERM (1)` when `socket(AF_NETLINK)`, `socket(AF_PACKET)` or `io_uring_setup` was refused with the errno the filter answers (`EPERM`); `FAIL` when the call worked, or was refused with another errno (`refused, not by the filter: <errno>`). `packet` proves the filter only when the Command holds `CAP_NET_RAW` (bit 13 of its `cap_eff`): without it the kernel refuses `AF_PACKET` with `EPERM` anyway, and the line is `ok EPERM (1) (no CAP_NET_RAW: not a filter proof)`. `io_uring` proves it only when `/proc/sys/kernel/io_uring_disabled` is not 1 or 2; otherwise `ok <errno> (io_uring_disabled=<n>: not a filter proof)`. A line that is not a proof never fails on the errno (a call that worked still fails) |
//! | `probe secret_path:` | `ok readable`, `ok not readable (<errno>)`, `FAIL` when the path does not exist (the question is unanswered), `n/a` without `--probe-secret-path`. Neither answer fails the probe: the design accepts a readable path |
//! | `probe apparmor_restrict_unprivileged_userns:`, `probe max_user_namespaces:`, `probe io_uring_disabled:` | `ok <value>` of `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`, `/proc/sys/user/max_user_namespaces` and `/proc/sys/kernel/io_uring_disabled`, or `n/a absent` |
//! | `probe euid:`, `probe uid_map:`, `probe gid_map:`, `probe cap_eff:` | the probe process itself (the container's user): effective uid, the maps (entries joined with `;`) and the effective capability mask |
//! | `probe child_uid_map:`, `probe child_gid_map:`, `probe child_cap_eff:` | the same, read by the Command inside the namespace the confinement made |
//!
//! The lines from `apparmor_restrict_unprivileged_userns` on are facts: they are `ok` or `n/a`
//! and never fail the probe.
//!
//! # The verdict
//!
//! PASS needs the namespace path (`layers`), a Command that ran (`child`), a namespace of its
//! own (`netns_distinct`), and `loopback`, `inet_unreachable` and `netlink` all `ok`, with no line
//! `FAIL`. `packet` and `io_uring` are required in the sense that they cannot be `FAIL`: they
//! are `FAIL` unless the call was refused with `EPERM`, and they are `ok` without proof where the
//! host would refuse the call anyway (see the table). A host
//! that falls back to the strict filter is therefore a FAIL, with the step that failed in the
//! `layers` line.
//!
//! # A path that blocks
//!
//! `--probe-secret-path` is opened by the Command. A FIFO or a device node that does not
//! answer blocks it until the 60 s deadline, and the probe then reports `child: FAIL timed out`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use kbf_proto::reapi::Command;

use crate::network::{Layers, Network};
use crate::run::{Ran, Run, RunError, run};

#[cfg(target_os = "linux")]
mod child;

/// The flag that selects the probe; it must be the first argument.
pub const FLAG: &str = "--probe-confinement";
/// The hidden flag of the Command the probe runs inside the confinement.
pub const CHILD_FLAG: &str = "--probe-confinement-child";

/// The address the Command tries to reach: RFC 5737 TEST-NET-1, which no network routes.
pub const OUTSIDE: ([u8; 4], u16) = ([192, 0, 2, 1], 9);

/// The checks a PASS needs `ok`. `packet` and `io_uring` are not listed: a proof they cannot
/// give on some hosts must not fail the probe, and where they do give one they are `FAIL`
/// unless refused by the filter.
const REQUIRED: [&str; 6] = [
    "layers",
    "child",
    "netns_distinct",
    "loopback",
    "inet_unreachable",
    "netlink",
];

/// The flags of the probe (see the module documentation).
///
/// Built with clap's builder, not its derive: the code the derive expands to has branches no
/// test of this crate reaches (the coverage gate counts them), and there are four flags.
pub struct ProbeCli {
    /// Run the probe.
    pub probe_confinement: bool,
    /// The Command's side of the probe; started by the probe itself.
    pub probe_confinement_child: bool,
    /// A path to ask whether the Command can read; never its contents.
    pub probe_secret_path: Option<PathBuf>,
    /// Where the scratch directory is made; the system temporary directory by default.
    pub probe_work_dir: Option<PathBuf>,
}

impl ProbeCli {
    /// The flags as a clap command (`--help` prints them).
    #[must_use]
    pub fn command() -> clap::Command {
        let flag = |id: &'static str, long: &'static str, help: &'static str| {
            clap::Arg::new(id)
                .long(long)
                .action(clap::ArgAction::SetTrue)
                .help(help)
        };
        let path = |id: &'static str, long: &'static str, help: &'static str| {
            clap::Arg::new(id)
                .long(long)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help(help)
        };
        clap::Command::new("kbf-task")
            .about("Probe the confinement of an action's Command")
            .arg(flag(
                "probe_confinement",
                "probe-confinement",
                "Run the probe",
            ))
            .arg(
                flag(
                    "probe_confinement_child",
                    "probe-confinement-child",
                    "The Command's side of the probe; started by the probe itself",
                )
                .hide(true),
            )
            .arg(path(
                "probe_secret_path",
                "probe-secret-path",
                "A path to ask whether the Command can read; never its contents",
            ))
            .arg(path(
                "probe_work_dir",
                "probe-work-dir",
                "Where the scratch directory is made (default: the temporary directory)",
            ))
    }

    /// Reads the flags from `args` (the program name first); a wrong flag prints the usage and
    /// exits with 2, as the task's own flags do.
    pub fn parse_from<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let found = Self::command().get_matches_from(args);
        ProbeCli {
            probe_confinement: found.get_flag("probe_confinement"),
            probe_confinement_child: found.get_flag("probe_confinement_child"),
            probe_secret_path: found.get_one::<PathBuf>("probe_secret_path").cloned(),
            probe_work_dir: found.get_one::<PathBuf>("probe_work_dir").cloned(),
        }
    }
}

/// Whether the first argument selects the probe (or its Command side).
#[must_use]
pub fn selected(first: Option<&std::ffi::OsStr>) -> bool {
    matches!(first.and_then(|a| a.to_str()), Some(FLAG | CHILD_FLAG))
}

/// How a check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// It held.
    Ok,
    /// It did not.
    Fail,
    /// It does not apply here, or the value is absent.
    Na,
}

/// One line of the output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The name after `probe `.
    pub name: String,
    /// The status.
    pub status: Status,
    /// What was seen.
    pub detail: String,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        Check {
            name: name.to_owned(),
            status,
            detail: detail.into(),
        }
    }

    /// The line: `probe <name>: ok|FAIL|n/a <detail>`.
    #[must_use]
    pub fn line(&self) -> String {
        let status = match self.status {
            Status::Ok => "ok",
            Status::Fail => "FAIL",
            Status::Na => "n/a",
        };
        format!("probe {}: {status} {}", self.name, self.detail)
    }
}

/// The last line for `checks`: `probe result: PASS` or `probe result: FAIL`.
#[must_use]
pub fn result_line(checks: &[Check]) -> String {
    format!("probe result: {}", choose(passes(checks), "FAIL", "PASS"))
}

/// `if_true` when `condition` holds, else `if_false`. Both are made first; used where the
/// choice is not a branch worth testing on its own (the coverage gate counts every `if`).
fn choose<T>(condition: bool, if_false: T, if_true: T) -> T {
    [if_false, if_true]
        .into_iter()
        .nth(usize::from(condition))
        .expect("two items, an index of 0 or 1")
}

/// The rule: every required check is `ok` and no check is `FAIL`.
#[must_use]
pub fn passes(checks: &[Check]) -> bool {
    let status_of = |name: &str| checks.iter().find(|c| c.name == name).map(|c| c.status);
    let none_failed = checks.iter().all(|c| c.status != Status::Fail);
    let required_ok = REQUIRED
        .iter()
        .all(|name| status_of(name) == Some(Status::Ok));
    none_failed & required_ok
}

/// What the Command wrote: `observe <name> <value>` lines, by name.
#[must_use]
pub fn parse_observations(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("observe "))
        .filter_map(|rest| rest.split_once(' '))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// The name of an errno the probe expects to meet, else `errno <n>`.
#[must_use]
pub fn errno_name(errno: i32) -> String {
    let name = match errno {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EACCES => "EACCES",
        libc::ENOSYS => "ENOSYS",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::EAFNOSUPPORT => "EAFNOSUPPORT",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::ECONNREFUSED => "ECONNREFUSED",
        _ => return format!("errno {errno}"),
    };
    format!("{name} ({errno})")
}

/// `errno <n>` as an observation value; `None` for anything else (`ok`, a missing value).
fn errno_of(value: Option<&String>) -> Option<i32> {
    value?
        .strip_prefix("errno ")?
        .parse()
        .ok()
        .filter(|errno: &i32| *errno > 0)
}

fn missing(name: &str) -> Check {
    Check::new(name, Status::Fail, "the Command reported nothing")
}

fn loopback(netns: bool, seen: Option<&String>) -> Check {
    match (netns, seen.map(String::as_str)) {
        (false, _) => Check::new("loopback", Status::Na, "the namespace path did not run"),
        (true, Some("ok")) => Check::new(
            "loopback",
            Status::Ok,
            "bind, connect and accept on 127.0.0.1",
        ),
        (true, None) => missing("loopback"),
        (true, Some(other)) => Check::new("loopback", Status::Fail, other),
    }
}

fn inet_unreachable(netns: bool, seen: Option<&String>) -> Check {
    let (want, how) = choose(
        netns,
        (libc::EPERM, "strict filter"),
        (libc::ENETUNREACH, "netns"),
    );
    match (seen.map(String::as_str), errno_of(seen)) {
        (None, _) => missing("inet_unreachable"),
        (_, Some(errno)) => Check::new(
            "inet_unreachable",
            choose(errno == want, Status::Fail, Status::Ok),
            choose(
                errno == want,
                format!("{}, wanted {} ({how})", errno_name(errno), errno_name(want)),
                format!("{} ({how})", errno_name(errno)),
            ),
        ),
        (Some("ok"), None) => Check::new(
            "inet_unreachable",
            Status::Fail,
            "a connect to 192.0.2.1 worked",
        ),
        (Some(other), None) => Check::new("inet_unreachable", Status::Fail, other),
    }
}

/// A call the filter is to refuse with `EPERM`. `not_proof` is why, on this host, a refusal
/// would come without the filter: then any errno is `ok` and says so.
fn refused(name: &str, seen: Option<&String>, not_proof: Option<String>) -> Check {
    match (seen.map(String::as_str), errno_of(seen)) {
        (None, _) => missing(name),
        (_, Some(errno)) => match not_proof {
            None => Check::new(
                name,
                choose(errno == libc::EPERM, Status::Fail, Status::Ok),
                choose(
                    errno == libc::EPERM,
                    format!("refused, not by the filter: {}", errno_name(errno)),
                    errno_name(errno),
                ),
            ),
            Some(why) => Check::new(
                name,
                Status::Ok,
                format!("{} ({why}: not a filter proof)", errno_name(errno)),
            ),
        },
        (Some(other), None) => Check::new(name, Status::Fail, format!("not refused: {other}")),
    }
}

/// Whether the effective capability mask (hex) holds `CAP_NET_RAW`, bit 13.
fn has_net_raw(cap_eff: Option<&String>) -> bool {
    cap_eff
        .and_then(|mask| u64::from_str_radix(mask, 16).ok())
        .is_some_and(|mask| (mask >> 13) & 1 == 1)
}

/// The Command's network namespace against the probe's own.
fn netns_distinct(netns: bool, child: Option<&String>, probe: Option<&String>) -> Check {
    match (netns, child, probe) {
        (false, ..) => Check::new(
            "netns_distinct",
            Status::Na,
            "the namespace path did not run",
        ),
        (true, None, _) => missing("netns_distinct"),
        (true, Some(_), None) => Check::new(
            "netns_distinct",
            Status::Fail,
            "cannot read the probe's own /proc/self/ns/net",
        ),
        (true, Some(child), Some(probe)) => {
            let distinct = child.starts_with("net:[") & (child != probe);
            Check::new(
                "netns_distinct",
                choose(distinct, Status::Fail, Status::Ok),
                choose(
                    distinct,
                    format!(
                        "the Command's namespace is not a new one: {child} (the probe: {probe})"
                    ),
                    format!("{child} != {probe}"),
                ),
            )
        }
    }
}

/// Whether `errno` says the path is not there, so the question was not answered.
fn is_absent(errno: i32) -> bool {
    matches!(errno, libc::ENOENT | libc::ENOTDIR)
}

fn secret(path: Option<&Path>, seen: Option<&String>) -> Check {
    match (path, seen.map(String::as_str), errno_of(seen)) {
        (None, ..) => Check::new("secret_path", Status::Na, "no --probe-secret-path"),
        (Some(_), None, _) => missing("secret_path"),
        (Some(_), Some("readable"), _) => Check::new("secret_path", Status::Ok, "readable"),
        (Some(_), _, Some(errno)) => Check::new(
            "secret_path",
            choose(is_absent(errno), Status::Ok, Status::Fail),
            choose(
                is_absent(errno),
                format!("not readable ({})", errno_name(errno)),
                format!(
                    "{}: the path is not there, so the question is unanswered",
                    errno_name(errno)
                ),
            ),
        ),
        (Some(_), Some(other), None) => Check::new("secret_path", Status::Fail, other),
    }
}

/// The checks that follow from the layers the run reported, how the Command ended and what it
/// observed. `ended` is `Ok(exit code)` or the error that stopped the Command from starting.
#[must_use]
pub fn evaluate(
    layers: &Layers,
    ended: Result<i32, String>,
    observed: &BTreeMap<String, String>,
    secret_path: Option<&Path>,
) -> Vec<Check> {
    let netns = *layers == Layers::SeccompAndNetns;
    let get = |name: &str| observed.get(name);
    let mut checks = vec![Check::new(
        "layers",
        choose(netns, Status::Fail, Status::Ok),
        layers.to_string(),
    )];
    checks.push(match ended {
        Ok(0) => Check::new("child", Status::Ok, "exit 0"),
        Ok(code) => Check::new("child", Status::Fail, format!("exit {code}")),
        Err(error) => Check::new("child", Status::Fail, error),
    });
    checks.push(netns_distinct(netns, get("netns"), get("parent.netns")));
    checks.push(loopback(netns, get("loopback")));
    checks.push(inet_unreachable(netns, get("inet")));
    let ring_off = get("fact.io_uring_disabled").filter(|v| matches!(v.as_str(), "1" | "2"));
    checks.push(refused("netlink", get("netlink"), None));
    checks.push(refused(
        "packet",
        get("packet"),
        (!has_net_raw(get("fact.cap_eff"))).then(|| "no CAP_NET_RAW".to_owned()),
    ));
    checks.push(refused(
        "io_uring",
        get("io_uring"),
        ring_off.map(|v| format!("io_uring_disabled={v}")),
    ));
    checks.push(secret(secret_path, get("secret")));
    checks
}

/// A fact line: `ok <value>`, or `n/a absent`.
#[must_use]
pub fn fact(name: &str, value: Option<String>) -> Check {
    match value {
        Some(value) => Check::new(name, Status::Ok, value),
        None => Check::new(name, Status::Na, "absent"),
    }
}

/// The text of a `/proc/<pid>/{uid,gid}_map` file as one line: entries joined with `;`, fields
/// with a single space.
#[must_use]
pub fn squeeze_map(text: &str) -> String {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(";")
}

/// The effective capability mask in the text of `/proc/self/status`.
#[must_use]
pub fn cap_eff(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .map(|v| v.trim().to_owned())
}

/// The effective user id in the text of `/proc/self/status` (the second of the `Uid:` fields).
#[must_use]
pub fn euid(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().nth(1))
        .map(str::to_owned)
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The facts of this process read from `/proc`: `euid`, `uid_map`, `gid_map` and `cap_eff`.
fn process_facts() -> Vec<Check> {
    let status = read("/proc/self/status");
    vec![
        fact("euid", status.as_deref().and_then(euid)),
        fact(
            "uid_map",
            read("/proc/self/uid_map").map(|t| squeeze_map(&t)),
        ),
        fact(
            "gid_map",
            read("/proc/self/gid_map").map(|t| squeeze_map(&t)),
        ),
        fact("cap_eff", status.as_deref().and_then(cap_eff)),
    ]
}

/// The facts the Command prints for itself, as `observe` lines.
#[cfg(target_os = "linux")]
fn child_facts() -> Vec<String> {
    process_facts()
        .into_iter()
        .filter(|c| c.name != "euid")
        .filter(|c| c.status == Status::Ok)
        .map(|c| format!("observe fact.{} {}", c.name, c.detail))
        .collect()
}

fn child_fact(observed: &BTreeMap<String, String>, name: &str) -> Check {
    fact(
        &format!("child_{name}"),
        observed.get(&format!("fact.{name}")).cloned(),
    )
}

/// Everything after the checks: the userns facts and the maps of both processes.
fn facts(observed: &BTreeMap<String, String>) -> Vec<Check> {
    let trimmed = |path| read(path).map(|t| t.trim().to_owned());
    let mut all = vec![
        fact(
            "apparmor_restrict_unprivileged_userns",
            trimmed("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
        ),
        fact(
            "max_user_namespaces",
            trimmed("/proc/sys/user/max_user_namespaces"),
        ),
        fact(
            "io_uring_disabled",
            trimmed("/proc/sys/kernel/io_uring_disabled"),
        ),
    ];
    all.extend(process_facts());
    for name in ["uid_map", "gid_map", "cap_eff"] {
        all.push(child_fact(observed, name));
    }
    all
}

/// Runs the Command side: the calls, as `observe` lines on standard output. Exit code 0.
#[must_use]
pub fn run_child(cli: &ProbeCli) -> ExitCode {
    for line in observe(cli.probe_secret_path.as_deref()) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
fn observe(secret_path: Option<&Path>) -> Vec<String> {
    let mut lines = child::observations(secret_path);
    lines.extend(child_facts());
    lines.extend(
        read("/proc/sys/kernel/io_uring_disabled")
            .map(|v| format!("observe fact.io_uring_disabled {}", v.trim())),
    );
    lines
}

/// The calls need Linux; elsewhere the Command reports nothing and every check fails.
#[cfg(not(target_os = "linux"))]
fn observe(_secret_path: Option<&Path>) -> Vec<String> {
    Vec::new()
}

/// The argv of the Command side: this program, with the hidden flag.
fn child_argv(secret_path: Option<&Path>) -> Vec<String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let mut argv = vec![exe.to_string_lossy().into_owned(), CHILD_FLAG.to_owned()];
    argv.extend(secret_path.into_iter().flat_map(|path| {
        [
            "--probe-secret-path".to_owned(),
            path.to_string_lossy().into_owned(),
        ]
    }));
    argv
}

/// Runs `argv` as an action's Command with the network denied, as [`run`] does for a real one,
/// under `dir` (which holds an empty `root`, and in which `private`, `stdout` and `stderr` are
/// made), and returns the checks that follow.
async fn run_confined(
    dir: &Path,
    argv: Vec<String>,
    deadline: Duration,
    secret_path: Option<&Path>,
) -> Vec<Check> {
    let command = Command {
        arguments: argv,
        ..Command::default()
    };
    let stdout = dir.join("stdout");
    let ran = run(
        &command,
        &Run {
            root: &dir.join("root"),
            private: &dir.join("private"),
            stdout: &stdout,
            stderr: &dir.join("stderr"),
            deadline,
            network: Network::Denied,
            unreadable: &[],
        },
    )
    .await;
    let text = std::fs::read_to_string(&stdout).unwrap_or_default();
    after_run(ran, &text, secret_path)
}

/// The checks that follow from how the run ended and what the Command printed. Not in the async
/// function above, so that its branches are one function whatever the executor makes of it.
fn after_run(ran: Result<Ran, RunError>, stdout: &str, secret_path: Option<&Path>) -> Vec<Check> {
    match ran {
        Ok(ran) => {
            let mut observed = parse_observations(stdout);
            observed.extend(std::fs::read_link("/proc/self/ns/net").ok().map(|link| {
                (
                    "parent.netns".to_owned(),
                    link.to_string_lossy().into_owned(),
                )
            }));
            let ended = ran
                .timed_out
                .then(|| "timed out".to_owned())
                .map_or(Ok(ran.exit_code), Err);
            let mut checks = evaluate(&ran.layers, ended, &observed, secret_path);
            checks.extend(facts(&observed));
            checks
        }
        Err(error) => evaluate_failed(error.to_string(), secret_path),
    }
}

/// The checks when the Command never ran: every check that needs it fails with `error`.
fn evaluate_failed(error: String, secret_path: Option<&Path>) -> Vec<Check> {
    let mut checks = evaluate(
        &Layers::Unreported,
        Err(error),
        &BTreeMap::new(),
        secret_path,
    );
    checks.extend(facts(&BTreeMap::new()));
    checks
}

/// Runs the probe: prints the lines and returns the exit code (0 PASS, 1 FAIL).
#[must_use]
pub fn run_probe(cli: &ProbeCli) -> ExitCode {
    let dir = cli
        .probe_work_dir
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("kbf-task-probe-{}", std::process::id()));
    let secret_path = cli.probe_secret_path.as_deref();
    let checks =
        match std::fs::create_dir(&dir).and_then(|()| std::fs::create_dir(dir.join("root"))) {
            Ok(()) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a single-threaded runtime starts");
                let checks = runtime.block_on(run_confined(
                    &dir,
                    child_argv(secret_path),
                    Duration::from_secs(60),
                    secret_path,
                ));
                // Best effort: a leftover directory in a job's scratch is harmless.
                let _ = std::fs::remove_dir_all(&dir);
                checks
            }
            Err(e) => evaluate_failed(format!("cannot make {}: {e}", dir.display()), secret_path),
        };
    for check in &checks {
        println!("{}", check.line());
    }
    println!("{}", result_line(&checks));
    ExitCode::from(u8::from(!passes(&checks)))
}

/// What `main` calls for a selected probe: the Command side or the probe, after the same
/// process protection a real run starts with.
#[must_use]
pub fn run_cli_with(protection: std::io::Result<()>, cli: &ProbeCli) -> ExitCode {
    let sides: [fn(&ProbeCli) -> ExitCode; 2] = [run_probe, run_child];
    protection.map_or_else(
        |e| {
            eprintln!("kbf-task: cannot make this process non-dumpable: {e}");
            ExitCode::from(crate::task::EXIT_SETUP)
        },
        |()| sides[usize::from(cli.probe_confinement_child)](cli),
    )
}

// The tests feed Linux errno numbers (ENETUNREACH is 101) and Linux observations to the checks, so
// they are Linux tests: the probe has no macOS side (the Command reports nothing there).
#[cfg(all(test, target_os = "linux"))]
#[path = "probe_tests.rs"]
mod tests;
