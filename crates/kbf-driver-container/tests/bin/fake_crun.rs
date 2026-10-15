//! `kbf-fake-crun`: the stand-in `crun` of the container driver's OCI backend tests.
//! Each test links `<dir>/crun` to it (`tests/support/fake_oci.rs`); the fake keeps its
//! state and the test's knobs in `<dir>/crun-state` and uses `<dir>/cgroup` as the
//! cgroup mount. It reads the bundle's `config.json` as `crun` would and runs the
//! action as a plain process, so the OCI backend's steps can be tested on any machine.
//! Real `crun` behaviour is tested in `tests/oci_crun.rs`.
//!
//! It answers three verbs, after the global options `--root`, `--log`, `--log-format`:
//! - `run --bundle <dir> <id>`: an action (the config names a `linux.cgroupsPath`) or a
//!   helper container (it names none). The exit status is the action's (`128 + n` for
//!   a signal), as `crun run`'s is. A helper gives directories to an owner, removes the
//!   contents of one, or unpacks an image; its program is recognised by `process.args[0]`
//!   as the driver writes it and its work performed on the host directories its
//!   `/t/<n>` binds name.
//! - `kill <id> <signal>`: signals the last action.
//! - `delete --force <id>`: kills the last action and removes the container's state.
//!
//! Knobs (files in `crun-state/` the test writes):
//! - `action`: the act steps the action runs, one argument per line, as in
//!   `kbf-fake-podman` (`$ROOT`, `$UPPER`, `$CG` expanded; `kbf-act` beside the link).
//! - `nonce`: this Fake's marker; the action inherits `FAKE_LEASE=<nonce><cgroup>`.
//! - `crun-error`: the run fails before the action starts, with this text as an error
//!   record in the `--log` file and `crun-error-code` (default 1) as its status.
//! - `busy-once`: the next run fails as crun does when the program's exec finds it busy
//!   (ETXTBSY), once.
//! - `kill-fails`, `delete-fails`: that verb fails.
//! - `chown-fails`: a helper giving to the owner this knob holds (`1:1` or `0:0`) fails.
//! - `rm-noop`, `rm-fails`: a removing helper does nothing / fails.
//! - `tar-fails`: the unpacking helper's `tar` fails.
//! - `vanish-after-give`, `unexec-after-give`: the first helper that gives directories
//!   removes the `crun` program it was run as / makes it not executable (a test links a
//!   private copy, not the shared program).
//! - `leave-state`: the run leaves its state under `--root`, as a killed `crun` does.
//! - `exit-override`: the run exits with this status whatever the action did.
//!
//! Records: `calls` (one verb per line; `helper give`, `helper rm`, `helper mkdir`, `helper tar`, `helper touch`),
//! `config.json` (the last action's bundle config), `run.args`, `events` (`cgroup.kill
//! ended the action`, `run ended`, `delete`, `chown <owner> <paths>`, `chown-dirs
//! <owner> <dir>`), `owner` (the last owner given), `upper`, `root`, `cgroup`, `pid`,
//! `start-pid`, `run-not-reaped-at-delete`.
//!
//! The fake changes no owner (the test runs as one user), but models what an owner other
//! than the daemon's means: when a run ends while the overlay is the container's (`1:1`
//! last), it makes the upper directory unreadable (mode 000, the old mode kept in
//! `upper-mode`); a helper giving `0:0` restores it.

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use serde_json::Value;

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let link = PathBuf::from(args.next().unwrap_or_default());
    let args: Vec<Vec<u8>> = args.map(OsString::into_vec).collect();
    let here = link.parent().unwrap_or(Path::new(".")).to_owned();
    let fake = Fake {
        state: here.join("crun-state"),
        cgroot: here.join("cgroup"),
        link,
    };
    ExitCode::from(fake.run(&args))
}

struct Fake {
    link: PathBuf,
    state: PathBuf,
    cgroot: PathBuf,
}

fn refuse(message: &str, code: u8) -> u8 {
    eprintln!("{message}");
    code
}

fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn concat(a: &Path, b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&[a.as_os_str().as_bytes(), b].concat()))
}

fn lines(args: &[Vec<u8>]) -> Vec<u8> {
    args.iter()
        .flat_map(|a| [a.as_slice(), b"\n"].concat())
        .collect()
}

fn arguments(knob: &[u8]) -> impl Iterator<Item = &[u8]> {
    knob.split_inclusive(|b| *b == b'\n')
        .map(|arg| arg.strip_suffix(b"\n").unwrap_or(arg))
}

fn expand(arg: &[u8], paths: &[(&str, Vec<u8>)]) -> Vec<u8> {
    for (name, path) in paths {
        if let Some(rest) = arg
            .strip_prefix(b"$")
            .and_then(|a| a.strip_prefix(name.as_bytes()))
            && (rest.is_empty() || rest.starts_with(b"/"))
        {
            return [path.as_slice(), rest].concat();
        }
    }
    arg.to_vec()
}

/// The host path of `inside`, a path below one of the `/t/<n>` binds.
fn host_path(binds: &[(String, PathBuf)], inside: &str) -> Vec<u8> {
    for (dest, source) in binds {
        if let Some(rest) = inside.strip_prefix(dest.as_str())
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return [source.as_os_str().as_bytes(), rest.as_bytes()].concat();
        }
    }
    inside.as_bytes().to_vec()
}

/// The global options, then the verb and its arguments.
struct Call<'a> {
    root: Option<PathBuf>,
    log: Option<PathBuf>,
    verb: &'a [u8],
    rest: &'a [Vec<u8>],
}

fn parse(args: &[Vec<u8>]) -> Option<Call<'_>> {
    let (mut root, mut log) = (None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_slice() {
            b"--root" => {
                root = args.get(i + 1).map(|a| path(a).to_owned());
                i += 2;
            }
            b"--log-format" => i += 2,
            b"--log" => {
                log = args.get(i + 1).map(|a| path(a).to_owned());
                i += 2;
            }
            b"--debug" | b"--systemd-cgroup" => i += 1,
            _ => break,
        }
    }
    let verb = args.get(i)?;
    Some(Call {
        root,
        log,
        verb,
        rest: &args[i + 1..],
    })
}

impl Fake {
    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    fn has(&self, name: &str) -> bool {
        self.file(name).is_file()
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.file(name)).ok()
    }

    fn cat(&self, name: &str) -> Vec<u8> {
        let mut bytes = self.read(name).unwrap_or_default();
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        bytes
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        let _ = std::fs::write(self.file(name), bytes);
    }

    fn write_line(&self, name: &str, line: &[u8]) {
        self.write(name, &[line, b"\n"].concat());
    }

    fn append(&self, name: &str, line: &[u8]) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name))
        {
            let _ = file.write_all(&[line, b"\n"].concat());
        }
    }

    fn touch(&self, name: &str) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name));
    }

    fn lease_cgroup(&self) -> PathBuf {
        concat(&self.cgroot, &self.cat("cgroup"))
    }

    fn cgroup_killed(&self) -> bool {
        self.has("cgroup") && self.lease_cgroup().join("cgroup.kill").exists()
    }

    fn run(&self, args: &[Vec<u8>]) -> u8 {
        let _ = std::fs::create_dir_all(&self.state);
        let Some(call) = parse(args) else {
            return refuse("fake crun: no verb", 1);
        };
        match call.verb {
            b"run" => self.run_verb(&call),
            b"kill" => self.kill(call.rest),
            b"delete" => self.delete(&call),
            other => refuse(
                &format!(
                    "fake crun: unexpected verb {}",
                    String::from_utf8_lossy(other)
                ),
                1,
            ),
        }
    }

    /// An error record in the `--log` file, as `crun --log-format json` writes one.
    fn log_error(&self, call: &Call<'_>, msg: &str) {
        let Some(log) = &call.log else {
            eprintln!("crun: {msg}");
            return;
        };
        let record = serde_json::json!({"level": "error", "msg": msg, "time": "now"});
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(file, "{record}");
        }
    }

    fn run_verb(&self, call: &Call<'_>) -> u8 {
        self.write("run.args", &lines(call.rest));
        let bundle = call
            .rest
            .windows(2)
            .find(|w| w[0] == b"--bundle")
            .map(|w| path(&w[1]).to_owned());
        let Some(bundle) = bundle else {
            return refuse("fake crun: run: no --bundle", 1);
        };
        let Ok(text) = std::fs::read_to_string(bundle.join("config.json")) else {
            return refuse("fake crun: run: no config.json in the bundle", 1);
        };
        let Ok(config) = serde_json::from_str::<Value>(&text) else {
            return refuse("fake crun: run: config.json is not JSON", 1);
        };
        let id = call.rest.last().cloned().unwrap_or_default();
        if config["linux"]["cgroupsPath"].is_string() {
            self.append("calls", b"run");
            self.write("config.json", text.as_bytes());
            self.action(call, &config, &id)
        } else {
            self.helper(&config)
        }
    }

    fn action(&self, call: &Call<'_>, config: &Value, id: &[u8]) -> u8 {
        self.write_line("start-pid", std::process::id().to_string().as_bytes());
        self.write_line("id", id);
        // The container's cgroup is `<lease cgroup>/ctr`.
        let cgroup = config["linux"]["cgroupsPath"].as_str().unwrap_or_default();
        let lease_cgroup = cgroup.strip_suffix("/ctr").unwrap_or(cgroup);
        self.write_line("cgroup", lease_cgroup.as_bytes());
        for mount in config["mounts"].as_array().into_iter().flatten() {
            if mount["type"] == "overlay" {
                let options = mount["options"][0].as_str().unwrap_or_default();
                for part in options.split(',') {
                    if let Some(dir) = part.strip_prefix("lowerdir=") {
                        self.write_line("root", dir.as_bytes());
                    } else if let Some(dir) = part.strip_prefix("upperdir=") {
                        self.write_line("upper", dir.as_bytes());
                    }
                }
            }
        }
        // The state `crun run` keeps while the container exists, under `--root`.
        let state_marker = call
            .root
            .as_ref()
            .map(|root| root.join(String::from_utf8_lossy(id).as_ref()));
        if let Some(marker) = &state_marker {
            let _ = std::fs::create_dir_all(marker);
        }
        let remove_state = || {
            if let Some(marker) = &state_marker {
                let _ = std::fs::remove_dir_all(marker);
            }
        };
        if let Some(text) = self.read("crun-error") {
            let code = self.cat("crun-error-code").first().map_or(1, |_| {
                String::from_utf8_lossy(&self.cat("crun-error-code"))
                    .parse()
                    .unwrap_or(1)
            });
            self.log_error(call, String::from_utf8_lossy(&text).trim());
            remove_state();
            self.append("events", b"run ended");
            return code;
        }
        if self.has("busy-once") {
            let _ = std::fs::remove_file(self.file("busy-once"));
            self.log_error(
                call,
                "exec container process `/kbf/root/tool`: Text file busy",
            );
            remove_state();
            self.append("events", b"run ended");
            return 1;
        }
        let lease = [self.cat("nonce"), lease_cgroup.as_bytes().to_vec()].concat();
        let paths = [
            (
                "CG",
                concat(&self.cgroot, lease_cgroup.as_bytes())
                    .into_os_string()
                    .into_vec(),
            ),
            ("ROOT", self.cat("root")),
            ("UPPER", self.cat("upper")),
        ];
        let act = self.link.with_file_name("kbf-act");
        let mut action = Command::new(&act);
        let steps = self.read("action").unwrap_or_default();
        for arg in arguments(&steps) {
            action.arg(OsString::from_vec(expand(arg, &paths)));
        }
        for (name, path) in &paths {
            action.env(name, OsStr::from_bytes(path));
        }
        action
            .env("FAKE_LEASE", OsStr::from_bytes(&lease))
            .stdin(Stdio::null());
        // SAFETY: the closure runs in the child between fork and exec, and makes one
        // async-signal-safe system call (setsid); it allocates nothing. The action leads
        // its own session and process group, the fake's stand-in for the lease cgroup.
        unsafe {
            action.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
        }
        let mut child = match action.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.log_error(call, &format!("run {}: {e}", act.display()));
                remove_state();
                return 127;
            }
        };
        self.write_line("pid.new", child.id().to_string().as_bytes());
        let _ = std::fs::rename(self.file("pid.new"), self.file("pid"));
        let kill = self.lease_cgroup().join("cgroup.kill");
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    eprintln!("fake crun: wait: {e}");
                    break ExitStatus::from_raw(1 << 8);
                }
            }
            if kill.exists() {
                self.append("events", b"cgroup.kill ended the action");
                self.kill_action();
                break child.wait().unwrap_or(ExitStatus::from_raw(1 << 8));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let code = status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1);
        let code = match self.read("exit-override") {
            Some(text) => String::from_utf8_lossy(&text)
                .trim()
                .parse()
                .unwrap_or(code),
            None => code,
        };
        // `crun run` deletes the container as it exits (unless a test models a `crun`
        // that was killed: its state stays for `delete --force`).
        if !self.has("leave-state") {
            remove_state();
        }
        self.append("events", b"run ended");
        if self.cat("owner") == b"1:1" {
            let upper = self.cat("upper");
            let upper = path(&upper);
            if let Ok(meta) = std::fs::metadata(upper) {
                let mode = meta.permissions().mode() & 0o7777;
                self.write_line("upper-mode", format!("{mode:o}").as_bytes());
            }
            let _ = std::fs::set_permissions(upper, std::fs::Permissions::from_mode(0o000));
        }
        u8::try_from(code & 0xff).unwrap_or(1)
    }

    /// A helper container: the host directory each `/t/<n>` bind names.
    fn helper(&self, config: &Value) -> u8 {
        let mut binds: Vec<(String, PathBuf)> = Vec::new();
        for mount in config["mounts"].as_array().into_iter().flatten() {
            let (Some(dest), Some(source)) =
                (mount["destination"].as_str(), mount["source"].as_str())
            else {
                continue;
            };
            if dest.starts_with("/t/") {
                binds.push((dest.to_owned(), PathBuf::from(source)));
            }
        }
        let host = |name: &str| host_path(&binds, name);
        let args: Vec<String> = config["process"]["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect();
        match args.first().map(String::as_str) {
            Some("/usr/bin/find") => self.helper_rm(&host("/t/0")),
            Some("/usr/bin/mkdir") => {
                self.append("calls", b"helper mkdir");
                for dir in args.iter().skip(1).filter(|a| a.starts_with("/t/")) {
                    let _ = std::fs::create_dir_all(path(&host_path(&binds, dir)));
                }
                0
            }
            Some("/usr/bin/tar") => {
                self.append("calls", b"helper tar");
                if self.has("tar-fails") {
                    return refuse("fake crun: tar refused", 2);
                }
                0
            }
            Some("/usr/bin/touch") => {
                self.append("calls", b"helper touch");
                for file in args.iter().skip(1).filter(|a| a.starts_with("/t/")) {
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path(&host_path(&binds, file)));
                }
                0
            }
            Some("/kbf/daemon") => self.helper_give(&args, &host),
            other => refuse(&format!("fake crun: a helper running {other:?}"), 1),
        }
    }

    /// `find /t/0 -mindepth 1 -delete`: the contents of the bound directory.
    fn helper_rm(&self, dir: &[u8]) -> u8 {
        self.append("calls", b"helper rm");
        if self.has("rm-fails") {
            return refuse("fake crun: rm refused", 1);
        }
        if self.has("rm-noop") {
            return 0;
        }
        add_mode_all(path(dir), 0o700);
        if let Ok(entries) = std::fs::read_dir(path(dir)) {
            for entry in entries.flatten() {
                let p = entry.path();
                let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
            }
        }
        0
    }

    /// `/kbf/daemon __chown-dirs OWNER DIRS|- --whole TREES...`.
    fn helper_give(&self, args: &[String], host: &dyn Fn(&str) -> Vec<u8>) -> u8 {
        self.append("calls", b"helper give");
        // A runtime that goes away (or stops being executable) once it has run a helper.
        if self.has("vanish-after-give") {
            let _ = std::fs::remove_file(&self.link);
        }
        if self.has("unexec-after-give") {
            let _ = std::fs::set_permissions(&self.link, std::fs::Permissions::from_mode(0o644));
        }
        let owner = args.get(2).cloned().unwrap_or_default();
        let dirs = args.get(3).cloned().unwrap_or_default();
        let paths: Vec<Vec<u8>> = args.iter().skip(5).map(|p| host(p)).collect();
        if dirs != "-" {
            self.append(
                "events",
                &[
                    b"chown-dirs ".as_slice(),
                    owner.as_bytes(),
                    b" ",
                    &host(&dirs),
                ]
                .concat(),
            );
        }
        self.append(
            "events",
            &[
                b"chown ".as_slice(),
                owner.as_bytes(),
                b" ",
                &paths.join(&b' '),
            ]
            .concat(),
        );
        if self.has("chown-fails") && self.cat("chown-fails") == owner.as_bytes() {
            return refuse("fake crun: chown refused", 1);
        }
        self.write_line("owner", owner.as_bytes());
        if owner == "0:0" && self.has("upper-mode") {
            let mode = String::from_utf8_lossy(&self.cat("upper-mode")).into_owned();
            if let Ok(mode) = u32::from_str_radix(&mode, 8) {
                let upper = self.cat("upper");
                let mode = std::fs::Permissions::from_mode(mode);
                let _ = std::fs::set_permissions(path(&upper), mode);
            }
            let _ = std::fs::remove_file(self.file("upper-mode"));
        }
        0
    }

    fn kill(&self, args: &[Vec<u8>]) -> u8 {
        self.append("calls", b"kill");
        if self.has("kill-fails") {
            return refuse("fake crun: kill refused", 1);
        }
        if !self.has("pid") {
            return refuse("fake crun: no such container", 1);
        }
        let name = args.get(1).map_or(&[][..], Vec::as_slice);
        let Some(signal) = signal(name) else {
            let name = String::from_utf8_lossy(name);
            return refuse(&format!("fake crun: kill: {name}: invalid signal"), 1);
        };
        let Some(pid) = self.pid() else {
            return refuse("fake crun: kill: no pid", 1);
        };
        match rustix::process::kill_process(pid, signal) {
            Ok(()) => 0,
            Err(e) => refuse(&format!("fake crun: kill: {e}"), 1),
        }
    }

    fn delete(&self, call: &Call<'_>) -> u8 {
        self.append("calls", b"delete");
        if self.has("start-pid") {
            let proc = concat(Path::new("/proc/"), &self.cat("start-pid"));
            if proc.exists() {
                self.touch("run-not-reaped-at-delete");
            }
        }
        if self.cgroup_killed() {
            self.kill_action();
        }
        if self.has("delete-fails") {
            return refuse("fake crun: delete refused", 1);
        }
        self.touch("removed");
        if let (Some(root), Some(id)) = (&call.root, call.rest.last()) {
            let _ = std::fs::remove_dir_all(root.join(String::from_utf8_lossy(id).as_ref()));
        }
        self.append("events", b"delete");
        if self.cgroup_killed() {
            self.touch("killed-before-delete");
        }
        self.kill_action();
        if self.has("cgroup") {
            let dir = self.lease_cgroup();
            add_mode(&dir, 0o200);
            remove_files(&dir);
        }
        0
    }

    fn pid(&self) -> Option<Pid> {
        let text = String::from_utf8_lossy(&self.cat("pid")).into_owned();
        text.parse().ok().and_then(Pid::from_raw)
    }

    /// SIGKILL to the last action and its process group, returning once it has ended
    /// (see `kbf-fake-podman`, which this follows).
    fn kill_action(&self) {
        let Some(pid) = self.pid() else {
            return;
        };
        let _ = rustix::process::kill_process(pid, Signal::KILL);
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        let stat = format!("/proc/{}/stat", pid.as_raw_nonzero());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let running = std::fs::read_to_string(&stat).is_ok_and(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, rest)| !rest.starts_with('Z'))
            });
            if !running {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The signal `kill` names: a name, with or without `SIG`, or a number.
fn signal(name: &[u8]) -> Option<Signal> {
    let name = name.strip_prefix(b"SIG").unwrap_or(name);
    Some(match name {
        b"HUP" => Signal::HUP,
        b"INT" => Signal::INT,
        b"QUIT" => Signal::QUIT,
        b"KILL" => Signal::KILL,
        b"USR1" => Signal::USR1,
        b"USR2" => Signal::USR2,
        b"TERM" => Signal::TERM,
        b"CONT" => Signal::CONT,
        b"STOP" => Signal::STOP,
        _ => {
            let number = std::str::from_utf8(name).ok()?.parse().ok()?;
            Signal::from_named_raw(number)?
        }
    })
}

fn add_mode(path: &Path, bits: u32) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() | bits;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

fn add_mode_all(top: &Path, bits: u32) {
    add_mode(top, bits);
    let mut pending = vec![top.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            add_mode(&entry.path(), bits);
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
}

fn remove_files(dir: &Path) {
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_file() => {
                    let _ = std::fs::remove_file(entry.path());
                }
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                _ => {}
            }
        }
    }
}
