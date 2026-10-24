use std::cell::RefCell;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;
use crate::fixtures::{ran, scratch_dir};
use crate::layout::Platform;

const TOML: &str = "mode = \"daemon\"\n[identity]\nnode-id = \"n\"\n[daemon]\ndriver = [\"native\"]\ndata-dir = \"/Volumes/kbf/data\"\n";
const DAEMON: &str = "/usr/local/kbf-installer/releases/current/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml";
const KBF_TOML: &str = "/usr/local/etc/kbf/kbf.toml";

/// What a fake program answers: `None` is a program that could not be started.
pub(crate) type Reply = Option<Ran>;

fn said(code: i32, text: &str) -> Reply {
    Some(ran(code, text, ""))
}

/// A `ps` row, laid out in padded columns as `ps` does.
fn row(pid: u32, ppid: u32, etime: &str, command: &str) -> String {
    format!("{pid:>5} {ppid:>5} {etime:>10} {command}\n")
}

/// What runs when a spawn starts.
type Hook = Box<dyn Fn(&Spawn)>;

/// The programs of the host and how they answer.
struct Script {
    keepers: Reply,
    daemons: Reply,
    ps: Reply,
    /// Run when a spawn starts (a test makes the host change under the check).
    hook: Option<Hook>,
}

struct ScriptRunner {
    script: Rc<RefCell<Script>>,
    calls: Rc<RefCell<Vec<Spawn>>>,
}

impl Runner for ScriptRunner {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        self.calls.borrow_mut().push(spawn.clone());
        let script = self.script.borrow();
        if let Some(hook) = &script.hook {
            hook(spawn);
        }
        let args: Vec<_> = spawn.args.iter().map(|a| a.to_string_lossy()).collect();
        let reply = match (spawn.program.to_str(), args.get(1).map(|a| &**a)) {
            (Some(PGREP), Some(KEEPER_PATTERN)) => &script.keepers,
            (Some(PGREP), Some(DAEMON_PATTERN)) => &script.daemons,
            (Some(PS), _) => &script.ps,
            _ => panic!("an unexpected program: {spawn:?}"),
        };
        reply
            .clone()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

/// A Mac with a trusted `kbf.toml`, `sandbox-exec`, an empty `leases/` and a daemon that has
/// run for ten minutes, none of its processes yet doing anything.
pub(crate) struct MacHost {
    pub root: PathBuf,
    pub layout: Layout,
    exec: FakeExec,
    script: Rc<RefCell<Script>>,
    calls: Rc<RefCell<Vec<Spawn>>>,
    /// The clock the source reads: 1000 s after `kbf.toml` last changed.
    pub now: i64,
}

fn root_owned() -> Owner {
    Owner { uid: 0, gid: 0 }
}

impl MacHost {
    pub fn new(name: &str) -> MacHost {
        let root = scratch_dir(&format!("mac-leases-{name}"));
        let layout = Layout::new(Root::new(&root).expect("absolute"), Platform::MacOs);
        let planted = |path: &str, text: &str| {
            let at = root.join(path.trim_start_matches('/'));
            fs::create_dir_all(at.parent().expect("a parent")).expect("parent");
            fs::write(at, text).expect("file");
        };
        planted(SANDBOX_EXEC, "#!");
        planted(KBF_TOML, TOML);
        fs::create_dir_all(root.join("Volumes/kbf/data/leases")).expect("leases");
        let meta = fs::metadata(root.join("usr/local/etc/kbf/kbf.toml")).expect("meta");
        let mut exec = FakeExec::new(0);
        for dir in ["/usr", "/usr/local", "/usr/local/etc", "/usr/local/etc/kbf"] {
            exec.entries.insert(
                HostPath::new(dir).expect("path"),
                Entry::Dir {
                    perms: Perms::new(0o755),
                    owner: root_owned(),
                },
            );
        }
        let mut rig = MacHost {
            root,
            layout,
            exec,
            script: Rc::new(RefCell::new(Script {
                keepers: said(1, ""),
                daemons: said(0, "309\n"),
                ps: Some(ran(0, &row(309, 1, "10:00", DAEMON), "")),
                hook: None,
            })),
            calls: Rc::new(RefCell::new(Vec::new())),
            now: meta.mtime().max(meta.ctime()) + 1000,
        };
        rig.toml(TOML);
        rig
    }

    /// The `kbf.toml` the installer reads (as root's, 0644).
    pub fn toml(&mut self, text: &str) {
        self.exec.entries.insert(
            HostPath::new(KBF_TOML).expect("path"),
            Entry::File {
                perms: Perms::new(0o644),
                owner: root_owned(),
                bytes: text.as_bytes().to_vec(),
            },
        );
    }

    /// The directory entry `name` below `leases/`.
    pub fn dir(&self, name: &str) {
        fs::create_dir_all(self.root.join("Volumes/kbf/data/leases").join(name)).expect("dir");
    }

    pub fn keepers(&self, reply: Reply) {
        self.script.borrow_mut().keepers = reply;
    }

    pub fn daemons(&self, reply: Reply) {
        self.script.borrow_mut().daemons = reply;
    }

    pub fn ps(&self, reply: Reply) {
        self.script.borrow_mut().ps = reply;
    }

    /// No kbf daemon runs.
    pub fn no_daemon(&self) {
        self.daemons(said(1, ""));
    }

    /// The daemon's one row: parent `ppid`, running for `etime`, started as `command`.
    pub fn daemon(&self, ppid: u32, etime: &str, command: &str) {
        self.ps(said(0, &row(309, ppid, etime, command)));
    }

    pub fn hook(&self, hook: impl Fn(&Spawn) + 'static) {
        self.script.borrow_mut().hook = Some(Box::new(hook));
    }

    pub fn calls(&self) -> Vec<Spawn> {
        self.calls.borrow().clone()
    }

    pub fn source(&mut self) -> MacLeases {
        let exec = std::mem::replace(&mut self.exec, FakeExec::new(0));
        let now = self.now;
        MacLeases::new(
            self.layout.clone(),
            Box::new(exec),
            Box::new(ScriptRunner {
                script: self.script.clone(),
                calls: self.calls.clone(),
            }),
            Box::new(move || now),
            Duration::from_secs(5),
        )
    }

    pub fn live(&mut self) -> Result<(usize, Option<String>), String> {
        self.source().live_noted()
    }

    /// The answer's count only.
    pub fn count(&mut self) -> Result<usize, String> {
        self.live().map(|(n, _)| n)
    }

    /// Remove `sandbox-exec`.
    pub fn without_sandbox(&self) {
        fs::remove_file(self.root.join("usr/bin/sandbox-exec")).expect("remove");
    }

    /// The `leases/` directory of the host.
    pub fn leases(&self) -> PathBuf {
        self.root.join("Volumes/kbf/data/leases")
    }
}

/// A keepers' `pgrep` answer listing `pids`.
pub(crate) fn reply_pids(pids: &str) -> Reply {
    said(0, pids)
}

/// An error that holds `needle`, or a failure showing what came instead.
#[track_caller]
fn refused(answer: Result<usize, String>, needle: &str) {
    match answer {
        Err(why) => assert!(why.contains(needle), "{why:?} lacks {needle:?}"),
        Ok(n) => panic!("answered {n}, not an error naming {needle:?}"),
    }
}

/// Catches an idle Mac read as busy, or the programs asked wrongly: with the daemon running
/// and no keeper and no lease directory the count is 0 with no note, and the installer asked
/// for the keepers, then the daemon, then its row, by absolute path, with an empty
/// environment, in `/`, as root, with the timeout.
///
/// The argument list and the environment have no separate defect named here; they are pinned by equality.
#[test]
fn an_idle_mac_reads_zero_and_the_programs_are_asked_as_designed() {
    let mut rig = MacHost::new("idle");
    assert_eq!(rig.live(), Ok((0, None)));
    let calls = rig.calls();
    let asked: Vec<_> = calls
        .iter()
        .map(|c| {
            let args: Vec<_> = c.args.iter().map(|a| a.to_str().expect("text")).collect();
            (c.program.to_str().expect("text"), args.join(" "))
        })
        .collect();
    assert_eq!(
        asked,
        vec![
            ("/usr/bin/pgrep", format!("-f {KEEPER_PATTERN}")),
            ("/usr/bin/pgrep", format!("-f {DAEMON_PATTERN}")),
            (
                "/bin/ps",
                "-ww -p 309 -o pid= -o ppid= -o etime= -o command=".to_owned()
            ),
        ]
    );
    for call in &calls {
        assert!(call.env.is_empty());
        assert_eq!(call.cwd, PathBuf::from("/"));
        assert_eq!(call.run_as, None);
        assert_eq!(call.timeout, Duration::from_secs(5));
    }
}

/// Catches a keeper not counted: a keeper with no lease directory (the directory removed
/// by an action, or a `leases/` the daemon has not made) is a live lease; each keeper is
/// one, however many times `pgrep` repeats a pid.
///
/// Catches: keepers ignored (the keeper count taken as 0).
#[test]
fn a_keeper_alone_is_a_live_lease() {
    let mut rig = MacHost::new("keeper");
    rig.keepers(said(0, "412\n"));
    assert_eq!(rig.live(), Ok((1, None)));
    let mut rig = MacHost::new("keepers");
    rig.keepers(said(0, "412\n 413\n412\n"));
    assert_eq!(rig.count(), Ok(2));
}

/// Catches lease directories not counted: a directory with no keeper (a lease before its
/// keeper starts, or after its action ended and before the outputs are uploaded) is a live
/// lease while the daemon runs.
///
/// Catches: lease directories ignored.
#[test]
fn a_lease_directory_alone_is_a_live_lease_while_the_daemon_runs() {
    let mut rig = MacHost::new("dir");
    rig.dir("lease-3-7");
    assert_eq!(rig.live(), Ok((1, None)));
}

/// Catches the two signals summed or one of them dropped: a lease shows in both, so the
/// answer is the larger count (one keeper and one directory is one lease; two keepers and one
/// directory is two; one keeper and three directories is three).
///
/// Catches: the two counts summed instead of the larger taken.
#[test]
fn the_answer_is_the_larger_of_the_keepers_and_the_directories() {
    let mut rig = MacHost::new("max-1");
    rig.keepers(said(0, "412\n"));
    rig.dir("lease-3-7");
    assert_eq!(rig.count(), Ok(1));
    let mut rig = MacHost::new("max-2");
    rig.keepers(said(0, "412\n413\n"));
    rig.dir("lease-3-7");
    assert_eq!(rig.count(), Ok(2));
    let mut rig = MacHost::new("max-3");
    rig.keepers(said(0, "412\n"));
    for name in ["lease-3-7", "lease-4-1", "lease-5-9"] {
        rig.dir(name);
    }
    assert_eq!(rig.count(), Ok(3));
}

/// Catches the daemon's own entries counted as leases, which would refuse every activation
/// for good, and a lease whose name is not known passing as none. `lease-warm-up`,
/// `lease-warm-up-<digits>` and `lease-survey` are the daemon's warm-up and survey, and
/// `runs/`, `quarantine/`, `spool/` and `daemon-instance` are not leases: a `leases/` with
/// only those is idle. Every other name that starts `lease-` counts, as a file or a directory
/// and whatever its bytes: `lease-3-7` (a file), `lease-4-1`, a name that only starts like
/// the warm-up's or the survey's, the bare prefix and a name that is not UTF-8.
///
/// Catches: warm-up and survey directories counted as leases; an unknown `lease-` name ignored (only `lease-<n>-<n>` counted).
#[test]
fn warm_up_and_survey_directories_are_not_leases_and_an_unknown_lease_name_is() {
    let mut rig = MacHost::new("names-own");
    for name in [
        "lease-warm-up",
        "lease-warm-up-2",
        "lease-warm-up-17",
        "lease-survey",
        "runs",
        "quarantine",
        "spool",
        "daemon-instance",
        "not-a-lease-3-7",
    ] {
        rig.dir(name);
    }
    assert_eq!(rig.live(), Ok((0, None)));

    let mut rig = MacHost::new("names-leases");
    fs::write(rig.leases().join("lease-3-7"), "").expect("a file");
    for name in [
        "lease-4-1",
        "lease-warm-upX",
        "lease-warm-up-",
        "lease-warm-up-2x",
        "lease-surveyor",
        "lease-survey-2",
        "lease-",
    ] {
        rig.dir(name);
    }
    // A name that is not UTF-8 counts too. APFS refuses such a name (EILSEQ), so on a Mac
    // the case cannot arise and is planted only where the file system takes it.
    #[cfg(target_os = "linux")]
    fs::create_dir(rig.leases().join(OsStr::from_bytes(b"lease-\xff"))).expect("odd name");
    let want = if cfg!(target_os = "linux") { 9 } else { 8 };
    assert_eq!(rig.count(), Ok(want));
}

/// Catches a `pgrep` that did not say "none" read as none. Exit 1 with nothing printed is
/// the only "none"; exit 2 and 3 (bad options, an internal error), no exit code (a signal),
/// a timeout, a program that would not start, output that is not text or not pids, exit 0
/// with no pid, and exit 1 with a pid are all "cannot tell". The same for the keepers and
/// for the daemon: an error asking for the daemon is not "no daemon" (which would let the
/// stale-directory rule through).
///
/// Catches: `pgrep` exit 2 read as none; unparseable output skipped (garbled output read as idle); exit 0 with no pid read as none; a failing daemon `pgrep` read as no daemon.
#[test]
fn a_pgrep_that_did_not_say_none_cannot_tell() {
    let failing: Vec<(&str, Reply, &str)> = vec![
        ("exit 2", said(2, ""), "failed (status 2)"),
        ("exit 3", said(3, "412\n"), "failed (status 3)"),
        ("exit 0, no pid", said(0, ""), "listed no pid"),
        ("exit 0, words", said(0, "no such\n"), "is not a pid"),
        ("exit 0, half garbled", said(0, "412\nx\n"), "is not a pid"),
        ("exit 1, a pid", said(1, "412\n"), "said none matched"),
        (
            "signal",
            Some(Ran {
                code: None,
                ..ran(0, "", "")
            }),
            "ended by a signal",
        ),
        (
            "time",
            Some(Ran {
                timed_out: true,
                ..ran(0, "412\n", "")
            }),
            "ran out of time",
        ),
        (
            "not text",
            Some(Ran {
                stdout: vec![0xff, 0xfe],
                ..ran(0, "", "")
            }),
            "not UTF-8",
        ),
        ("start", None, "/usr/bin/pgrep"),
    ];
    for (name, reply, why) in failing {
        let mut rig = MacHost::new("pgrep-keepers");
        rig.keepers(reply.clone());
        match rig.count() {
            Err(e) => assert!(e.contains(why), "keepers {name}: {e}"),
            Ok(n) => panic!("keepers {name}: answered {n}"),
        }
        let mut rig = MacHost::new("pgrep-daemons");
        rig.dir("lease-3-7");
        rig.daemons(reply);
        match rig.count() {
            Err(e) => assert!(e.contains(why), "daemons {name}: {e}"),
            Ok(n) => panic!("daemons {name}: answered {n}"),
        }
    }
}

/// Catches an output cut at the capture limit read as complete: a keeper pid listed only
/// past the first 64 KiB would be lost, and the host would read idle. Output of exactly
/// `CAPTURE` bytes is refused (even though every line in it is a pid), and one byte less is
/// read.
///
/// Catches: the length check against the capture limit removed (truncated output read as complete).
#[test]
fn output_that_fills_the_capture_buffer_cannot_tell() {
    let lines = |bytes: usize| "1\n".repeat(bytes / 2);
    let cap = usize::try_from(CAPTURE).expect("fits");
    let mut rig = MacHost::new("capture-full");
    rig.keepers(Some(Ran {
        stdout: lines(cap).into_bytes(),
        ..ran(0, "", "")
    }));
    refused(rig.count(), "cut off");
    let mut rig = MacHost::new("capture-short");
    rig.keepers(Some(Ran {
        stdout: lines(cap - 2).into_bytes(),
        ..ran(0, "", "")
    }));
    assert_eq!(rig.count(), Ok(1), "all of the lines are the one pid 1");
}

/// Catches a `ps` listing of the daemon that cannot be relied on passing: it did not start,
/// exited non-zero, timed out, printed no row for a pid that `pgrep` named (the process
/// ended, or the list is cut), two rows for one pid, or a row it cannot read (a missing
/// column, a parent or a time that is no number).
///
/// Catches: a pid with no row accepted; a pid with two rows accepted.
#[test]
fn a_daemon_row_that_cannot_be_read_cannot_tell() {
    let good = row(309, 1, "10:00", DAEMON);
    let failing: Vec<(&str, Reply, &str)> = vec![
        ("start", None, "/bin/ps"),
        ("exit 1", said(1, &good), "failed (status 1)"),
        (
            "time",
            Some(Ran {
                timed_out: true,
                ..ran(0, &good, "")
            }),
            "ran out of time",
        ),
        ("no row", said(0, ""), "did not list pid 309"),
        (
            "other pid",
            said(0, &row(310, 1, "10:00", DAEMON)),
            "did not list pid 309",
        ),
        ("twice", said(0, &format!("{good}{good}")), "twice"),
        (
            "no command",
            said(0, "  309     1      10:00\n"),
            "cannot read",
        ),
        (
            "ppid",
            said(0, &row(309, 1, "10:00", DAEMON).replacen(" 1 ", " x ", 1)),
            "cannot read",
        ),
        ("etime", said(0, &row(309, 1, "10", DAEMON)), "cannot read"),
    ];
    for (name, reply, why) in failing {
        let mut rig = MacHost::new("ps-row");
        rig.ps(reply);
        match rig.count() {
            Err(e) => assert!(e.contains(why), "{name}: {e}"),
            Ok(n) => panic!("{name}: answered {n}"),
        }
    }
}

/// Catches a command with a line break in its arguments breaking or forging the listing:
/// continuation lines (which do not start with a listed pid) are ignored, a continuation line
/// that starts with a pid `pgrep` did not name is ignored, and the daemon's own row, wherever
/// it falls, is found.
///
/// Catches: the parent-is-launchd filter dropped (the continuation test then finds two daemons).
#[test]
fn continuation_lines_of_a_multi_line_command_are_ignored() {
    let mut rig = MacHost::new("continuation");
    rig.daemons(said(0, "309\n412\n"));
    let text = format!(
        "{}  not a row\n  999     1      00:01 kbf --mode=daemon --config /elsewhere\n{}",
        row(412, 400, "00:05", "kbf --mode=daemon -x\n"),
        row(309, 1, "10:00", DAEMON)
    );
    rig.ps(said(0, &text));
    assert_eq!(rig.live(), Ok((0, None)));
}

/// Catches the daemon taken for a process that merely looks like it: of the processes whose
/// command line has `--mode=daemon`, only the one started by launchd (parent 1) is the daemon.
/// One that an action started (here running as pid 412 under another parent) does not make
/// the check refuse; none under launchd, or two, cannot tell, and the refusal says to run the
/// daemon only as its launchd job.
///
/// Catches: the parent-is-launchd filter dropped; the launchd-job hint dropped from the
/// refusal.
#[test]
fn the_daemon_is_the_one_process_launchd_started() {
    let mut rig = MacHost::new("launchd-one");
    rig.daemons(said(0, "309\n412\n"));
    let text = format!(
        "{}{}",
        row(309, 1, "10:00", DAEMON),
        row(
            412,
            400,
            "00:05",
            "kbf --mode=daemon --config /scratch/k.toml"
        )
    );
    rig.ps(said(0, &text));
    assert_eq!(rig.live(), Ok((0, None)));

    let mut rig = MacHost::new("launchd-none");
    rig.daemon(400, "10:00", DAEMON);
    let answer = rig.count();
    refused(answer.clone(), "0 processes started by launchd");
    refused(answer, "launchctl kickstart -k system/");

    let mut rig = MacHost::new("launchd-two");
    rig.daemons(said(0, "309\n310\n"));
    let text = format!(
        "{}{}",
        row(309, 1, "10:00", DAEMON),
        row(310, 1, "10:00", DAEMON)
    );
    rig.ps(said(0, &text));
    refused(rig.count(), "2 processes started by launchd");
}

/// Catches the daemon's settings taken from a file it was not started with: the daemon
/// reads its configuration once, so if its command line is anything but
/// `kbf --mode=daemon --config <this host's kbf.toml>` (another config file, a flag that
/// overrides a key, a binary that is not `kbf`, a different mode) the `data-dir` read here may
/// not be the one it uses, and the check cannot tell.
///
/// Catches: the command-line check dropped (H2).
#[test]
fn a_daemon_not_started_as_kbf_toml_says_cannot_tell() {
    let cases = [
        "/usr/local/bin/kbf --mode=daemon --config /elsewhere/kbf.toml",
        "/usr/local/bin/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml --data-dir /x",
        "/usr/local/bin/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml --pool-users 4",
        "/usr/local/bin/kbf --mode=daemon",
        "/usr/local/bin/kbf --config /usr/local/etc/kbf/kbf.toml --mode=daemon",
        "/usr/local/bin/kbf --mode=daemon --config=/usr/local/etc/kbf/kbf.toml",
        "/usr/local/bin/not-kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml",
        "/usr/local/bin/kbf-x --mode=daemon --config /usr/local/etc/kbf/kbf.toml",
    ];
    for command in cases {
        let mut rig = MacHost::new("argv");
        rig.daemon(1, "10:00", command);
        refused(rig.count(), "was started as");
    }
    for command in [
        "kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml",
        "/a/b/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml",
    ] {
        let mut rig = MacHost::new("argv-ok");
        rig.daemon(1, "10:00", command);
        assert_eq!(rig.count(), Ok(0), "{command}");
    }
}

/// Catches a daemon that started before `kbf.toml` last changed passing: it still uses the
/// old `data-dir`, so the directory read here may be the wrong one, and the lease it holds
/// would be invisible. The daemon must have started at least 2 s after the file's last
/// change (the elapsed time and the file's time are both whole seconds): the clock is 1000 s
/// after the change, so 16:38 (998 s) is allowed, 16:39 (997 s: the daemon started 1 s after
/// the change) and anything older is not. A daemon older than a day is read in days.
///
/// Catches: the start-time check dropped (H2); the 2 s slack set to 0; the days of an
/// elapsed time dropped; the restart hint dropped from the refusal.
#[test]
fn a_daemon_that_started_before_kbf_toml_changed_cannot_tell() {
    for (etime, ok) in [
        ("16:38", true),
        ("16:39", false),
        ("16:40", false),
        ("17:00", false),
        ("1:00:00", false),
        ("1-00:00:00", false),
        ("00:00", true),
    ] {
        let mut rig = MacHost::new("start-time");
        rig.daemon(1, etime, DAEMON);
        match rig.count() {
            Ok(0) => assert!(ok, "{etime} is too old"),
            Err(e) => {
                assert!(!ok, "{etime}: {e}");
                assert!(e.contains("changed after the daemon"), "{e}");
                assert!(e.contains("launchctl kickstart -k system/"), "{e}");
            }
            other => panic!("{etime}: {other:?}"),
        }
    }
}

/// Catches the file's status-change time (`ctime`) ignored by the start-time check (H2): a
/// `chmod`, `chown` or a rename into place changes the file without changing its content time,
/// and a daemon that started before that may have read an older file. Here the content time is
/// long past and only the status-change time is recent: a daemon that started 200 s before it
/// cannot be told to use this `kbf.toml`, one that started 995 s after it can.
///
/// Catches: the later of the two times replaced by the modification time alone; the
/// restart hint dropped from the refusal.
#[test]
fn a_kbf_toml_whose_status_changed_after_the_daemon_started_cannot_tell() {
    for (etime, ok) in [("20:00", false), ("00:05", true)] {
        let mut rig = MacHost::new("ctime");
        let file = fs::File::options()
            .write(true)
            .open(rig.root.join("usr/local/etc/kbf/kbf.toml"))
            .expect("open");
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1000))
            .expect("an old content time");
        let meta = fs::metadata(rig.root.join("usr/local/etc/kbf/kbf.toml")).expect("meta");
        assert!(meta.ctime() > meta.mtime() + 1_000_000, "ctime is recent");
        rig.now = meta.ctime() + 1000;
        rig.daemon(1, etime, DAEMON);
        match rig.count() {
            Ok(0) => assert!(ok, "{etime}: started before the status change"),
            Err(e) => {
                assert!(!ok, "{etime}: {e}");
                assert!(e.contains("changed after the daemon"), "{e}");
                assert!(e.contains("launchctl kickstart -k system/"), "{e}");
            }
            other => panic!("{etime}: {other:?}"),
        }
    }
}

/// Catches the stale-directory rule (F1) letting through what it must not. Lease directories
/// remain and no keeper exists:
/// - with the daemon positively not running the check allows (count 0), the note names each
///   directory by its host path and says the daemon's start-up sweep removes it;
/// - with the daemon running they are live leases (a refusal);
/// - with a keeper they are live leases even though no daemon runs;
/// - when `pgrep` cannot say whether the daemon runs, the check cannot tell.
///
/// With no directory there is nothing to note.
///
/// Catches: the stale-directory rule allowing while the daemon IS running; a failing daemon `pgrep` read as no daemon; `pgrep` exit 2 read as none; the two counts summed; the note naming no directory.
#[test]
fn stale_directories_are_allowed_only_when_the_daemon_is_positively_not_running() {
    let mut rig = MacHost::new("f1-stale");
    rig.no_daemon();
    rig.dir("lease-3-7");
    rig.dir("lease-4-1");
    rig.dir("lease-survey");
    let (live, note) = rig.live().expect("allowed");
    assert_eq!(live, 0);
    assert_eq!(
        note.as_deref(),
        Some(
            "/Volumes/kbf/data/leases/lease-3-7, /Volumes/kbf/data/leases/lease-4-1 remains and no kbf daemon is running; the daemon's start-up sweep removes it"
        )
    );

    let mut rig = MacHost::new("f1-running");
    rig.dir("lease-3-7");
    rig.dir("lease-4-1");
    assert_eq!(rig.live(), Ok((2, None)));

    let mut rig = MacHost::new("f1-keeper");
    rig.no_daemon();
    rig.dir("lease-3-7");
    rig.keepers(said(0, "412\n"));
    assert_eq!(rig.live(), Ok((1, None)));

    let mut rig = MacHost::new("f1-unknown");
    rig.dir("lease-3-7");
    rig.daemons(said(2, ""));
    refused(rig.count(), "failed (status 2)");

    let mut rig = MacHost::new("f1-nothing");
    rig.no_daemon();
    assert_eq!(rig.live(), Ok((0, None)));
}

/// Catches a Mac without the sandbox (H5) passing: without `/usr/bin/sandbox-exec` the
/// daemon applies no isolation, so an action could remove its own lease directory and kill
/// its keeper, and neither signal can be trusted. Missing, a directory in its place and a
/// link to nothing cannot tell; a link to a file is a file (the daemon tests it the same way).
///
/// Catches: the `sandbox-exec` check dropped (H5).
#[test]
fn a_mac_without_sandbox_exec_cannot_tell() {
    let at = |rig: &MacHost| rig.root.join("usr/bin/sandbox-exec");
    let mut rig = MacHost::new("sandbox-missing");
    fs::remove_file(at(&rig)).expect("remove");
    refused(rig.count(), "/usr/bin/sandbox-exec is not a file");

    let mut rig = MacHost::new("sandbox-dir");
    fs::remove_file(at(&rig)).expect("remove");
    fs::create_dir(at(&rig)).expect("dir");
    refused(rig.count(), "is not a file");

    let mut rig = MacHost::new("sandbox-dangling");
    fs::remove_file(at(&rig)).expect("remove");
    symlink("nowhere", at(&rig)).expect("link");
    refused(rig.count(), "is not a file");

    let mut rig = MacHost::new("sandbox-link");
    fs::rename(at(&rig), rig.root.join("usr/bin/real")).expect("move");
    symlink("real", at(&rig)).expect("link");
    assert_eq!(rig.count(), Ok(0));
}

/// Catches an unreliable `kbf.toml` passing. Each of these cannot tell, naming the file or
/// the key: absent, a link or a directory where the file should be, not root's, group
/// writable, in a directory others can write, over 1 MiB, not text, not TOML, a mode that is
/// not `daemon`, no mode, a `driver` that is absent, not `["native"]` or holds more, no
/// `data-dir` or a relative one, a pool key (not read by this source), and a file that is
/// there for the trust check but gone for the timestamp read.
///
/// Catches: the trust check dropped; the driver check dropped; the pool-key check dropped; the mode check dropped.
#[test]
fn a_kbf_toml_that_is_not_a_native_daemons_or_not_trusted_cannot_tell() {
    let toml_cases: Vec<(&str, String, &str)> = vec![
        (
            "mode",
            TOML.replace("mode = \"daemon\"", "mode = \"frontend\""),
            "mode is \"frontend\"",
        ),
        (
            "no mode",
            TOML.replace("mode = \"daemon\"\n", ""),
            "has no mode",
        ),
        (
            "no driver",
            TOML.replace("driver = [\"native\"]\n", ""),
            "driver is None",
        ),
        (
            "two drivers",
            TOML.replace("[\"native\"]", "[\"native\", \"native-whole-machine\"]"),
            "not [\"native\"]",
        ),
        (
            "other driver",
            TOML.replace("native", "container"),
            "not [\"native\"]",
        ),
        (
            "no data-dir",
            TOML.replace("data-dir = \"/Volumes/kbf/data\"\n", ""),
            "no absolute data-dir",
        ),
        (
            "relative",
            TOML.replace("/Volumes/kbf/data", "data"),
            "no absolute data-dir",
        ),
        (
            "not a string",
            TOML.replace("\"/Volumes/kbf/data\"", "7"),
            "no absolute data-dir",
        ),
        (
            "pool users",
            format!("{TOML}[macos]\npool-users = 4\n"),
            "sets a pool key",
        ),
        (
            "pool uid",
            format!("{TOML}[macos]\npool-first-uid = 600\n"),
            "sets a pool key",
        ),
        ("not toml", "mode = = \n".to_owned(), "is not TOML"),
        (
            "big",
            format!("{TOML}#{}\n", "x".repeat(1 << 20)),
            "is over",
        ),
    ];
    for (name, text, why) in toml_cases {
        let mut rig = MacHost::new("toml");
        rig.toml(&text);
        match rig.count() {
            Err(e) => assert!(e.contains(why), "{name}: {e}"),
            Ok(n) => panic!("{name}: answered {n}"),
        }
    }

    let file = |perms, uid, bytes: &[u8]| Entry::File {
        perms: Perms::new(perms),
        owner: Owner { uid, gid: 0 },
        bytes: bytes.to_vec(),
    };
    let entry_cases: Vec<(&str, Option<Entry>, &str)> = vec![
        ("absent", None, "is not there"),
        (
            "not utf-8",
            Some(file(0o644, 0, &[0xff, 0xfe])),
            "not UTF-8",
        ),
        (
            "a link",
            Some(Entry::Link { target: "x".into() }),
            "not a plain file",
        ),
        (
            "a directory",
            Some(Entry::Dir {
                perms: Perms::new(0o755),
                owner: root_owned(),
            }),
            "not a plain file",
        ),
        (
            "not root's",
            Some(file(0o644, 501, TOML.as_bytes())),
            "not trusted",
        ),
        (
            "group writable",
            Some(file(0o664, 0, TOML.as_bytes())),
            "not trusted",
        ),
    ];
    for (name, entry, why) in entry_cases {
        let mut rig = MacHost::new("entry");
        match entry {
            Some(entry) => {
                rig.exec
                    .entries
                    .insert(HostPath::new(KBF_TOML).expect("path"), entry);
            }
            None => {
                rig.exec
                    .entries
                    .remove(&HostPath::new(KBF_TOML).expect("path"));
            }
        }
        match rig.count() {
            Err(e) => assert!(e.contains(why), "{name}: {e}"),
            Ok(n) => panic!("{name}: answered {n}"),
        }
    }

    let mut rig = MacHost::new("entry-dir-writable");
    rig.exec.entries.insert(
        HostPath::new("/usr/local/etc").expect("path"),
        Entry::Dir {
            perms: Perms::new(0o775),
            owner: root_owned(),
        },
    );
    refused(rig.count(), "not trusted");

    let mut rig = MacHost::new("unreadable");
    rig.exec.entries.insert(
        HostPath::new("/usr/local/etc/kbf/kbf.toml").expect("path"),
        file(0o644, 0, TOML.as_bytes()),
    );
    fs::remove_file(rig.root.join("usr/local/etc/kbf/kbf.toml")).expect("remove");
    refused(rig.count(), "kbf.toml");
}

/// Asserts that a `[macos]` table without a pool key is read like a file without the table (an
/// idle daemon host counts no lease), and that a pool key given beside other keys of the table
/// is still refused as a pool key.
#[test]
fn a_macos_table_without_a_pool_key_does_not_make_the_file_unreadable() {
    let mut rig = MacHost::new("macos-table");
    rig.toml(&format!("{TOML}[macos]\nsandbox = true\n"));
    assert_eq!(rig.live(), Ok((0, None)));
    let mut rig = MacHost::new("macos-table-pool");
    rig.toml(&format!("{TOML}[macos]\nsandbox = true\npool-users = 4\n"));
    refused(rig.count(), "sets a pool key");
}

/// Catches a `leases/` that cannot be listed read as empty: missing, a file, or a link
/// (even to an empty directory) cannot tell, naming the directory. The listing's own errors
/// are failures too, except a name that vanished from the listing, which is a lease that
/// ended and is not one.
///
/// Catches: a symlinked `leases/` followed.
#[test]
fn a_leases_directory_that_is_not_a_plain_directory_cannot_tell() {
    let leases = |rig: &MacHost| rig.leases();
    let mut rig = MacHost::new("leases-missing");
    fs::remove_dir(leases(&rig)).expect("remove");
    refused(rig.count(), "/Volumes/kbf/data/leases");

    let mut rig = MacHost::new("leases-file");
    fs::remove_dir(leases(&rig)).expect("remove");
    fs::write(leases(&rig), "").expect("file");
    refused(rig.count(), "is not a directory");

    let mut rig = MacHost::new("leases-link");
    fs::remove_dir(leases(&rig)).expect("remove");
    fs::create_dir(rig.root.join("elsewhere")).expect("dir");
    symlink(rig.root.join("elsewhere"), leases(&rig)).expect("link");
    refused(rig.count(), "is a link");

    let names =
        |items: Vec<io::Result<&str>>| names_of(items.into_iter().map(|i| i.map(Into::into)), "/d");
    assert_eq!(
        names(vec![
            Ok("lease-2-1"),
            Err(io::ErrorKind::NotFound.into()),
            Ok("lease-1-1"),
            Ok("runs")
        ]),
        Ok(vec!["lease-1-1".into(), "lease-2-1".into()])
    );
    let err = names(vec![
        Ok("lease-1-1"),
        Err(io::ErrorKind::PermissionDenied.into()),
    ])
    .expect_err("a listing error");
    assert!(err.starts_with("/d: "), "{err}");
}

/// Catches a check run by an account that cannot see every process's arguments: `pgrep -f`
/// shows another account's arguments to root only, and for anyone else it would match
/// nothing and read idle.
///
/// Catches: the root check dropped.
#[test]
fn a_check_not_run_as_root_cannot_tell() {
    let mut rig = MacHost::new("not-root");
    rig.exec.euid = 501;
    refused(rig.count(), "not root");
    assert!(rig.calls().is_empty(), "nothing is run first");
}

/// Catches the directories read before the processes (H4): a lease that starts while the
/// check runs (here, one whose directory appears as the first, or as the second, `pgrep`
/// starts) has made its directory by the time the directories are read last, so it counts;
/// read first, the directory would be missed and its keeper not yet started.
///
/// Catches: the directories read before the processes (H4); the directories ignored.
#[test]
fn a_lease_that_starts_during_the_check_is_seen() {
    for during in [KEEPER_PATTERN, DAEMON_PATTERN] {
        let mut rig = MacHost::new("order");
        let leases = rig.leases();
        rig.hook(move |spawn| {
            if spawn.args.get(1).is_some_and(|a| a == during) {
                fs::create_dir_all(leases.join("lease-9-9")).expect("lease starts");
            }
        });
        assert_eq!(rig.count(), Ok(1), "{during}");
    }
}

/// Catches `ps` columns parsed wrongly: padded columns, a day count, hours and a command
/// with spaces are read; a line that starts with a pid not asked for, a blank line and a
/// header are skipped.
///
/// Catches: the days of an elapsed time dropped.
#[test]
fn rows_are_read_from_padded_columns() {
    let text = "  PID  PPID ELAPSED COMMAND\n\n   42     1 2-03:04:05 /bin/x  a  b \n    7     1    1:02:03 y\n   99     1    00:01 z\n";
    let rows = rows_of(text, &[42, 7]).expect("rows");
    assert_eq!(
        rows,
        vec![
            Row {
                pid: 42,
                ppid: 1,
                elapsed: 183_845,
                command: "/bin/x  a  b".into()
            },
            Row {
                pid: 7,
                ppid: 1,
                elapsed: 3723,
                command: "y".into()
            },
        ]
    );
}

/// Catches `etime` read wrongly: `mm:ss`, `hh:mm:ss` and `dd-hh:mm:ss` are read; an empty
/// text, a bare number, too many fields, a sign, letters, an empty field and a number too
/// long to be a time are not.
///
/// Catches: the days of an elapsed time dropped.
#[test]
fn elapsed_time_is_read_as_ps_prints_it() {
    assert_eq!(etime_secs("05:03"), Some(303));
    assert_eq!(etime_secs("1:02:03"), Some(3723));
    assert_eq!(etime_secs("2-03:04:05"), Some(183_845));
    assert_eq!(etime_secs("00:00"), Some(0));
    for bad in [
        "",
        "5",
        "1:2:3:4",
        "a:b",
        "-1:00",
        "1-",
        "-",
        "+1:00",
        "1::2",
        ":",
        "1:-2",
        "12345678901:00",
        "x-01:00:00",
    ] {
        assert_eq!(etime_secs(bad), None, "{bad:?}");
    }
}

/// Catches the real clock returning nothing useful (0 would make every daemon look as if it
/// started before `kbf.toml` last changed): it reads the present.
#[test]
fn the_clock_reads_the_present() {
    let now = unix_now();
    assert!(now > 1_700_000_000, "{now}");
}

/// Catches `on_host` building a source that does not read the host it is given: its trust
/// is a real executor under the root and its runner starts real programs, so on a Linux
/// scratch root with no `sandbox-exec` it cannot tell, naming the file.
///
/// Catches: the `sandbox-exec` check dropped.
#[test]
fn the_host_source_reads_the_host_root_it_is_given() {
    let root = scratch_dir("mac-leases-on-host");
    let layout = Layout::new(Root::new(&root).expect("absolute"), Platform::MacOs);
    let err = MacLeases::on_host(layout).live().expect_err("no sandbox");
    assert!(err.contains("/usr/bin/sandbox-exec"), "{err}");
}
