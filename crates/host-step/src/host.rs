//! The host a plan is made against and carried out on.

use std::io;

use crate::RUNUSER;

/// What kind of thing a path is. A link is not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// Anything else, a symbolic link included.
    Other,
}

/// What `stat` says of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The kind.
    pub kind: Kind,
    /// The permission bits, `0o7777` at most.
    pub mode: u32,
    /// The owning user.
    pub uid: u32,
    /// The owning group.
    pub gid: u32,
    /// The size in bytes.
    pub size: u64,
}

/// A program and its arguments. The program is an absolute path; there is no shell and no
/// environment, so nothing a name holds is interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

impl Cmd {
    /// A command running `program`.
    pub fn new(program: &str) -> Cmd {
        Cmd {
            program: program.to_owned(),
            args: Vec::new(),
        }
    }

    /// With one more argument.
    #[must_use]
    pub fn arg(mut self, arg: &str) -> Cmd {
        self.args.push(arg.to_owned());
        self
    }

    /// With these more arguments.
    #[must_use]
    pub fn args<S: AsRef<str>>(mut self, args: &[S]) -> Cmd {
        self.args.extend(args.iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// The same command run as `user` (`runuser -u USER -- PROGRAM ARGS`).
    #[must_use]
    pub fn as_user(self, user: &str) -> Cmd {
        Cmd::new(RUNUSER)
            .arg("-u")
            .arg(user)
            .arg("--")
            .arg(&self.program)
            .args(&self.args)
    }

    /// The program and its arguments as one line, for a plan.
    pub fn line(&self) -> String {
        let mut words = vec![self.program.as_str()];
        words.extend(self.args.iter().map(String::as_str));
        words.join(" ")
    }

    /// The program and the arguments, as `argv`.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![self.program.clone()];
        argv.extend(self.args.clone());
        argv
    }
}

/// What a program printed and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The exit status; -1 when a signal ended the program.
    pub code: i32,
    /// What it wrote to standard output.
    pub stdout: String,
    /// What it wrote to standard error.
    pub stderr: String,
}

/// The machine: files, programs and the effective user. Paths are absolute.
pub trait Host {
    /// The effective user id.
    fn euid(&self) -> u32;

    /// The effective group id.
    fn egid(&self) -> u32;

    /// `path` with every symbolic link on the way resolved. The part of the path that does
    /// not exist yet is kept as given.
    ///
    /// # Errors
    /// The system's error, other than "not found"; too many links.
    fn canonical(&self, path: &str) -> io::Result<String>;

    /// `lstat` of `path` (a link on the way is refused, a link at the end is reported as
    /// such); `None` when there is nothing there.
    ///
    /// # Errors
    /// The system's error, other than "not found".
    fn stat(&self, path: &str) -> io::Result<Option<Meta>>;

    /// The bytes of the file at `path`; `None` when there is nothing there. The real host
    /// follows no link: not one on the way, and not the file itself.
    ///
    /// # Errors
    /// The system's error, other than "not found" (a permission error included).
    fn read(&self, path: &str) -> io::Result<Option<Vec<u8>>>;

    /// Like [`Host::read`], but a file longer than `limit` bytes is an error of kind
    /// `FileTooLarge`; no more than `limit + 1` bytes are read. The real host opens the file
    /// once and reads that descriptor, so a swapped name cannot slip past the limit.
    ///
    /// # Errors
    /// As [`Host::read`]; the file is too large.
    fn read_limited(&self, path: &str, limit: u64) -> io::Result<Option<Vec<u8>>>;

    /// Writes `bytes` to `path` with exactly `mode`, owned by `owner` (uid, gid), replacing
    /// any file there in one step.
    ///
    /// # Errors
    /// The system's error.
    fn write(&self, path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) -> io::Result<()>;

    /// Makes the directory `path` (and any parent that is missing, as root's 0755) with
    /// exactly `mode`, owned by `owner`; one that is there is brought to them.
    ///
    /// # Errors
    /// The system's error.
    fn mkdir(&self, path: &str, mode: u32, owner: (u32, u32)) -> io::Result<()>;

    /// Removes the file at `path`; nothing there is not an error.
    ///
    /// # Errors
    /// The system's error, other than "not found".
    fn remove(&self, path: &str) -> io::Result<()>;

    /// Runs `cmd` to its end.
    ///
    /// # Errors
    /// The program could not be started (a program that ran and failed is an [`Output`]).
    fn run(&self, cmd: &Cmd) -> io::Result<Output>;
}
