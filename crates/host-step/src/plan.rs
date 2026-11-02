//! The planner and the executor.

use std::fmt;
use std::io::{self, Write};

use crate::{Cmd, DRY_RUN_NOTE, GETENT, Host, Kind, NOTHING_TO_DO, Output, SYSTEMCTL, USERADD};

/// Whether a plan is printed or carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Print each step; change nothing.
    DryRun,
    /// Carry each step out, printing it first.
    Apply,
}

/// A user and a group, by name; the ids are looked up when they are needed, so a plan can
/// name a user it is about to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// The user.
    pub user: String,
    /// The group.
    pub group: String,
}

impl Account {
    /// `user` and `group`.
    pub fn new(user: &str, group: &str) -> Account {
        Account {
            user: user.to_owned(),
            group: group.to_owned(),
        }
    }

    /// `root:root`.
    pub fn root() -> Account {
        Account::new("root", "root")
    }
}

/// What a systemctl step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitVerb {
    /// `daemon-reload`, no units.
    DaemonReload,
    /// `enable`.
    Enable,
    /// `start`.
    Start,
    /// `restart`.
    Restart,
    /// `disable --now`.
    DisableNow,
}

impl UnitVerb {
    fn words(self) -> &'static [&'static str] {
        match self {
            UnitVerb::DaemonReload => &["daemon-reload"],
            UnitVerb::Enable => &["enable"],
            UnitVerb::Start => &["start"],
            UnitVerb::Restart => &["restart"],
            UnitVerb::DisableNow => &["disable", "--now"],
        }
    }
}

/// One change to a host. `Debug` prints the length of a file's bytes, never the bytes: a
/// plan that is formatted by an assertion, a panic or a log line cannot show a secret.
#[derive(Clone, PartialEq, Eq)]
pub enum Step {
    /// A directory with this mode and owner.
    Dir {
        /// Where.
        path: String,
        /// The permission bits.
        mode: u32,
        /// The owner.
        owner: Account,
    },
    /// A file with these bytes, mode and owner. The bytes are never printed.
    File {
        /// Where.
        path: String,
        /// The permission bits.
        mode: u32,
        /// The owner.
        owner: Account,
        /// The content.
        bytes: Vec<u8>,
    },
    /// No file at `path`.
    Absent {
        /// Where.
        path: String,
    },
    /// A system user (and a group of its name), with no login and no home made.
    User {
        /// The name.
        name: String,
        /// The home directory it is recorded with.
        home: String,
    },
    /// A systemctl action.
    Unit {
        /// The action.
        verb: UnitVerb,
        /// The units, none for a reload.
        units: Vec<String>,
    },
    /// A program.
    Run(Cmd),
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Dir { path, mode, owner } => f
                .debug_struct("Dir")
                .field("path", path)
                .field("mode", &format_args!("{mode:04o}"))
                .field("owner", owner)
                .finish(),
            Step::File {
                path,
                mode,
                owner,
                bytes,
            } => f
                .debug_struct("File")
                .field("path", path)
                .field("mode", &format_args!("{mode:04o}"))
                .field("owner", owner)
                .field("bytes", &format_args!("{} bytes", bytes.len()))
                .finish(),
            Step::Absent { path } => f.debug_struct("Absent").field("path", path).finish(),
            Step::User { name, home } => f
                .debug_struct("User")
                .field("name", name)
                .field("home", home)
                .finish(),
            Step::Unit { verb, units } => f
                .debug_struct("Unit")
                .field("verb", verb)
                .field("units", units)
                .finish(),
            Step::Run(cmd) => f.debug_tuple("Run").field(cmd).finish(),
        }
    }
}

impl Step {
    /// The step's line in a plan: names, sizes and modes only.
    pub fn describe(&self) -> String {
        match self {
            Step::Dir { path, mode, owner } => format!(
                "mkdir {path} mode {mode:04o} owner {}:{}",
                owner.user, owner.group
            ),
            Step::File {
                path,
                mode,
                owner,
                bytes,
            } => format!(
                "write {path} mode {mode:04o} owner {}:{} ({} bytes)",
                owner.user,
                owner.group,
                bytes.len()
            ),
            Step::Absent { path } => format!("remove {path}"),
            Step::User { name, home } => {
                format!("useradd {name} (system user and group, home {home}, no login)")
            }
            Step::Unit { verb, units } => {
                format!("systemctl {}", unit_cmd(*verb, units).args.join(" "))
            }
            Step::Run(cmd) => format!("run {}", cmd.line()),
        }
    }
}

/// The systemctl command of a unit step; the units come after `--`, so a name that starts
/// with a hyphen is not read as an option.
fn unit_cmd(verb: UnitVerb, units: &[String]) -> Cmd {
    let cmd = Cmd::new(SYSTEMCTL).args(verb.words());
    match units {
        [] => cmd,
        _ => cmd.arg("--").args(units),
    }
}

/// The steps a host needs, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The steps.
    pub steps: Vec<Step>,
}

/// The ids of `account`; `None` when the user or the group does not exist.
///
/// # Errors
/// A lookup command could not be started.
pub fn ids(host: &dyn Host, account: &Account) -> io::Result<Option<(u32, u32)>> {
    let uid = id(host, "passwd", &account.user)?;
    let gid = id(host, "group", &account.group)?;
    Ok(uid.zip(gid))
}

fn id(host: &dyn Host, database: &str, name: &str) -> io::Result<Option<u32>> {
    // `getent passwd NAME` prints `name:x:uid:gid:...`, `getent group NAME` `name:x:gid:...`.
    let found = host.run(&Cmd::new(GETENT).arg(database).arg(name))?;
    let field = found.stdout.trim().split(':').nth(2);
    let id = field.and_then(|f| f.parse().ok());
    Ok(id.filter(|_| found.code == 0))
}

/// Looks at a host and records what would change it.
pub struct Planner<'h> {
    host: &'h dyn Host,
    steps: Vec<Step>,
}

impl<'h> Planner<'h> {
    /// A planner for `host` with no steps.
    pub fn new(host: &'h dyn Host) -> Planner<'h> {
        Planner {
            host,
            steps: Vec::new(),
        }
    }

    /// The plan made.
    pub fn finish(self) -> Plan {
        Plan { steps: self.steps }
    }

    /// The user `name`, unless it exists. Whether a step was added.
    ///
    /// # Errors
    /// A lookup command could not be started.
    pub fn user(&mut self, name: &str, home: &str) -> io::Result<bool> {
        let exists = id(self.host, "passwd", name)?.is_some();
        if !exists {
            self.steps.push(Step::User {
                name: name.to_owned(),
                home: home.to_owned(),
            });
        }
        Ok(!exists)
    }

    /// The directory, unless it is there with this mode and owner. Whether a step was added.
    ///
    /// # Errors
    /// A read or a lookup failed.
    pub fn dir(&mut self, path: &str, mode: u32, owner: &Account) -> io::Result<bool> {
        let have = self
            .host
            .stat(path)?
            .map(|m| (m.kind, m.mode, (m.uid, m.gid)));
        let want = Some((
            Kind::Dir,
            mode,
            ids(self.host, owner)?.unwrap_or((u32::MAX, u32::MAX)),
        ));
        let same = have == want;
        if !same {
            self.steps.push(Step::Dir {
                path: path.to_owned(),
                mode,
                owner: owner.clone(),
            });
        }
        Ok(!same)
    }

    /// The file, unless it is there with these bytes, this mode and owner. Whether a step
    /// was added.
    ///
    /// # Errors
    /// A read or a lookup failed.
    pub fn file(
        &mut self,
        path: &str,
        mode: u32,
        owner: &Account,
        bytes: &[u8],
    ) -> io::Result<bool> {
        let have = self
            .host
            .stat(path)?
            .map(|m| (m.kind, m.mode, (m.uid, m.gid)));
        let want = Some((
            Kind::File,
            mode,
            ids(self.host, owner)?.unwrap_or((u32::MAX, u32::MAX)),
        ));
        let same = (have, self.host.read(path)?) == (want, Some(bytes.to_vec()));
        if !same {
            self.steps.push(Step::File {
                path: path.to_owned(),
                mode,
                owner: owner.clone(),
                bytes: bytes.to_vec(),
            });
        }
        Ok(!same)
    }

    /// No file at `path`, if there is one. Whether a step was added.
    ///
    /// # Errors
    /// The stat failed.
    pub fn absent(&mut self, path: &str) -> io::Result<bool> {
        let there = self.host.stat(path)?.is_some();
        if there {
            self.steps.push(Step::Absent {
                path: path.to_owned(),
            });
        }
        Ok(there)
    }

    /// A systemctl action.
    pub fn unit(&mut self, verb: UnitVerb, units: &[&str]) {
        self.steps.push(Step::Unit {
            verb,
            units: units.iter().map(|u| (*u).to_owned()).collect(),
        });
    }

    /// A program.
    pub fn run(&mut self, cmd: Cmd) {
        self.steps.push(Step::Run(cmd));
    }

    /// Whether `systemctl <query> <unit>` succeeds (`is-enabled`, `is-active`).
    ///
    /// # Errors
    /// systemctl could not be started.
    pub fn unit_is(&self, query: &str, unit: &str) -> io::Result<bool> {
        let out = self.host.run(&Cmd::new(SYSTEMCTL).arg(query).arg(unit))?;
        Ok(out.code == 0)
    }

    /// `unit` enabled, and restarted when `again`, else started when it is not active.
    /// Whether it is (re)started.
    ///
    /// # Errors
    /// systemctl could not be started.
    pub fn service(&mut self, unit: &str, again: bool) -> io::Result<bool> {
        if !self.unit_is("is-enabled", unit)? {
            self.unit(UnitVerb::Enable, &[unit]);
        }
        let idle = !self.unit_is("is-active", unit)?;
        if again {
            self.unit(UnitVerb::Restart, &[unit]);
        } else if idle {
            self.unit(UnitVerb::Start, &[unit]);
        }
        Ok(again || idle)
    }
}

/// Why a plan could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// Applying needs root.
    #[error("applying a plan needs root (effective uid is {0})")]
    NotRoot(u32),
    /// A step failed; the steps before it were done.
    #[error("step {index} ({what}) failed: {why}")]
    Step {
        /// The step's position, from 1.
        index: usize,
        /// Its line in the plan.
        what: String,
        /// What went wrong.
        why: String,
    },
    /// The plan could not be written out.
    #[error("cannot write the plan out: {0}")]
    Write(#[from] io::Error),
}

/// Prints `plan`, and with [`Mode::Apply`] carries it out step by step, stopping at the first
/// that fails.
///
/// # Errors
/// [`ExecError`].
pub fn execute(
    plan: &Plan,
    mode: Mode,
    host: &dyn Host,
    out: &mut dyn Write,
) -> Result<(), ExecError> {
    if mode == Mode::Apply && host.euid() != 0 {
        return Err(ExecError::NotRoot(host.euid()));
    }
    if plan.steps.is_empty() {
        writeln!(out, "{NOTHING_TO_DO}")?;
    }
    for (i, step) in plan.steps.iter().enumerate() {
        let what = step.describe();
        match mode {
            Mode::DryRun => writeln!(out, "DRY-RUN: {what}")?,
            Mode::Apply => {
                writeln!(out, "+ {what}")?;
                apply(host, step).map_err(|e| ExecError::Step {
                    index: i + 1,
                    what,
                    why: e.to_string(),
                })?;
            }
        }
    }
    if mode == Mode::DryRun && !plan.steps.is_empty() {
        writeln!(out, "{DRY_RUN_NOTE}")?;
    }
    Ok(())
}

fn apply(host: &dyn Host, step: &Step) -> io::Result<()> {
    match step {
        Step::Dir { path, mode, owner } => host.mkdir(path, *mode, owner_ids(host, owner)?),
        Step::File {
            path,
            mode,
            owner,
            bytes,
        } => host.write(path, *mode, owner_ids(host, owner)?, bytes),
        Step::Absent { path } => host.remove(path),
        Step::User { name, home } => {
            let cmd = Cmd::new(USERADD).args(&[
                "--system",
                "--user-group",
                "--home-dir",
                home,
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                name,
            ]);
            succeeded(host.run(&cmd)?)
        }
        Step::Unit { verb, units } => succeeded(host.run(&unit_cmd(*verb, units))?),
        Step::Run(cmd) => succeeded(host.run(cmd)?),
    }
}

fn owner_ids(host: &dyn Host, owner: &Account) -> io::Result<(u32, u32)> {
    ids(host, owner)?
        .ok_or_else(|| io::Error::other(format!("no user {} or group {}", owner.user, owner.group)))
}

/// A program's exit status as a result. Its output is not repeated: a program may print a
/// value it should not.
fn succeeded(out: Output) -> io::Result<()> {
    match out.code {
        0 => Ok(()),
        code => Err(io::Error::other(format!("exit status {code}"))),
    }
}
