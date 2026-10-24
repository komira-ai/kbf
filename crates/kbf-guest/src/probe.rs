//! Probe modes: what a process inside a guest can do, written as a JSON file the host
//! reads from the shared folder.
//!
//! These run in the base image before any agent exists (`kbf-vmm probe g2` and `g3`
//! describe the questions). Unlike every other subcommand they run as any user, root
//! included, because what each user may do is part of the answer; they write one file and
//! run no command for the host.
//!
//! Result keys the host reads: `mount_custom_tag.exit_code`, `automount.dir_readable`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// How long a probe waits for its shared folder (or the login window) to appear.
pub const WAIT: Duration = Duration::from_secs(300);
/// One wait step.
pub const TICK: Duration = Duration::from_millis(500);

/// What a probe asks of the guest; a fake in tests.
pub trait Host {
    /// The real and effective user ids.
    fn ids(&self) -> (u32, u32);
    /// The launchd session type (`System`, `Aqua`, ...).
    fn session(&self) -> String;
    fn exists(&self, path: &Path) -> bool;
    fn process_running(&self, name: &str) -> bool;
    /// Tries a guest-side vsock connection to the host on `port`.
    fn vsock(&self, port: u32) -> Value;
    /// Looks for the console port `name` and writes to every candidate device.
    fn console(&self, name: &str) -> Value;
    /// Mounts the virtiofs `tag` at `mountpoint` the way a user would.
    fn mount_tag(&self, tag: &str, mountpoint: &Path) -> Value;
    /// Whether the automount share at `dir` can be read.
    fn automount(&self, dir: &Path) -> Value;
    fn pause(&self, d: Duration);
}

/// Polls `cond` every [`TICK`] until it holds or `limit` of waiting has passed; the time
/// waited when it held, `None` when the time was up.
pub fn wait_until(host: &dyn Host, limit: Duration, cond: impl Fn() -> bool) -> Option<Duration> {
    let mut waited = Duration::ZERO;
    while !cond() {
        if waited >= limit {
            return None;
        }
        host.pause(TICK);
        waited += TICK;
    }
    Some(waited)
}

/// `probe-guest`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct GuestArgs {
    /// The file to write.
    #[arg(long)]
    pub out: PathBuf,
    /// Wait for this path to exist first (the automount share).
    #[arg(long)]
    pub wait_path: Option<PathBuf>,
    /// The directory the system's own automount would show the share at; its
    /// readability is part of the answer, whatever `--wait-path` is.
    #[arg(long, default_value = "/Volumes/My Shared Files/kbf")]
    pub automount_dir: PathBuf,
    #[arg(long, default_value_t = 5000)]
    pub vsock_port: u32,
    #[arg(long, default_value = "kbf.probe")]
    pub console_name: String,
    /// The virtiofs tag to mount.
    #[arg(long, default_value = "kbfprobe")]
    pub tag: String,
    /// Where to mount it.
    #[arg(long, default_value = "/private/tmp/kbfmnt")]
    pub mountpoint: PathBuf,
}

/// `probe-ready`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct ReadyArgs {
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long)]
    pub wait_path: Option<PathBuf>,
    /// Wait for this process (for instance `loginwindow`) to be running first.
    #[arg(long)]
    pub wait_process: Option<String>,
}

/// Waits for `path`; the milliseconds it took (0 when there is nothing to wait for).
fn wait_for_path(host: &dyn Host, path: Option<&Path>) -> Result<u64, String> {
    match path {
        Some(p) => wait_until(host, WAIT, || host.exists(p))
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .ok_or_else(|| format!("{} never appeared", p.display())),
        None => Ok(0),
    }
}

/// Runs every question and returns the answers.
///
/// # Errors
/// The wait path never appeared.
pub fn probe_guest(host: &dyn Host, a: &GuestArgs) -> Result<Value, String> {
    let share_wait_ms = wait_for_path(host, a.wait_path.as_deref())?;
    let (uid, euid) = host.ids();
    Ok(json!({
        "share_wait_ms": share_wait_ms,
        "share_path": a.wait_path,
        "uid": uid,
        "euid": euid,
        "session": host.session(),
        "vsock": host.vsock(a.vsock_port),
        "console": host.console(&a.console_name),
        "mount_custom_tag": host.mount_tag(&a.tag, &a.mountpoint),
        "automount": host.automount(&a.automount_dir),
    }))
}

/// Waits for the path and the process, then reports who and where it is.
///
/// # Errors
/// The path or the process never appeared.
pub fn probe_ready(host: &dyn Host, a: &ReadyArgs) -> Result<Value, String> {
    let share_wait_ms = wait_for_path(host, a.wait_path.as_deref())?;
    if let Some(name) = &a.wait_process
        && wait_until(host, WAIT, || host.process_running(name)).is_none()
    {
        return Err(format!("process {name} never appeared"));
    }
    let (uid, euid) = host.ids();
    Ok(json!({
        "uid": uid,
        "euid": euid,
        "session": host.session(),
        "share_wait_ms": share_wait_ms,
    }))
}

/// Writes `value` to `out` through a temporary file, so the host never reads half of it.
///
/// # Errors
/// The write or the rename failed.
pub fn write_result(out: &Path, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let tmp = out.with_extension("tmp");
    let first = std::fs::write(&tmp, &bytes)
        .map_err(|e| format!("create {}: {e}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, out)
                .map_err(|e| format!("rename {} to {}: {e}", tmp.display(), out.display()))
        });
    let Err(first) = first else { return Ok(()) };
    // A share may refuse the temporary file or the rename; the file itself is what the
    // host waits for, so write it in place rather than not at all.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(out, &bytes).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "create {}: {e} (the temporary file route failed first: {first})",
                out.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Fake {
        ids: (u32, u32),
        present: RefCell<Vec<PathBuf>>,
        appears_after: Cell<Option<u32>>,
        pauses: Cell<u32>,
        process_after: Cell<Option<u32>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                ids: (0, 0),
                present: RefCell::new(Vec::new()),
                appears_after: Cell::new(None),
                pauses: Cell::new(0),
                process_after: Cell::new(Some(0)),
            }
        }
    }

    impl Host for Fake {
        fn ids(&self) -> (u32, u32) {
            self.ids
        }
        fn session(&self) -> String {
            "System".to_owned()
        }
        fn exists(&self, path: &Path) -> bool {
            self.present.borrow().iter().any(|p| p == path)
                || self
                    .appears_after
                    .get()
                    .is_some_and(|n| self.pauses.get() >= n)
        }
        fn process_running(&self, _name: &str) -> bool {
            self.process_after
                .get()
                .is_some_and(|n| self.pauses.get() >= n)
        }
        fn vsock(&self, port: u32) -> Value {
            json!({"port": port})
        }
        fn console(&self, name: &str) -> Value {
            json!({"name": name})
        }
        fn mount_tag(&self, tag: &str, mountpoint: &Path) -> Value {
            json!({"tag": tag, "mountpoint": mountpoint, "exit_code": 1})
        }
        fn automount(&self, dir: &Path) -> Value {
            json!({"dir": dir, "dir_readable": true})
        }
        fn pause(&self, _d: Duration) {
            self.pauses.set(self.pauses.get() + 1);
        }
    }

    fn guest_args(wait: Option<&str>) -> GuestArgs {
        GuestArgs {
            out: PathBuf::from("/o"),
            wait_path: wait.map(PathBuf::from),
            automount_dir: PathBuf::from("/auto"),
            vsock_port: 5000,
            console_name: "c".into(),
            tag: "t".into(),
            mountpoint: PathBuf::from("/m"),
        }
    }

    /// Catches a wait that never returns, returns early, or counts the final check.
    #[test]
    fn waiting_stops_when_the_condition_holds_or_the_time_is_up() {
        let h = Fake::new();
        assert_eq!(wait_until(&h, WAIT, || true), Some(Duration::ZERO));
        assert_eq!(h.pauses.get(), 0);
        assert_eq!(wait_until(&h, Duration::from_secs(1), || false), None);
        assert_eq!(h.pauses.get(), 2);
        let h = Fake::new();
        let n = Cell::new(0);
        let waited = wait_until(&h, WAIT, || {
            n.set(n.get() + 1);
            n.get() > 3
        });
        assert_eq!(waited, Some(TICK * 3));
    }

    /// Catches answers lost on the way into the JSON, a wait path that is ignored, the
    /// time spent waiting for the share not recorded, and an automount probe aimed at
    /// the wait path instead of the system automount directory.
    #[test]
    fn probe_guest_collects_every_answer_after_the_share_appears() {
        let h = Fake::new();
        h.appears_after.set(Some(3));
        let v = probe_guest(&h, &guest_args(Some("/share"))).expect("probe");
        assert_eq!(h.pauses.get(), 3);
        assert_eq!(v["share_wait_ms"], 1500);
        assert_eq!(v["uid"], 0);
        assert_eq!(v["session"], "System");
        assert_eq!(v["vsock"]["port"], 5000);
        assert_eq!(v["console"]["name"], "c");
        assert_eq!(v["mount_custom_tag"]["exit_code"], 1);
        assert_eq!(v["automount"]["dir"], "/auto");
        assert_eq!(v["share_path"], "/share");
        let there = Fake::new();
        there.present.borrow_mut().push(PathBuf::from("/share"));
        probe_guest(&there, &guest_args(Some("/share"))).expect("present");
        assert_eq!(there.pauses.get(), 0);
        let v = probe_guest(&Fake::new(), &guest_args(None)).expect("no wait");
        assert_eq!(v["share_wait_ms"], 0);
    }

    /// Catches a probe that writes an answer when its share never mounted.
    #[test]
    fn probe_guest_fails_when_the_share_never_appears() {
        let err = probe_guest(&Fake::new(), &guest_args(Some("/share"))).expect_err("timeout");
        assert!(err.contains("/share never appeared"), "{err}");
    }

    /// Catches a ready marker written before the process it waits for exists, or one
    /// that waits forever.
    #[test]
    fn probe_ready_waits_for_the_path_and_the_process() {
        let h = Fake::new();
        h.process_after.set(Some(4));
        let a = ReadyArgs {
            out: "/o".into(),
            wait_path: None,
            wait_process: Some("loginwindow".into()),
        };
        let v = probe_ready(&h, &a).expect("ready");
        assert_eq!((h.pauses.get(), v["session"].clone()), (4, json!("System")));
        h.process_after.set(None);
        assert!(
            probe_ready(&h, &a)
                .expect_err("never")
                .contains("loginwindow")
        );
        let a = ReadyArgs {
            out: "/o".into(),
            wait_path: Some("/x".into()),
            wait_process: None,
        };
        assert!(probe_ready(&Fake::new(), &a).is_err());
        let a = ReadyArgs {
            out: "/o".into(),
            wait_path: None,
            wait_process: None,
        };
        assert!(probe_ready(&Fake::new(), &a).is_ok());
    }

    /// Catches a result written in place (the host could read half of it) and a failed
    /// write reported as success.
    #[test]
    fn the_result_is_written_through_a_rename() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-probe");
        let out = dir.join("r.json");
        write_result(&out, &json!({"a": 1})).expect("write");
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&out).expect("read")).expect("json")["a"],
            1
        );
        assert!(!dir.join("r.tmp").exists());
        assert!(write_result(&dir.join("no/such/r.json"), &json!({})).is_err());
        // A temporary file that cannot be made (the first E1 share refused writes) falls back
        // to writing the result in place.
        std::fs::create_dir(dir.join("fb.tmp")).expect("blocker");
        write_result(&dir.join("fb.json"), &json!({"b": 2})).expect("in place");
        assert!(
            std::fs::read_to_string(dir.join("fb.json"))
                .expect("read")
                .contains("\"b\"")
        );
        // When both routes fail the error names both, so the log says which step was refused.
        std::fs::create_dir(dir.join("both.json")).expect("dir");
        std::fs::write(dir.join("both.json").join("x"), "x").expect("x");
        let e = write_result(&dir.join("both.json"), &json!({}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("rename"), "{e}");
        assert!(e.contains("both.json"), "{e}");
        assert!(
            !dir.join("both.tmp").exists(),
            "the temporary file is removed"
        );
    }
}
