//! [`FakeSession`]: a [`VmSession`] with no helper and no Mac. A "run" is a thread that
//! plays `kbf-vmm vm-run` on the descriptors it is given (as the helper would hand them
//! on): it prints its scripted events on stdout, reads stdin, and ends at end of file or
//! at a `shutdown` line, as `vm-run` does. Every call is logged.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use kbf_driver_native::Record;
use kbf_vmm::spec::Event;

use crate::mac::session::{RunExit, RunFds, VmSession};

/// The first pid a run gets.
pub const FIRST_PID: u32 = 4000;

#[derive(Default)]
struct State {
    calls: Vec<String>,
    ready: bool,
    boot: String,
    /// The process table: pid to start time.
    table: BTreeMap<i32, u64>,
    next_pid: u32,
    fail: BTreeSet<&'static str>,
    /// Runs ignore the end of their stdin; only `reap` (or `kill`) ends them.
    stubborn: bool,
    /// Whether `reap` ends a run.
    reap_kills: bool,
    /// Events a run prints as it starts.
    script: Vec<Event>,
    kills: BTreeMap<u32, Sender<()>>,
    /// Pids whose stdin reached the end.
    eof: BTreeSet<u32>,
    reaped: Vec<Record>,
    argvs: Vec<Vec<String>>,
    /// What the runs directory held when `run` was called.
    seen_at_run: Vec<Vec<String>>,
    runs_dir: Option<PathBuf>,
    wait_error: bool,
    connectable: bool,
    /// `process_start` finds nothing: the process was gone when the host looked.
    hidden: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// See the module documentation. Clones share their state.
#[derive(Clone)]
pub struct FakeSession(Arc<Shared>);

/// A fake run.
pub struct FakeRunning {
    pub pid: u32,
    exit: Receiver<RunExit>,
}

impl FakeSession {
    pub fn new() -> Self {
        let state = State {
            ready: true,
            boot: "boot-1".to_owned(),
            next_pid: FIRST_PID,
            reap_kills: true,
            script: vec![Event::Configured { ms: 1 }, Event::Started { ms: 2 }],
            ..State::default()
        };
        Self(Arc::new(Shared {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn call(&self, name: &'static str, text: String) -> io::Result<()> {
        let mut state = self.lock();
        state.calls.push(text);
        if state.fail.contains(name) {
            return Err(io::Error::other(format!("{name} failed")));
        }
        Ok(())
    }

    /// Every call so far, as text; the log is not cleared.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    /// How many calls of kind `name` (the first word of the log line) were made.
    pub fn count(&self, name: &str) -> usize {
        self.lock()
            .calls
            .iter()
            .filter(|call| call.split(' ').next() == Some(name))
            .count()
    }

    pub fn set_ready(&self, ready: bool) {
        self.lock().ready = ready;
    }

    pub fn fail(&self, call: &'static str) {
        self.lock().fail.insert(call);
    }

    pub fn heal(&self) {
        self.lock().fail.clear();
    }

    pub fn set_boot(&self, boot: &str) {
        self.lock().boot = boot.to_owned();
    }

    pub fn script(&self, events: Vec<Event>) {
        self.lock().script = events;
    }

    pub fn set_stubborn(&self, stubborn: bool) {
        self.lock().stubborn = stubborn;
    }

    pub fn set_reap_kills(&self, kills: bool) {
        self.lock().reap_kills = kills;
    }

    pub fn set_wait_error(&self, error: bool) {
        self.lock().wait_error = error;
    }

    pub fn hide_from_table(&self) {
        self.lock().hidden = true;
    }

    pub fn set_connectable(&self, connectable: bool) {
        self.lock().connectable = connectable;
    }

    pub fn watch_runs_dir(&self, dir: &Path) {
        self.lock().runs_dir = Some(dir.to_owned());
    }

    /// The names the runs directory held at each `run` call.
    pub fn seen_at_run(&self) -> Vec<Vec<String>> {
        self.lock().seen_at_run.clone()
    }

    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.lock().argvs.clone()
    }

    pub fn reaped(&self) -> Vec<Record> {
        self.lock().reaped.clone()
    }

    /// Puts a process in the table, as one an earlier daemon's run left.
    pub fn leave_process(&self, pid: i32, start: u64) {
        self.lock().table.insert(pid, start);
    }

    pub fn alive(&self, pid: i32) -> bool {
        self.lock().table.contains_key(&pid)
    }

    /// Ends the run `pid` as a kill would.
    pub fn kill(&self, pid: u32) {
        if let Some(kill) = self.lock().kills.get(&pid) {
            let _ = kill.send(());
        }
    }

    /// Blocks until the stdin of run `pid` has reached its end.
    pub fn wait_eof(&self, pid: u32) {
        let mut state = self.lock();
        while !state.eof.contains(&pid) {
            state = self
                .0
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub fn saw_eof(&self, pid: u32) -> bool {
        self.lock().eof.contains(&pid)
    }
}

/// What a run does with its descriptors: `vm-run`.
fn vm_run(
    shared: &Arc<Shared>,
    pid: u32,
    stdin: File,
    mut stdout: File,
    kill: &Receiver<()>,
) -> RunExit {
    let (events, stubborn) = {
        let state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.script.clone(), state.stubborn)
    };
    for event in events {
        let _ = writeln!(stdout, "{}", event.to_line());
    }
    let mut input = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.eof.insert(pid);
            shared.changed.notify_all();
            drop(state);
            if stubborn {
                let _ = kill.recv();
                return RunExit {
                    code: None,
                    signal: Some(9),
                };
            }
            let _ = writeln!(stdout, "{}", Event::Stopped { ms: 3 }.to_line());
            return RunExit {
                code: Some(0),
                signal: None,
            };
        }
        if line.trim_end() == "shutdown" {
            let _ = writeln!(stdout, "{}", Event::Stopped { ms: 3 }.to_line());
            return RunExit {
                code: Some(0),
                signal: None,
            };
        }
    }
}

impl VmSession for FakeSession {
    type Running = FakeRunning;
    type Stream = UnixStream;

    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()> {
        self.call("clone_image", format!("clone_image {}", clone.display()))?;
        crate::fake::copy_dir(image, clone)
    }

    fn session_ready(&self) -> io::Result<bool> {
        self.call("session_ready", "session_ready".to_owned())?;
        Ok(self.lock().ready)
    }

    fn run(&self, argv: &[String], fds: RunFds<'_>) -> io::Result<FakeRunning> {
        // What the real helper does first: its own copies of the descriptors.
        let stdin = File::from(fds.stdin.try_clone_to_owned()?);
        let stdout = File::from(fds.stdout.try_clone_to_owned()?);
        let mut stderr = File::from(fds.stderr.try_clone_to_owned()?);
        writeln!(stderr, "vm-run starts")?;
        let dir = self.lock().runs_dir.clone();
        if let Some(dir) = dir {
            let mut names: Vec<String> = std::fs::read_dir(dir)?
                .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<io::Result<_>>()?;
            names.sort();
            self.lock().seen_at_run.push(names);
        }
        self.lock().argvs.push(argv.to_vec());
        self.call("run", format!("run {}", argv.join(" ")))?;
        let (pid, wait_error) = {
            let mut state = self.lock();
            let pid = state.next_pid;
            state.next_pid += 1;
            let start = 1_000_000 + u64::from(pid);
            state.table.insert(i32::try_from(pid).unwrap(), start);
            (pid, state.wait_error)
        };
        let (kill, killed) = mpsc::channel();
        let (done, exit) = mpsc::channel();
        self.lock().kills.insert(pid, kill);
        let shared = Arc::clone(&self.0);
        std::thread::spawn(move || {
            let how = vm_run(&shared, pid, stdin, stdout, &killed);
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.table.remove(&i32::try_from(pid).unwrap());
            drop(state);
            if !wait_error {
                let _ = done.send(how);
            }
            // With a wait error the sender is dropped, so `wait` fails.
        });
        Ok(FakeRunning { pid, exit })
    }

    fn pid(&self, running: &FakeRunning) -> u32 {
        running.pid
    }

    fn wait(&self, running: FakeRunning) -> io::Result<RunExit> {
        running
            .exit
            .recv()
            .map_err(|_| io::Error::other("the helper hung up"))
    }

    fn reap(&self, record: &Record) -> io::Result<()> {
        self.call("reap", format!("reap {}", record.pgid))?;
        let mut state = self.lock();
        state.reaped.push(record.clone());
        if state.reap_kills {
            // The real helper kills the group; here the run is a thread, or only an entry.
            state.table.remove(&record.pgid);
            if let Some(kill) = state.kills.get(&u32::try_from(record.pgid).unwrap()) {
                let _ = kill.send(());
            }
        }
        Ok(())
    }

    fn boot_id(&self) -> io::Result<String> {
        self.call("boot_id", "boot_id".to_owned())?;
        Ok(self.lock().boot.clone())
    }

    fn process_start(&self, pid: i32) -> io::Result<Option<u64>> {
        self.call("process_start", format!("process_start {pid}"))?;
        let state = self.lock();
        Ok(if state.hidden {
            None
        } else {
            state.table.get(&pid).copied()
        })
    }

    fn connect(&self, name: &str) -> io::Result<UnixStream> {
        self.call("connect", format!("connect {name}"))?;
        if !self.lock().connectable {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "no relay"));
        }
        let (ours, mut theirs) = UnixStream::pair()?;
        // The guest end echoes what it is sent, upper-cased, until the stream ends.
        std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            while theirs.read(&mut byte).unwrap_or(0) == 1 {
                if theirs.write_all(&byte.to_ascii_uppercase()).is_err() {
                    return;
                }
            }
        });
        Ok(ours)
    }
}
