//! The programs `tests/executor_real_service_integration.rs` gives the pinned `js-agent` in
//! place of the host's: `crun`, `ip`, `nft` and `nsenter`, one binary (`kbf-js-stand-in`)
//! linked under each name and answering as the one its argv0 names.
//!
//! Each link sits in a directory beside a directory `stand-in/` ([`Place::of`]), which holds
//! what the test tells the programs and what they tell the test:
//!
//! - `calls`: every run appends a line, its name and its arguments joined by spaces;
//! - `plan/<hash>`: what the container of the action whose digest has that hash does, one
//!   line: `exit <n>` (it exits `n`) or `hold` (it runs until it is signalled, or until the
//!   test hangs up); without the file it exits 0. A job that names no action (the cutover's
//!   canary, the task tool with `--version` alone) follows `plan/no-action` ([`NO_ACTION`]);
//! - `events.sock`: the test's listener, or a link to it (a socket's path is short, and the
//!   link's target is what is dialled). The container of each run connects, sends one line,
//!   `start <container name>` and the job's arguments, each after a tab, and keeps the
//!   connection open while it runs, so the test sees it start, sees what it was given, and
//!   sees it end (the connection closes), with no clock;
//! - `netns-pid`: the process `crun state` names as the container's first process. The test
//!   keeps a process of its own in a network namespace of its own there: the agent wires a
//!   job's network into that namespace, through the stand-in `ip` and `nsenter`, which change
//!   nothing;
//! - `nft-loaded`: what the agent loaded with `nft -f -`, each load appended.
//!
//! `crun` answers `--version` (a release the agent accepts), `run` (a `helper-*` bundle exits
//! 0 at once; a job's bundle makes the container's cgroup the agent looks for, joins it,
//! records its process id under `--root`, reports to the test and does what the plan says),
//! `kill` (signals the process `run` recorded), `delete` and `state`. `ip -j link show` lists
//! no link, `nft -j list table` an empty table, and every other call of the three succeeds.
//! Nothing here reads a clock.

use std::io::{Read, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

/// The plan a job that names no `--action` follows: `plan/no-action`.
pub const NO_ACTION: &str = "no-action";

/// What `nft -j list table <family> <table>` prints: the listing of a table with nothing in
/// it, which the agent reads back the same way every time.
pub const EMPTY_TABLE: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9","release_name":"stand-in","json_schema_version":1}}]}"#;

/// The release `crun --version` names: one the agent accepts (1.14 or later).
pub const VERSION: &str = "crun version 1.14.4\ncommit: stand-in\nspec: 1.0.0\n";

/// Where the programs read and write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// The `stand-in/` directory.
    pub dir: PathBuf,
    /// The cgroup version 2 mount the paths of a bundle's `linux.cgroupsPath` are under.
    pub cgroups: PathBuf,
}

impl Place {
    /// The place of the program at `exe`: `stand-in/` beside its directory, and the
    /// machine's cgroup mount.
    #[must_use]
    pub fn of(exe: &Path) -> Place {
        let dir = exe.parent().unwrap_or(Path::new("/")).join("stand-in");
        Place {
            dir,
            cgroups: PathBuf::from("/sys/fs/cgroup"),
        }
    }
}

/// What a run printed and how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
}

impl Answer {
    fn ok(stdout: impl Into<String>) -> Answer {
        Answer {
            stdout: stdout.into(),
            ..Answer::default()
        }
    }

    fn failed(why: impl Into<String>) -> Answer {
        Answer {
            stderr: why.into(),
            code: 1,
            ..Answer::default()
        }
    }
}

/// The program, run as `argv0` with `args`, `stdin` and `place`.
pub fn answer(argv0: &str, args: &[String], stdin: &mut dyn Read, place: &Place) -> Answer {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let line = format!("{name} {}\n", args.join(" "));
    if let Err(why) = append(&place.dir.join("calls"), &line) {
        return Answer::failed(format!("{name}: recording the call: {why}"));
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match name {
        "crun" => crun(args, place),
        "ip" => match words[..] {
            ["-j", "link", "show"] => Answer::ok("[]"),
            _ => drain(stdin),
        },
        "nft" => match words[..] {
            ["-f", "-"] => load(stdin, place),
            ["-j", "list", ..] => Answer::ok(EMPTY_TABLE),
            _ => Answer::default(),
        },
        "nsenter" => drain(stdin),
        other => Answer::failed(format!("kbf-js-stand-in: no program {other:?}")),
    }
}

/// Runs as the program argv0 names, over the process's own arguments and standard input.
pub fn main() -> ExitCode {
    let mut all = std::env::args();
    let argv0 = all.next().unwrap_or_default();
    let args: Vec<String> = all.collect();
    let exe = std::env::current_exe().unwrap_or_default();
    let said = answer(&argv0, &args, &mut std::io::stdin(), &Place::of(&exe));
    print!("{}", said.stdout);
    eprint!("{}", said.stderr);
    ExitCode::from(said.code)
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Reads standard input to its end (`ip -batch -` and `nsenter ... ip -batch -` are given
/// their commands there) and succeeds.
fn drain(stdin: &mut dyn Read) -> Answer {
    let _ = std::io::copy(stdin, &mut std::io::sink());
    Answer::default()
}

/// `nft -f -`: what was loaded is appended to `nft-loaded`.
fn load(stdin: &mut dyn Read, place: &Place) -> Answer {
    let mut text = String::new();
    let _ = stdin.read_to_string(&mut text);
    match append(&place.dir.join("nft-loaded"), &text) {
        Ok(()) => Answer::default(),
        Err(why) => Answer::failed(format!("nft: {why}")),
    }
}

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// `crun`'s global options (`--root R`, `--log L`, `--log-format F`, `--debug`) and the
/// command after them, with its own arguments.
fn command(args: &[String]) -> (Option<&str>, &[String]) {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        match arg.as_str() {
            "--root" | "--log" | "--log-format" => at += 2,
            // `--version` is an option that is the whole call.
            "--version" => break,
            flag if flag.starts_with('-') => at += 1,
            _ => break,
        }
    }
    let rest = args.get(at..).unwrap_or_default();
    (
        rest.first().map(String::as_str),
        rest.get(1..).unwrap_or_default(),
    )
}

fn crun(args: &[String], place: &Place) -> Answer {
    let root = after(args, "--root").map(PathBuf::from);
    match (command(args), root) {
        ((Some("--version"), _), _) => Answer::ok(VERSION),
        ((Some("run"), rest), Some(root)) => run(rest, &root, place),
        ((Some("kill"), rest), Some(root)) => kill(rest, &root),
        ((Some("delete"), _), Some(_)) => Answer::default(),
        ((Some("state"), [.., name]), Some(_)) => state(name, place),
        _ => Answer::failed(format!("crun: unknown command: {}", args.join(" "))),
    }
}

/// `crun state <name>`: running, with the process the test keeps in a network namespace of
/// its own as the first process.
fn state(name: &str, place: &Place) -> Answer {
    let pid = std::fs::read_to_string(place.dir.join("netns-pid")).unwrap_or_default();
    match pid.trim().parse::<u32>() {
        Ok(pid) => Answer::ok(
            serde_json::json!({
                "ociVersion": "1.0.2",
                "id": name,
                "pid": pid,
                "status": "running",
            })
            .to_string(),
        ),
        Err(why) => Answer::failed(format!("crun state: netns-pid: {why}")),
    }
}

/// `crun kill <name> <signal>`: the signal goes to the process `run` recorded.
fn kill(rest: &[String], root: &Path) -> Answer {
    let pid = std::fs::read_to_string(root.join("pid")).unwrap_or_default();
    let pid = pid
        .trim()
        .parse()
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let signal = rest.get(1).and_then(|n| n.parse().ok());
    let signal = signal.and_then(rustix::process::Signal::from_named_raw);
    match (pid, signal) {
        (Some(pid), Some(signal)) => match rustix::process::kill_process(pid, signal) {
            Ok(()) => Answer::default(),
            Err(why) => Answer::failed(format!("crun kill: {why}")),
        },
        _ => Answer::failed(format!(
            "crun kill: no container to signal: {}",
            rest.join(" ")
        )),
    }
}

/// What a bundle's `config.json` says the container runs and where its cgroup is.
fn bundle(path: &Path) -> Result<(Vec<String>, String), String> {
    let text = std::fs::read_to_string(path.join("config.json"))
        .map_err(|why| format!("{}/config.json: {why}", path.display()))?;
    let config: Value = serde_json::from_str(&text).map_err(|why| why.to_string())?;
    let args = config["process"]["args"]
        .as_array()
        .map(|args| args.iter().filter_map(Value::as_str).map(str::to_owned))
        .ok_or("config.json names no process.args")?
        .collect();
    let cgroup = config["linux"]["cgroupsPath"]
        .as_str()
        .ok_or("config.json names no linux.cgroupsPath")?
        .to_owned();
    Ok((args, cgroup))
}

/// `crun run --bundle <bundle> <name>`.
fn run(rest: &[String], root: &Path, place: &Place) -> Answer {
    let (Some(bundle_dir), Some(name)) = (after(rest, "--bundle"), rest.last()) else {
        return Answer::failed("crun run: needs --bundle and a name");
    };
    let bundle_dir = Path::new(bundle_dir);
    let helper = bundle_dir
        .file_name()
        .is_some_and(|dir| dir.to_string_lossy().starts_with("helper-"));
    if helper {
        return Answer::default();
    }
    match container(bundle_dir, name, root, place) {
        Ok(code) => Answer {
            code,
            ..Answer::default()
        },
        Err(why) => Answer {
            stderr: format!("crun run {name}: {why}"),
            code: 125,
            ..Answer::default()
        },
    }
}

/// The container of a job: its cgroup made and joined, its process id recorded, the test
/// told, and then the plan of its action carried out. The exit code is the plan's.
fn container(bundle_dir: &Path, name: &str, root: &Path, place: &Place) -> Result<u8, String> {
    let (args, cgroup) = bundle(bundle_dir)?;
    let cgroup = place.cgroups.join(cgroup.trim_start_matches('/'));
    std::fs::create_dir_all(&cgroup).map_err(|why| format!("{}: {why}", cgroup.display()))?;
    let me = std::process::id().to_string();
    std::fs::write(cgroup.join("cgroup.procs"), &me)
        .map_err(|why| format!("joining {}: {why}", cgroup.display()))?;
    std::fs::create_dir_all(root).map_err(|why| format!("{}: {why}", root.display()))?;
    std::fs::write(root.join("pid"), &me).map_err(|why| format!("recording the pid: {why}"))?;
    let hash = after(&args, "--action").map_or(NO_ACTION, |action| {
        action.split('/').next().unwrap_or_default()
    });
    let plan = std::fs::read_to_string(place.dir.join("plan").join(hash)).unwrap_or_default();
    let socket = place.dir.join("events.sock");
    let socket = std::fs::read_link(&socket).unwrap_or(socket);
    let line = format!("start\t{name}\t{}\n", args.join("\t"));
    let mut test = UnixStream::connect(socket)
        .and_then(|mut test| test.write_all(line.as_bytes()).map(|()| test))
        .map_err(|why| format!("events.sock: {why}"))?;
    match plan.split_whitespace().collect::<Vec<_>>()[..] {
        [] => Ok(0),
        ["exit", code] => code.parse().map_err(|_| format!("plan: exit {code:?}")),
        ["hold"] => {
            // Until the test hangs up; a signal ends the process before that.
            let _ = std::io::copy(&mut test, &mut std::io::sink());
            Ok(0)
        }
        _ => Err(format!("plan: {plan:?}")),
    }
}

#[cfg(test)]
mod tests;
