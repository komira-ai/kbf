//! A fake [`System`] for unit tests: files are really copied (in a temp directory), VM
//! helpers are scripted.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

use crate::bundle::FILES;
use crate::spec::{Event, LIMIT_EXCEEDED, VmSpec};
use crate::system::{OfflineFile, Poll, Signature, System, VmProcess};

/// What one scripted helper does.
#[derive(Clone)]
pub struct Script {
    /// Answers to `poll`, in order; then `Quiet`.
    pub events: Vec<Poll>,
    /// Files the "guest" delivers as soon as the helper starts: over vsock into the spec's
    /// results directory when it has one (as a real guest does), else into
    /// `<automount>/results/`.
    pub guest_files: Vec<(String, String)>,
    /// Files the "guest" writes into the share's `results/` directory whatever the spec
    /// says (the fallback path, or a guest faking a frame).
    pub share_files: Vec<(String, String)>,
    /// Files delivered like `guest_files` but only once the helper has been polled this many
    /// times: a guest whose frames arrive one after another.
    pub late_files: Vec<(u32, String, String)>,
    /// Whether the helper exits after `request_stop`.
    pub exits_on_stop: bool,
    /// Whether the guest obeys a shutdown request (the helper then exits by itself).
    pub obeys_shutdown: bool,
    /// What the helper reports about its vsock `shutdown` command when asked to shut the
    /// guest down: whether the guest acked, and why not. `None`: nothing is reported.
    pub shutdown_command: Option<(bool, String)>,
    /// What the helper's log tail says.
    pub log: Option<String>,
}

impl Script {
    /// A VM that starts and keeps running.
    pub fn booted() -> Self {
        Self {
            events: vec![Poll::Event(Event::Started { ms: 100 })],
            guest_files: Vec::new(),
            share_files: Vec::new(),
            late_files: Vec::new(),
            exits_on_stop: true,
            obeys_shutdown: true,
            shutdown_command: None,
            log: None,
        }
    }

    /// A start refused with the guest-limit error.
    pub fn limit() -> Self {
        Self::failing("limit", Some(LIMIT_EXCEEDED))
    }

    /// A start that fails with `message` and, maybe, a code.
    pub fn failing(message: &str, code: Option<&str>) -> Self {
        Self {
            events: vec![Poll::Event(Event::Error {
                stage: "start".to_owned(),
                message: message.to_owned(),
                code: code.map(str::to_owned),
            })],
            guest_files: Vec::new(),
            share_files: Vec::new(),
            late_files: Vec::new(),
            exits_on_stop: true,
            obeys_shutdown: true,
            shutdown_command: None,
            log: None,
        }
    }

    /// Adds a file the guest writes.
    pub fn with_guest_file(mut self, name: &str, body: &str) -> Self {
        self.guest_files.push((name.to_owned(), body.to_owned()));
        self
    }

    /// Adds a file the guest writes into the share's `results/` directory itself.
    pub fn with_share_file(mut self, name: &str, body: &str) -> Self {
        self.share_files.push((name.to_owned(), body.to_owned()));
        self
    }

    /// Adds a frame the guest delivers after the helper has been polled `polls` times.
    pub fn delivering_after(mut self, polls: u32, name: &str, body: &str) -> Self {
        self.late_files
            .push((polls, name.to_owned(), body.to_owned()));
        self
    }

    /// A guest whose daemon acks the vsock `shutdown` command (and then, unless it is
    /// also [`deaf`](Self::deaf), shuts down).
    pub fn acking_shutdown(mut self) -> Self {
        self.shutdown_command = Some((true, "acked".to_owned()));
        self
    }

    /// A guest whose daemon does not answer the vsock command (`why` is the helper's
    /// reason); the helper falls back to the stop request.
    pub fn unreachable_by_vsock(mut self, why: &str) -> Self {
        self.shutdown_command = Some((false, why.to_owned()));
        self
    }

    /// A guest that ignores a shutdown request.
    pub fn deaf(mut self) -> Self {
        self.obeys_shutdown = false;
        self
    }

    /// Sets the helper's log tail.
    pub fn with_log(mut self, text: &str) -> Self {
        self.log = Some(text.to_owned());
        self
    }

    /// Adds an event after the existing ones.
    pub fn then(mut self, p: Poll) -> Self {
        self.events.push(p);
        self
    }
}

pub struct FakeProc {
    pid: u32,
    events: VecDeque<Poll>,
    /// Frames still to arrive: the poll count they are due at, where, and what.
    late: Vec<(u32, PathBuf, String)>,
    polls: u32,
    killed: bool,
    stop_asked: bool,
    shutdown_asked: bool,
    exits_on_stop: bool,
    obeys_shutdown: bool,
    shutdown_command: Option<(bool, String)>,
    log: Option<String>,
}

impl VmProcess for FakeProc {
    fn diagnostics(&self) -> Option<String> {
        self.log.clone()
    }
    fn pid(&self) -> u32 {
        self.pid
    }
    fn poll(&mut self, wait: Duration) -> Poll {
        self.polls += 1;
        let polls = self.polls;
        self.late.retain(|(due, path, body)| {
            let arrived = polls >= *due;
            if arrived {
                std::fs::write(path, body).expect("late guest file");
            }
            !arrived
        });
        if self.killed {
            return Poll::Gone {
                code: None,
                signal: Some(9),
            };
        }
        self.events.pop_front().unwrap_or_else(|| {
            std::thread::sleep(wait.min(Duration::from_millis(1)));
            Poll::Quiet
        })
    }
    fn kill9(&mut self) {
        self.killed = true;
    }
    fn request_shutdown(&mut self) {
        self.shutdown_asked = true;
        if let Some((acked, detail)) = self.shutdown_command.take() {
            self.events.push_back(Poll::Event(Event::ShutdownCommand {
                acked,
                detail,
                ms: 1,
            }));
        }
    }
    fn request_stop(&mut self) {
        self.stop_asked = true;
    }
    fn wait_gone(&mut self, _wait: Duration) -> bool {
        self.killed
            || (self.shutdown_asked && self.obeys_shutdown)
            || (self.stop_asked && self.exits_on_stop)
    }
}

/// A hook run on each clone's destination.
pub type CloneHook = Box<dyn Fn(&Path)>;

pub struct FakeSystem {
    pub scripts: RefCell<VecDeque<Script>>,
    pub starts: RefCell<Vec<(VmSpec, Option<u32>)>>,
    pub start_error: RefCell<Option<String>>,
    pub clone_fails_on: RefCell<Option<&'static str>>,
    /// How many more `clonefile` calls succeed (`None`: all).
    pub clone_budget: Cell<Option<usize>>,
    /// Runs after each successful clone with the destination path.
    pub after_clone: RefCell<Option<CloneHook>>,
    pub cloned: RefCell<Vec<(PathBuf, PathBuf)>>,
    pub identity: RefCell<Result<Vec<u8>, String>>,
    pub uid: Cell<u32>,
    pub ppid: Cell<u32>,
    pub context: RefCell<Option<String>>,
    pub console_uid: Cell<Option<u32>>,
    pub purge: RefCell<Result<(), String>>,
    pub purges: Cell<u32>,
    pub sig: RefCell<Result<Signature, String>>,
    pub offline: RefCell<Result<Value, String>>,
    pub offline_calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
    pub harvest: RefCell<Result<Value, String>>,
    pub harvest_calls: RefCell<Vec<(PathBuf, PathBuf)>>,
    pub table: RefCell<Result<String, String>>,
    pub fetch: RefCell<Result<(), String>>,
    pub fetch_body: RefCell<Vec<u8>>,
    pub fetched: RefCell<Vec<String>>,
    pub install: RefCell<Result<Value, String>>,
    pub installs: RefCell<Vec<(PathBuf, PathBuf, u64)>>,
    pub helper: RefCell<Result<(), String>>,
    pub helper_specs: RefCell<Vec<VmSpec>>,
    next_pid: Cell<u32>,
}

impl FakeSystem {
    pub fn new() -> Self {
        Self {
            scripts: RefCell::new(VecDeque::new()),
            starts: RefCell::new(Vec::new()),
            start_error: RefCell::new(None),
            clone_fails_on: RefCell::new(None),
            clone_budget: Cell::new(None),
            after_clone: RefCell::new(None),
            cloned: RefCell::new(Vec::new()),
            identity: RefCell::new(Ok(b"fresh-id".to_vec())),
            uid: Cell::new(501),
            ppid: Cell::new(1234),
            context: RefCell::new(Some("System".to_owned())),
            console_uid: Cell::new(Some(501)),
            purge: RefCell::new(Ok(())),
            purges: Cell::new(0),
            sig: RefCell::new(Ok(good_signature())),
            offline: RefCell::new(Ok(json!({"written": 5}))),
            offline_calls: RefCell::new(Vec::new()),
            harvest: RefCell::new(Ok(json!({"copied": []}))),
            harvest_calls: RefCell::new(Vec::new()),
            table: RefCell::new(Ok(String::new())),
            fetch: RefCell::new(Ok(())),
            fetch_body: RefCell::new(b"ipsw-bytes".to_vec()),
            fetched: RefCell::new(Vec::new()),
            install: RefCell::new(Ok(json!({"macos_version": "27.0.0"}))),
            installs: RefCell::new(Vec::new()),
            helper: RefCell::new(Ok(())),
            helper_specs: RefCell::new(Vec::new()),
            next_pid: Cell::new(1000),
        }
    }

    pub fn script(&self, s: Script) -> &Self {
        self.scripts.borrow_mut().push_back(s);
        self
    }
}

pub fn good_signature() -> Signature {
    Signature {
        adhoc: true,
        hardened_runtime: true,
        virtualization_entitlement: true,
        raw: "flags=0x10002(adhoc,runtime)".to_owned(),
    }
}

impl System for FakeSystem {
    fn clonefile(&self, src: &Path, dst: &Path) -> io::Result<()> {
        let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if *self.clone_fails_on.borrow() == Some(name) {
            return Err(io::Error::other("EXDEV"));
        }
        if let Some(left) = self.clone_budget.get() {
            if left == 0 {
                return Err(io::Error::other("no more clones"));
            }
            self.clone_budget.set(Some(left - 1));
        }
        self.cloned
            .borrow_mut()
            .push((src.to_owned(), dst.to_owned()));
        std::fs::copy(src, dst)?;
        if let Some(hook) = &*self.after_clone.borrow() {
            hook(dst);
        }
        Ok(())
    }
    fn new_machine_identifier(&self) -> Result<Vec<u8>, String> {
        self.identity.borrow().clone()
    }
    fn start_vm(&self, spec: &VmSpec, run_as: Option<u32>) -> Result<Box<dyn VmProcess>, String> {
        if let Some(e) = self.start_error.borrow().clone() {
            return Err(e);
        }
        self.starts.borrow_mut().push((spec.clone(), run_as));
        let script = self
            .scripts
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(Script::booted);
        let write = |dir: &Path, files: &[(String, String)]| {
            for (name, body) in files {
                let path = dir.join(name);
                std::fs::create_dir_all(dir).expect("results dir");
                std::fs::write(path, body).expect("guest file");
            }
        };
        let mut late = Vec::new();
        if let Some(dir) = &spec.automount_dir {
            let share = dir.join("results");
            write(&share, &script.share_files);
            match &spec.results_dir {
                Some(received) => write(received, &script.guest_files),
                None => write(&share, &script.guest_files),
            }
            let into = spec.results_dir.clone().unwrap_or(share);
            std::fs::create_dir_all(&into).expect("results dir");
            late = script
                .late_files
                .iter()
                .map(|(n, name, body)| (*n, into.join(name), body.clone()))
                .collect();
        }
        let pid = self.next_pid.get();
        self.next_pid.set(pid + 1);
        Ok(Box::new(FakeProc {
            pid,
            events: script.events.into(),
            late,
            polls: 0,
            killed: false,
            stop_asked: false,
            shutdown_asked: false,
            exits_on_stop: script.exits_on_stop,
            obeys_shutdown: script.obeys_shutdown,
            shutdown_command: script.shutdown_command,
            log: script.log,
        }))
    }
    fn purge_cache(&self) -> Result<(), String> {
        self.purges.set(self.purges.get() + 1);
        self.purge.borrow().clone()
    }
    fn uid(&self) -> u32 {
        self.uid.get()
    }
    fn ppid(&self) -> u32 {
        self.ppid.get()
    }
    fn launchd_context(&self) -> Option<String> {
        self.context.borrow().clone()
    }
    fn console_uid(&self) -> Option<u32> {
        self.console_uid.get()
    }
    fn host_facts(&self) -> Value {
        json!({"macos": "27.0", "arch": "arm64"})
    }
    fn signature(&self) -> Result<Signature, String> {
        self.sig.borrow().clone()
    }
    fn offline_write(&self, disk: &Path, files: &[OfflineFile]) -> Result<Value, String> {
        self.offline_calls.borrow_mut().push((
            disk.to_owned(),
            files.iter().map(|f| f.path.clone()).collect(),
        ));
        self.offline.borrow().clone()
    }
    fn offline_harvest(&self, disk: &Path, out: &Path) -> Result<Value, String> {
        self.harvest_calls
            .borrow_mut()
            .push((disk.to_owned(), out.to_owned()));
        self.harvest.borrow().clone()
    }
    fn process_table(&self) -> Result<String, String> {
        self.table.borrow().clone()
    }
    fn fetch(&self, url: &str, dest: &Path) -> Result<(), String> {
        self.fetched.borrow_mut().push(url.to_owned());
        let r = self.fetch.borrow().clone();
        if r.is_ok() {
            std::fs::write(dest, &*self.fetch_body.borrow()).map_err(|e| e.to_string())?;
        }
        r
    }
    fn install_image(&self, ipsw: &Path, out: &Path, disk_gib: u64) -> Result<Value, String> {
        self.installs
            .borrow_mut()
            .push((ipsw.to_owned(), out.to_owned(), disk_gib));
        self.install.borrow().clone()
    }
    fn run_vm_helper(&self, spec: &VmSpec) -> Result<(), String> {
        self.helper_specs.borrow_mut().push(spec.clone());
        self.helper.borrow().clone()
    }
}

/// A fresh empty directory for one test, removed when dropped.
pub fn tmp(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-vmm-{name}"))
}

/// A work directory holding a base bundle at `<work>/base`.
pub fn work_with_base(name: &str) -> TestDir {
    let work = tmp(name);
    let base = work.join("base");
    std::fs::create_dir_all(&base).expect("base");
    for f in FILES {
        std::fs::write(base.join(f), format!("contents of {f}")).expect("base file");
    }
    work
}

/// True when every condition holds. The conditions are evaluated first, so a test's
/// assertion on several of them is one branch.
pub fn holds(conditions: &[bool]) -> bool {
    conditions.iter().all(|c| *c)
}

/// A probe context over `sys` in `work`, with instant timing.
pub fn ctx<'a>(sys: &'a FakeSystem, work: &Path) -> crate::probe::Ctx<'a> {
    crate::probe::Ctx {
        sys,
        work: work.to_owned(),
        base: work.join("base"),
        guest_binary: None,
        keep: false,
        graphics: false,
        matrix: false,
        timing: crate::probe::Timing::instant(),
    }
}
