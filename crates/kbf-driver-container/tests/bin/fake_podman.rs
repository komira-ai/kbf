//! `kbf-fake-podman`: the stand-in `podman` of the container driver's tests, in Rust.
//! It answers every verb `fixtures/fake-podman.sh` answers, reads the same knobs and
//! writes the same records, byte for byte (`tests/fake_parity.rs` compares the two),
//! so a test passes or fails the same way on either. `tests/support/fake.rs` links one
//! of them as `<dir>/podman`, as `KBF_TEST_FAKE_PODMAN` picks it; the knobs and
//! records are listed at the top of the script.
//!
//! What the script runs other programs for, this does itself: `image-hangs` sleeps 30
//! seconds in this process, and the kills, the session the action leads, the file
//! deletes and the mode changes are system calls. The action (`state/action.sh`) is
//! the test's own script and is still run by `bash`, in a new session, with stdin
//! from `/dev/null` as a background job of the script has it.

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal};

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    // The link this was run through: the driver runs it by its full path.
    let link = PathBuf::from(args.next().unwrap_or_default());
    let args: Vec<Vec<u8>> = args.map(std::ffi::OsString::into_vec).collect();
    let here = link.parent().unwrap_or(Path::new(".")).to_owned();
    let fake = Fake {
        state: here.join("state"),
        cgroot: here.join("cgroup"),
        link,
    };
    ExitCode::from(fake.run(&args))
}

/// One run of the fake: where its state and fake cgroup mount are.
struct Fake {
    link: PathBuf,
    state: PathBuf,
    cgroot: PathBuf,
}

/// Prints `message` on stderr and returns `code`, the run's exit status.
fn refuse(message: &str, code: u8) -> u8 {
    eprintln!("{message}");
    code
}

/// `parts` joined by single spaces, as the script's `$*` is.
fn joined(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.join(&b' ')
}

/// `bytes` as a path.
fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// `a` then `b`, as the script's `"$A$B"` is (not a path join: `b` may start with `/`).
fn concat(a: &Path, b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&[a.as_os_str().as_bytes(), b].concat()))
}

/// What the script prints for `printf '%s\n' "$@"`: one argument per line, or one
/// empty line when there are none.
fn lines(args: &[Vec<u8>]) -> Vec<u8> {
    if args.is_empty() {
        return b"\n".to_vec();
    }
    args.iter()
        .flat_map(|a| [a.as_slice(), b"\n"].concat())
        .collect()
}

/// Writes `bytes` on stdout. A closed stdout is ignored, as `echo` into one is.
fn print(bytes: &[u8]) {
    let _ = std::io::stdout().write_all(bytes);
}

impl Fake {
    fn file(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    fn has(&self, name: &str) -> bool {
        self.file(name).is_file()
    }

    /// The file's bytes, `None` when it cannot be read.
    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.file(name)).ok()
    }

    /// The file as the script's `$(cat FILE)` has it: trailing newlines dropped, and
    /// empty when it cannot be read.
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

    /// Writes `line` and a newline to the file, as `echo LINE >FILE` does.
    fn write_line(&self, name: &str, line: &[u8]) {
        self.write(name, &[line, b"\n"].concat());
    }

    /// Appends `line` and a newline to the file, as `echo LINE >>FILE` does.
    fn append(&self, name: &str, line: &[u8]) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name))
        {
            let _ = file.write_all(&[line, b"\n"].concat());
        }
    }

    /// Creates the file if it is not there, as `touch` does (its time is not kept).
    fn touch(&self, name: &str) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file(name));
    }

    /// The lease cgroup `create` recorded, under the fake mount.
    fn lease_cgroup(&self) -> PathBuf {
        concat(&self.cgroot, &self.cat("cgroup"))
    }

    /// Whether `create` recorded a cgroup whose `cgroup.kill` has been written.
    fn cgroup_killed(&self) -> bool {
        self.has("cgroup") && self.lease_cgroup().join("cgroup.kill").exists()
    }

    fn run(&self, args: &[Vec<u8>]) -> u8 {
        let Some((flag, args)) = args.split_first() else {
            return refuse("fake podman: missing --cgroup-manager", 125);
        };
        if flag != b"--cgroup-manager=cgroupfs" {
            return refuse("fake podman: missing --cgroup-manager", 125);
        }
        let Some((verb, args)) = args.split_first() else {
            return refuse("fake podman: no verb", 125);
        };
        if verb == b"unshare" {
            let what = args.first().map_or(&[][..], Vec::as_slice);
            self.append("calls", &[b"unshare ", what].concat());
        } else {
            self.append("calls", verb);
        }
        match verb.as_slice() {
            b"image" => self.image(),
            b"info" => self.info(),
            b"create" => self.create(args),
            b"start" => self.start(),
            b"inspect" => self.inspect(),
            b"kill" => self.kill(args),
            b"ps" => self.ps(args),
            b"rm" => self.rm(),
            b"unshare" => self.unshare(args),
            _ => refuse(
                &format!(
                    "fake podman: unexpected verb {}",
                    String::from_utf8_lossy(verb)
                ),
                125,
            ),
        }
    }

    fn image(&self) -> u8 {
        if self.has("image-hangs") {
            std::thread::sleep(Duration::from_secs(30));
            return 0;
        }
        let Some(id) = self.read("image-id") else {
            return refuse("Error: image not known", 125);
        };
        print(&id);
        0
    }

    fn info(&self) -> u8 {
        if self.has("info-fails") {
            return refuse("Error: info refused", 125);
        }
        let store = self.state.join("store/fake-images");
        print(&[store.as_os_str().as_bytes(), b"\n"].concat());
        0
    }

    fn create(&self, args: &[Vec<u8>]) -> u8 {
        self.write("create.args", &lines(args));
        for arg in args {
            if let Some(parent) = arg.strip_prefix(b"--cgroup-parent=") {
                self.write_line("cgroup", parent);
            } else if let Some(owner) = arg.strip_prefix(b"--label=kbf.owner=") {
                self.write_line("owner-label", owner);
            } else if let Some(name) = arg.strip_prefix(b"--name=") {
                self.write_line("name", name);
            } else if let Some(volume) = arg.strip_prefix(b"--volume=") {
                // `${v%%:*}`, then `${v#*upperdir=}` and its `${u%%,*}`.
                let root = volume.split(|b| *b == b':').next().unwrap_or_default();
                self.write_line("root", root);
                let key = b"upperdir=";
                let upper = volume
                    .windows(key.len())
                    .position(|w| w == key)
                    .map_or(volume, |at| &volume[at + key.len()..]);
                let upper = upper.split(|b| *b == b',').next().unwrap_or_default();
                self.write_line("upper", upper);
            }
        }
        if self.has("create-fails") {
            return refuse("Error: create refused", 125);
        }
        if self.has("block-log") {
            let root = self.cat("root");
            let dir = path(&root).parent().unwrap_or(Path::new("."));
            let log = dir.join(OsStr::from_bytes(&self.cat("block-log")));
            if let Err(e) = std::fs::create_dir(&log) {
                eprintln!("fake podman: mkdir {}: {e}", log.display());
            }
        }
        // The podman program disappears once the container exists.
        if self.has("vanish-after-create") {
            let _ = std::fs::remove_file(&self.link);
        }
        print(b"0123456789ab\n");
        0
    }

    fn start(&self) -> u8 {
        self.write_line("start-pid", std::process::id().to_string().as_bytes());
        let cgroup = self.cat("cgroup");
        let lease = [self.cat("nonce"), cgroup.clone()].concat();
        let mut action = Command::new("bash");
        action
            .arg(self.file("action.sh"))
            .env("CG", concat(&self.cgroot, &cgroup))
            .env("ROOT", OsStr::from_bytes(&self.cat("root")))
            .env("UPPER", OsStr::from_bytes(&self.cat("upper")))
            .env("FAKE_LEASE", OsStr::from_bytes(&lease))
            .stdin(Stdio::null());
        // SAFETY: the closure runs in the child between fork and exec, and makes one
        // system call (setsid), which is async-signal-safe; it allocates nothing.
        // The action leads its own session and process group, the fake's stand-in for
        // the lease cgroup: `kill_action` ends everything the action started.
        unsafe {
            action.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
        }
        let mut child = match action.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("fake podman: run bash: {e}");
                return self.ended(127);
            }
        };
        // Published whole (a rename), so a reader never finds the file empty.
        self.write_line("pid.new", child.id().to_string().as_bytes());
        let _ = std::fs::rename(self.file("pid.new"), self.file("pid"));
        let kill = self.lease_cgroup().join("cgroup.kill");
        // cgroup.kill: a write to the file kills the action, as the kernel would. Watched
        // by this process, so nothing of the fake outlives a `start` the driver kills
        // (kbf #157).
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    eprintln!("fake podman: wait: {e}");
                    break ExitStatus::from_raw(1 << 8);
                }
            }
            if kill.exists() {
                // Recorded before the kill, so it precedes `start ended`.
                self.append("events", b"cgroup.kill ended the action");
                self.kill_action();
                break child.wait().unwrap_or(ExitStatus::from_raw(1 << 8));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // A shell's `$?`: the exit code, or 128 plus the signal that ended it.
        let code = status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1);
        self.ended(u8::try_from(code & 0xff).unwrap_or(1))
    }

    /// What `start` does once the action has ended with `code`: the status `inspect`
    /// reports, `start ended`, and the upper directory made unreadable when the
    /// overlay is the container's.
    fn ended(&self, code: u8) -> u8 {
        match self.read("status-override") {
            Some(status) => self.write("status", &status),
            None => self.write_line("status", format!("exited {code}").as_bytes()),
        }
        self.append("events", b"start ended");
        if self.cat("owner") == b"1:1" {
            let upper = self.cat("upper");
            let upper = path(&upper);
            if let Ok(meta) = std::fs::metadata(upper) {
                let mode = meta.permissions().mode() & 0o7777;
                self.write_line("upper-mode", format!("{mode:o}").as_bytes());
            }
            let _ = std::fs::set_permissions(upper, std::fs::Permissions::from_mode(0o000));
        }
        code
    }

    fn inspect(&self) -> u8 {
        if self.has("inspect-fails") {
            return refuse("Error: inspect refused", 125);
        }
        let Some(status) = self.read("status") else {
            return refuse("Error: no such container", 125);
        };
        print(&status);
        0
    }

    fn kill(&self, args: &[Vec<u8>]) -> u8 {
        if self.has("kill-fails") {
            return refuse("Error: kill refused", 125);
        }
        if !self.has("pid") {
            return refuse("Error: no such container", 125);
        }
        let name = args.first().map_or(&[][..], Vec::as_slice);
        let name = name.strip_prefix(b"--signal=").unwrap_or(name);
        let Some(signal) = signal(name) else {
            let name = String::from_utf8_lossy(name);
            return refuse(&format!("fake podman: kill: {name}: invalid signal"), 1);
        };
        let Some(pid) = self.pid() else {
            return refuse("fake podman: kill: no pid", 1);
        };
        match rustix::process::kill_process(pid, signal) {
            Ok(()) => 0,
            Err(e) => refuse(&format!("fake podman: kill: {e}"), 1),
        }
    }

    fn ps(&self, args: &[Vec<u8>]) -> u8 {
        self.write("ps.args", &lines(args));
        if self.has("ps-fails") {
            return refuse("Error: ps refused", 125);
        }
        if self.has("name") && self.has("owner-label") {
            let owner = self.cat("owner-label");
            let filter = [&b"--filter=label=kbf.owner="[..], &owner].concat();
            for arg in args {
                if *arg == filter {
                    print(&[&owner[..], b" ", &self.cat("name"), b"\n"].concat());
                }
            }
        }
        if let Some(extra) = self.read("ps-extra") {
            print(&extra);
        }
        0
    }

    fn rm(&self) -> u8 {
        // The driver must have waited for `podman start` before it removes the container:
        // what it started may otherwise still write into the lease after the clean.
        if self.has("start-pid") {
            let proc = concat(Path::new("/proc/"), &self.cat("start-pid"));
            if proc.exists() {
                self.touch("start-not-reaped-at-rm");
            }
        }
        // The kernel kills a cgroup's processes when cgroup.kill is written. Once `start`
        // (the watcher) is gone, nothing in the fake runs at that write; `rm` is the
        // first verb the clean runs after it, so the kill is modelled here, before any knob.
        if self.cgroup_killed() {
            self.kill_action();
        }
        if self.has("rm-fails") {
            return refuse("Error: rm refused", 125);
        }
        self.touch("removed");
        let _ = std::fs::remove_file(self.file("name"));
        self.append("events", b"rm");
        if self.cgroup_killed() {
            self.touch("killed-before-rm");
        }
        self.kill_action(); // --force: a container still running is killed.
        // Interface files are not files to rmdir on cgroupfs; here they are, so the fake
        // removes them as the kernel would, whatever the cgroup's mode (a test makes it
        // read-only).
        if self.has("cgroup") {
            let dir = self.lease_cgroup();
            add_mode(&dir, 0o200);
            remove_files(&dir);
        }
        0
    }

    fn unshare(&self, args: &[Vec<u8>]) -> u8 {
        if args.first().is_some_and(|a| a == b"chown") {
            // chown -hR OWNER -- PATHS...
            let (Some(b"-hR"), Some(owner), Some(b"--")) = (
                args.get(1).map(Vec::as_slice),
                args.get(2),
                args.get(3).map(Vec::as_slice),
            ) else {
                let all = String::from_utf8_lossy(&joined(args)).into_owned();
                return refuse(&format!("fake podman: chown {all}"), 125);
            };
            let paths = &args[4..];
            self.append(
                "events",
                &[b"chown ", &owner[..], b" ", &joined(paths)].concat(),
            );
            if self.has("chown-fails") && self.cat("chown-fails") == *owner {
                return refuse("Error: chown refused", 1);
            }
            self.write_line("owner", owner);
            if owner == b"0:0" && self.has("upper-mode") {
                let mode = String::from_utf8_lossy(&self.cat("upper-mode")).into_owned();
                if let Ok(mode) = u32::from_str_radix(&mode, 8) {
                    let upper = self.cat("upper");
                    let mode = std::fs::Permissions::from_mode(mode);
                    let _ = std::fs::set_permissions(path(&upper), mode);
                }
                let _ = std::fs::remove_file(self.file("upper-mode"));
            }
            return 0;
        }
        if self.has("unshare-fails") {
            return refuse("Error: unshare refused", 1);
        }
        if self.has("unshare-noop") {
            return 0;
        }
        // rm -rf -- DIR
        let Some(dir) = args.get(3).filter(|d| !d.is_empty()) else {
            return refuse("fake podman: unshare rm: no directory", 1);
        };
        let dir = path(dir);
        add_mode_all(dir, 0o700);
        match std::fs::remove_dir_all(dir) {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => refuse(&format!("fake podman: rm {}: {e}", dir.display()), 1),
        }
    }

    /// The last action's pid, as `start` published it.
    fn pid(&self) -> Option<Pid> {
        let text = String::from_utf8_lossy(&self.cat("pid")).into_owned();
        text.parse().ok().and_then(Pid::from_raw)
    }

    /// SIGKILL to the last action and its process group: everything it started. The
    /// pid first: a child not yet past `setsid` has no group of its own and has started
    /// nothing, so killing it is enough. Once it leads its group, the group outlives the
    /// leader for as long as anything is in it, so the group kill after still finds the
    /// rest.
    fn kill_action(&self) {
        if let Some(pid) = self.pid() {
            let _ = rustix::process::kill_process(pid, Signal::KILL);
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        }
    }
}

/// The signal `kill -NAME` names: a name, with or without `SIG`, or a number.
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

/// Adds `bits` to the mode of `path` (`chmod u+...`), following a symlink as `chmod`
/// does; a failure is ignored, as the script ignores it.
fn add_mode(path: &Path, bits: u32) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() | bits;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

/// `chmod -R u+...`: `bits` added to `top` and to everything below it, a directory
/// before what is in it (so it can be read). Symlinks below `top` are left alone.
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

/// `find DIR -type f -delete`: removes every regular file below `dir`, keeping the
/// directories, never following a symlink; a failure is ignored.
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
