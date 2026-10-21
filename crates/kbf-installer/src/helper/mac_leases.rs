//! macOS, daemon-user mode: the lease check for a Mac whose daemon runs as its own
//! account (`_kbf`) with no lease-user pool. A uid count says nothing there (the daemon and
//! every action share a uid), so the installer looks at two things only root can see for
//! itself, never at the order record:
//!
//! - **O1, keepers.** Every action the daemon runs has a keeper process: the daemon's own
//!   binary under the name `kbf-keeper`, started `__keeper <mode>`. It starts before the
//!   action's go byte and ends after the action's processes. The installer asks `pgrep -f`
//!   for them (pids only, so the answer is small, however long the other processes'
//!   command lines are).
//! - **O2, lease directories.** `<data-dir>/leases/lease-*`, from the `data-dir` of the
//!   root-owned `kbf.toml` ([`mac_config`]). A lease's directory exists from before its
//!   keeper starts until after its outputs are uploaded, so it covers what O1 does not and
//!   outlives it. `lease-survey` and `lease-warm-up[-<n>]` are the daemon's own, not
//!   leases; every other `lease-` name counts, so a name this installer does not know reads
//!   busy, never idle.
//!
//! The answer is the larger of the two counts (one lease usually shows in both).
//!
//! Where the signals could be blind, the answer is `Err` ("cannot tell"):
//!
//! - **The daemon is not the one `kbf.toml` describes (H2).** The daemon reads its
//!   configuration once. The running daemon (the one process started by launchd whose
//!   command line has `--mode=daemon`) must have been started as exactly
//!   `kbf --mode=daemon --config <kbf.toml>` (a flag would override the file) and after the
//!   file last changed, or `data-dir` may not be the directory it uses.
//! - **No isolation (H5).** Without `/usr/bin/sandbox-exec` the daemon applies no sandbox,
//!   so an action could remove its own lease directory and kill its own keeper.
//!   [`sandbox_present`] is that check.
//! - **A process listing that may have lost lines.** `pgrep` exit 1 is "none"; any other
//!   non-zero exit, a signal, a timeout, output that is not pids or fills the capture
//!   buffer, or exit 0 with no pid is an error.
//! - Not root (`pgrep -f` shows another account's arguments to root only), a `kbf.toml`
//!   that is untrusted or not a native daemon's (see [`mac_config`]), a `leases/` that is
//!   missing, a link or unreadable.
//!
//! The processes are read first and the directories last (H4): a lease that starts while
//! the check runs either shows as a keeper or has made its directory by the time the
//! directories are read.
//!
//! One answer is let through on a judgment, [`stale_dirs_with_no_daemon`] (F1): lease
//! directories remain, no keeper exists and the daemon is positively not running. Only the
//! daemon's start-up sweep removes a leftover directory, so refusing here would keep out
//! the very release that restarts a daemon stuck in a crash loop. The answer is then 0
//! with a note naming each directory, which the helper logs.
//!
//! Assumptions only a Mac can confirm (the macOS `pgrep(1)` man page documents the first
//! two; the third is inferred): `pgrep -f` matches an extended regular expression against
//! the whole argument list, exits 0 for a match, 1 for none, 2 or 3 for an error, and never
//! matches itself; the argument list starts with the process's own `argv[0]` (`kbf-keeper`)
//! or with its path, and the patterns accept both; `ps -ww -o etime=` prints
//! `[[dd-]hh:]mm:ss`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use kbf_host::exec::{Exec, HostExec};

use crate::helper::leases::{LeaseSource, PS};
use crate::helper::mac_config::{self, DaemonConfig};
use crate::layout::{self, Layout};
use crate::run::{CAPTURE, Ran, Runner, Spawn, SystemRunner};

/// The program that lists the pids of the processes whose argument list matches.
pub const PGREP: &str = "/usr/bin/pgrep";

/// The sandbox the daemon runs its actions in; without it the daemon keeps no isolation.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A keeper: `kbf-keeper __keeper ...` (or the daemon's binary under its own name or path).
/// An over-match only raises the count.
pub const KEEPER_PATTERN: &str = "(^|[ /])(kbf-keeper|kbf) __keeper( |$)";

/// A `kbf` daemon: `kbf ... --mode=daemon` (or `--mode daemon`). A wide pattern, so a
/// daemon started in an unexpected way is found and then refused, never missed.
pub const DAEMON_PATTERN: &str = "(^|[ /])kbf .*--mode[= ]daemon";

/// The daemon must have started at least this many seconds after `kbf.toml` last changed:
/// the elapsed time `ps` prints is rounded down, and so is the file's time.
const CLOCK_SLACK_SECS: i64 = 2;

/// Whether the daemon is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Daemon {
    /// No process looks like a `kbf` daemon.
    NotRunning,
    /// One does, started by launchd as `kbf.toml` says and after its last change.
    Running,
}

/// The lease source of a Mac whose daemon runs as its own account.
pub struct MacLeases {
    layout: Layout,
    trust: Box<dyn Exec>,
    runner: Box<dyn Runner>,
    /// Seconds since the epoch.
    now: Box<dyn Fn() -> i64>,
    timeout: Duration,
}

impl MacLeases {
    /// The source of the host `layout` describes: the real executor and runner and the
    /// clock.
    pub fn on_host(layout: Layout) -> MacLeases {
        let trust = Box::new(HostExec::new(layout.root().clone()));
        MacLeases::new(
            layout,
            trust,
            Box::new(SystemRunner),
            Box::new(unix_now),
            Duration::from_secs(5),
        )
    }

    /// A source over the parts given (a test plays the host).
    pub fn new(
        layout: Layout,
        trust: Box<dyn Exec>,
        runner: Box<dyn Runner>,
        now: Box<dyn Fn() -> i64>,
        timeout: Duration,
    ) -> MacLeases {
        MacLeases {
            layout,
            trust,
            runner,
            now,
            timeout,
        }
    }

    /// Run `program` and return its exit code and its text. A program that could not
    /// start, ran out of time, was ended by a signal, filled the capture buffer (so
    /// lines may be missing) or printed what is not text is an error.
    fn run(&self, program: &str, args: &[&str]) -> Result<(i32, String), String> {
        let spawn = Spawn {
            program: PathBuf::from(program),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
            cwd: PathBuf::from("/"),
            run_as: None,
            timeout: self.timeout,
        };
        let ran = self
            .runner
            .run(&spawn)
            .map_err(|e| format!("{program}: {e}"))?;
        text_of(program, ran)
    }

    /// The pids whose argument list matches `pattern`, sorted: exit 1 is none, and any other
    /// outcome that is not a list of pids is an error.
    fn pgrep(&self, pattern: &str) -> Result<Vec<u32>, String> {
        let (code, text) = self.run(PGREP, &["-f", pattern])?;
        let mut pids = Vec::new();
        for line in text.lines() {
            let pid = line
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{PGREP}: {line:?} is not a pid"))?;
            pids.push(pid);
        }
        pids.sort_unstable();
        pids.dedup();
        match (code, pids.is_empty()) {
            (0, false) | (1, true) => Ok(pids),
            (0, true) => Err(format!("{PGREP} matched but listed no pid")),
            (1, false) => Err(format!("{PGREP} listed pids but said none matched")),
            (code, _) => Err(format!("{PGREP} failed (status {code})")),
        }
    }

    /// O1: how many keepers run.
    fn keepers(&self) -> Result<usize, String> {
        Ok(self.pgrep(KEEPER_PATTERN)?.len())
    }

    /// Whether the daemon runs, and when it does, that it is the one `config` describes.
    fn daemon(&self, config: &DaemonConfig) -> Result<Daemon, String> {
        let pids = self.pgrep(DAEMON_PATTERN)?;
        if pids.is_empty() {
            return Ok(Daemon::NotRunning);
        }
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let (code, text) = self.run(
            PS,
            &[
                "-ww", "-p", &list, "-o", "pid=", "-o", "ppid=", "-o", "etime=", "-o", "command=",
            ],
        )?;
        if code != 0 {
            return Err(format!("{PS} failed (status {code})"));
        }
        let rows = rows_of(&text, &pids)?;
        let launched: Vec<&Row> = rows.iter().filter(|r| r.ppid == 1).collect();
        let [daemon] = launched.as_slice() else {
            return Err(format!(
                "{} processes started by launchd look like a kbf daemon (of pids {pids:?}); expected one",
                launched.len()
            ));
        };
        let file = self.layout.kbf_toml();
        let want = ["--mode=daemon", "--config", file.as_str()];
        let argv: Vec<&str> = daemon.command.split_whitespace().collect();
        let named_kbf = argv
            .first()
            .is_some_and(|a| a.rsplit('/').next() == Some("kbf"));
        if !named_kbf || argv.get(1..) != Some(want.as_slice()) {
            return Err(format!(
                "the daemon (pid {}) was started as {:?}, not as `kbf {}`: its settings may not be {file}'s",
                daemon.pid,
                daemon.command,
                want.join(" ")
            ));
        }
        let started = (self.now)() - daemon.elapsed;
        if started < config.changed_at + CLOCK_SLACK_SECS {
            return Err(format!(
                "{file} changed after the daemon (pid {}) started: its data-dir may not be {}",
                daemon.pid, config.data_dir
            ));
        }
        Ok(Daemon::Running)
    }

    /// O2: the names of the lease directories, sorted. The entries are named, never
    /// examined, so a lease that ends while the directory is listed is not a failure.
    fn lease_dirs(&self, config: &DaemonConfig) -> Result<Vec<OsString>, String> {
        let shown = leases_dir(config);
        let dir = self.layout.join(&config.data_dir).join("leases");
        let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{shown}: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{shown} is a link"));
        }
        if !meta.is_dir() {
            return Err(format!("{shown} is not a directory"));
        }
        let listing = std::fs::read_dir(&dir).map_err(|e| format!("{shown}: {e}"))?;
        names_of(listing.map(|entry| entry.map(|e| e.file_name())), &shown)
    }
}

impl LeaseSource for MacLeases {
    fn live(&self) -> Result<usize, String> {
        self.live_noted().map(|(live, _)| live)
    }

    fn live_noted(&self) -> Result<(usize, Option<String>), String> {
        sandbox_present(&self.layout)?;
        if self.trust.euid() != 0 {
            return Err(format!(
                "not root: only root sees every process's arguments ({PGREP} -f)"
            ));
        }
        let config = mac_config::read(&*self.trust, &self.layout)?;
        // The processes first, the directories last (H4).
        let keepers = self.keepers()?;
        let daemon = self.daemon(&config)?;
        let dirs = self.lease_dirs(&config)?;
        if stale_dirs_with_no_daemon(keepers, &dirs, daemon) {
            let shown = leases_dir(&config);
            let named: Vec<String> = dirs
                .iter()
                .map(|d| format!("{shown}/{}", d.to_string_lossy()))
                .collect();
            return Ok((
                0,
                Some(format!(
                    "{} remains and no kbf daemon is running; the daemon's start-up sweep removes it",
                    named.join(", ")
                )),
            ));
        }
        Ok((keepers.max(dirs.len()), None))
    }
}

/// F1 (the lead's decision is pending; the default is to allow): lease directories remain,
/// no keeper exists and the daemon is positively not running. Returning `false` here
/// instead makes a leftover directory block an activation until a person restarts the
/// daemon by hand.
fn stale_dirs_with_no_daemon(keepers: usize, dirs: &[OsString], daemon: Daemon) -> bool {
    keepers == 0 && !dirs.is_empty() && daemon == Daemon::NotRunning
}

/// H5 (the lead's decision is pending; the default is to refuse): `Ok` when the sandbox
/// the daemon runs actions in is there, as it tests for it (a file, links followed).
fn sandbox_present(layout: &Layout) -> Result<(), String> {
    let shown = layout::path(SANDBOX_EXEC);
    match std::fs::metadata(layout.join(&shown)) {
        Ok(meta) if meta.is_file() => Ok(()),
        _ => Err(format!(
            "{shown} is not a file: the daemon then applies no sandbox, and an action could remove its own lease directory"
        )),
    }
}

/// The sorted names of the leases among the entries of a listing of `shown`. A name that
/// vanished from the listing is a lease that ended, not a failure.
fn names_of(
    listing: impl Iterator<Item = std::io::Result<OsString>>,
    shown: &str,
) -> Result<Vec<OsString>, String> {
    let mut names = Vec::new();
    for entry in listing {
        match entry {
            Ok(name) if is_lease_name(&name) => names.push(name),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{shown}: {e}")),
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// Where the lease directories are, as the host names it.
fn leases_dir(config: &DaemonConfig) -> String {
    format!("{}/leases", config.data_dir.as_str().trim_end_matches('/'))
}

/// Whether the directory entry `name` is a lease: every `lease-` name but the daemon's own
/// warm-up (`lease-warm-up`, `lease-warm-up-<digits>`) and survey (`lease-survey`).
fn is_lease_name(name: &OsStr) -> bool {
    let name = name.as_bytes();
    if !name.starts_with(b"lease-") {
        return false;
    }
    let numbered = name
        .strip_prefix(b"lease-warm-up-")
        .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit));
    !(name == b"lease-survey" || name == b"lease-warm-up" || numbered)
}

/// What a finished program printed, with its exit code; why not when `ran` cannot be
/// trusted to have printed it all.
fn text_of(program: &str, ran: Ran) -> Result<(i32, String), String> {
    if ran.timed_out {
        return Err(format!("{program} ran out of time"));
    }
    let Some(code) = ran.code else {
        return Err(format!("{program} was ended by a signal"));
    };
    if ran.stdout.len() as u64 >= CAPTURE {
        return Err(format!(
            "{program} printed {CAPTURE} bytes or more: the rest is cut off"
        ));
    }
    let text = String::from_utf8(ran.stdout).map_err(|_| format!("{program}: not UTF-8"))?;
    Ok((code, text))
}

/// One process of the daemon's `ps` listing.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    pid: u32,
    ppid: u32,
    /// Seconds since it started, rounded down.
    elapsed: i64,
    command: String,
}

/// The rows of `text` (`pid ppid etime command...` per line) for each of `pids`. A line
/// that does not start with one of the pids is not a row (a command with a line break
/// continues on the next line) and is ignored; a pid with no row, or two, is an error.
fn rows_of(text: &str, pids: &[u32]) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = Vec::new();
    for line in text.lines() {
        let Some((first, rest)) = token(line) else {
            continue;
        };
        let Some(pid) = first.parse::<u32>().ok().filter(|p| pids.contains(p)) else {
            continue;
        };
        let parsed = token(rest).and_then(|(ppid, rest)| {
            let (etime, command) = token(rest)?;
            Some((
                ppid.parse::<u32>().ok()?,
                etime_secs(etime)?,
                command.trim(),
            ))
        });
        let Some((ppid, elapsed, command)) = parsed.filter(|(_, _, c)| !c.is_empty()) else {
            return Err(format!("{PS}: cannot read the line of pid {pid}: {line:?}"));
        };
        if rows.iter().any(|r| r.pid == pid) {
            return Err(format!("{PS} listed pid {pid} twice"));
        }
        rows.push(Row {
            pid,
            ppid,
            elapsed,
            command: command.to_owned(),
        });
    }
    match pids.iter().find(|p| rows.iter().all(|r| r.pid != **p)) {
        Some(pid) => Err(format!(
            "{PS} did not list pid {pid}: it ended, or the list is cut"
        )),
        None => Ok(rows),
    }
}

/// The first run of non-blank characters of `text`, and what follows it.
fn token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    Some(text.split_at(text.find(char::is_whitespace).unwrap_or(text.len())))
}

/// `[[dd-]hh:]mm:ss` as seconds.
fn etime_secs(text: &str) -> Option<i64> {
    let number = |s: &str| {
        (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<i64>().ok())
            .flatten()
    };
    let (days, clock) = match text.split_once('-') {
        Some((d, rest)) => (number(d)?, rest),
        None => (0, text),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0, number(m)?, number(s)?),
        [h, m, s] => (number(h)?, number(m)?, number(s)?),
        _ => return None,
    };
    Some(((days * 24 + hours) * 60 + minutes) * 60 + seconds)
}

/// The clock, in seconds since the epoch.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "mac_leases_tests.rs"]
pub(crate) mod tests;
