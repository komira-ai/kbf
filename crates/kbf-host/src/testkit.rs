//! Test helpers, for this crate's tests and (with the `testkit` feature) other crates':
//! [`FakeExec`], an in-memory host that records every step, and scratch directories.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use crate::exec::{Entry, Exec};
use crate::path::HostPath;
use crate::step::{Owner, Step};
use crate::trust::Untrusted;

/// A host made of a map from path to entry. Every step `apply` is given is pushed to
/// `log` first; file, directory and removal steps then change the map, so a plan made
/// after an apply sees the result. Commands and waits only enter the log.
pub struct FakeExec {
    pub euid: u32,
    pub entries: BTreeMap<HostPath, Entry>,
    pub log: Vec<Step>,
    /// Fail the step at this index of `log` (counting every step applied).
    pub fail_at: Option<usize>,
    /// Make `check_binary` refuse.
    pub untrusted: bool,
}

/// The owner a step with `owner: None` leaves: the fake's user.
pub const RUNNER: Owner = Owner { uid: 501, gid: 20 };

impl FakeExec {
    pub fn new(euid: u32) -> FakeExec {
        FakeExec {
            euid,
            entries: BTreeMap::new(),
            log: Vec::new(),
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
            Step::Run(_) | Step::WaitHttp { .. } => {}
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

/// A fresh, empty directory for one test: `<target>/tmp/kbf-host/<name>`. Every crate
/// that uses it shares that directory, so `name` must be unique among them.
pub fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("<target>/<profile>/deps/<test binary>");
    let dir = target.join("tmp").join("kbf-host").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make the scratch directory");
    dir
}
