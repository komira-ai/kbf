//! Test helpers: [`FakeExec`], an in-memory host that records every step and query, and
//! scratch directories.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Output};

use crate::exec::{Entry, Exec};
use crate::path::HostPath;
use crate::step::{Cmd, HttpUrl, Owner, Step};
use crate::trust::Untrusted;

/// How the fake answers a command: a query, a [`Step::Run`] or a [`Step::Capture`].
pub type Answer = Box<dyn Fn(&Cmd) -> io::Result<Output>>;

/// A host made of a map from path to entry. Every step `apply` is given is pushed to
/// `log` first, and every query to `queries`; file, directory, removal and capture
/// steps then change the map, so a plan made after an apply sees the result. Commands
/// get their output from `answer` (an error there is a tool that cannot start); a
/// [`Step::Run`] or [`Step::Capture`] whose answer is a non-zero exit fails.
pub struct FakeExec {
    pub euid: u32,
    pub entries: BTreeMap<HostPath, Entry>,
    pub log: Vec<Step>,
    pub queries: RefCell<Vec<Cmd>>,
    pub answer: Answer,
    /// What every `http_status` answers.
    pub http: Result<u16, io::ErrorKind>,
    /// Fail the step at this index of `log` (counting every step applied).
    pub fail_at: Option<usize>,
    /// Make `check_binary` refuse.
    pub untrusted: bool,
}

/// The owner a step with `owner: None` leaves: the fake's user.
pub const RUNNER: Owner = Owner { uid: 501, gid: 20 };

/// A command's output: exit `code`, with `stdout` and `stderr`.
pub fn reply(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

impl FakeExec {
    /// Every command exits 0 with no output, and every URL answers 200.
    pub fn new(euid: u32) -> FakeExec {
        FakeExec {
            euid,
            entries: BTreeMap::new(),
            log: Vec::new(),
            queries: RefCell::new(Vec::new()),
            answer: Box::new(|_| Ok(reply(0, "", ""))),
            http: Ok(200),
            fail_at: None,
            untrusted: false,
        }
    }
}

impl Exec for FakeExec {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        if path.as_str() == "/unreadable" {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(self.entries.get(path).cloned())
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.queries.borrow_mut().push(cmd.clone());
        (self.answer)(cmd)
    }

    fn http_status(&self, _url: &HttpUrl) -> io::Result<u16> {
        self.http.map_err(io::Error::from)
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        self.log.push(step.clone());
        if self.fail_at == Some(self.log.len() - 1) {
            return Err(io::Error::other("planted failure"));
        }
        match step {
            Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            } => {
                let entry = Entry::File {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                    bytes: bytes.clone(),
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::Mkdir { path, perms, owner } => {
                let entry = Entry::Dir {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::Remove { path } => {
                self.entries.remove(path);
            }
            Step::Run(cmd) => {
                crate::exec::succeeded(cmd, (self.answer)(cmd)?)?;
            }
            Step::Capture {
                cmd,
                filter,
                path,
                perms,
                owner,
            } => {
                let out = crate::exec::succeeded(cmd, (self.answer)(cmd)?)?;
                let entry = Entry::File {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                    bytes: (filter.apply)(&out.stdout).map_err(io::Error::other)?,
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::WaitHttp { .. } => {}
        }
        Ok(())
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        if self.untrusted {
            return Err(Untrusted::Writable {
                path: PathBuf::from("/opt/downloads/kbf-deploy"),
                mode: 0o775,
            });
        }
        Ok(())
    }
}

/// A fresh, empty directory for one test: `<target>/tmp/kbf-deploy/<name>`.
pub fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("<target>/<profile>/deps/<test binary>");
    let dir = target.join("tmp").join("kbf-deploy").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make the scratch directory");
    dir
}
