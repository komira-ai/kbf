//! Where steps happen: the host ([`HostExec`]), or a recording fake in tests.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::path::{HostPath, Root};
use crate::step::{Cmd, HttpUrl, Owner, Perms, Step};
use crate::trust::{self, Untrusted};

/// What is at a path, as a plan compares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File {
        perms: Perms,
        owner: Owner,
        bytes: Vec<u8>,
    },
    Dir {
        perms: Perms,
        owner: Owner,
    },
    /// A symbolic link, socket or device: never what a plan wants, so a file or
    /// directory step replaces it (or fails) and a removal removes it.
    Other,
}

/// Reads and changes a host. [`HostExec`] is the real one; tests use a fake that
/// records each step.
pub trait Exec {
    /// The effective uid the steps would run with.
    fn euid(&self) -> u32;

    /// What is at `path` now; `None` when nothing is.
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>>;

    /// Carry out one step.
    fn apply(&mut self, step: &Step) -> io::Result<()>;

    /// Refuses unless the running binary and every directory above it are owned by root
    /// and writable by no one else; asked only before applying as root.
    fn check_binary(&self) -> Result<(), Untrusted>;
}

/// The real host, with every path joined to `root`.
#[derive(Clone, Debug)]
pub struct HostExec {
    root: Root,
}

impl HostExec {
    pub fn new(root: Root) -> HostExec {
        HostExec { root }
    }
}

impl Exec for HostExec {
    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        let full = self.root.join(path);
        let meta = match fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let perms = Perms::new(meta.mode() & 0o7777);
        let owner = Owner {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        Ok(Some(if meta.is_file() {
            Entry::File {
                perms,
                owner,
                bytes: fs::read(&full)?,
            }
        } else if meta.is_dir() {
            Entry::Dir { perms, owner }
        } else {
            Entry::Other
        }))
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        match step {
            Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            } => write_file(&self.root.join(path), *perms, *owner, bytes),
            Step::Mkdir { path, perms, owner } => {
                let full = self.root.join(path);
                // An existing directory (not a link to one) only gets its permissions
                // and owner set; anything else at the path fails the step.
                match fs::create_dir(&full) {
                    Ok(()) => {}
                    Err(e)
                        if e.kind() == io::ErrorKind::AlreadyExists
                            && fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir()) => {}
                    Err(e) => return Err(e),
                }
                set_perms_owner(&full, *perms, *owner)
            }
            Step::Remove { path } => {
                let full = self.root.join(path);
                if fs::symlink_metadata(&full)?.is_dir() {
                    fs::remove_dir(&full)
                } else {
                    fs::remove_file(&full)
                }
            }
            Step::Run(cmd) => run(cmd),
            Step::WaitHttp { url, deadline } => wait_http(url, *deadline),
        }
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        trust::check_running_binary()
    }
}

/// Write `bytes` beside `full` and rename the copy over it, so a reader sees the old
/// file or the new one, never a part.
fn write_file(full: &Path, perms: Perms, owner: Option<Owner>, bytes: &[u8]) -> io::Result<()> {
    let name = full
        .file_name()
        .expect("a HostPath names a file")
        .to_string_lossy();
    let tmp = full.with_file_name(format!(".{name}.kbf-deploy-tmp"));
    // A copy left by an interrupted run; `create_new` below never follows a link there.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    set_perms_owner(&tmp, perms, owner)?;
    file.sync_all()?;
    fs::rename(&tmp, full)
}

fn set_perms_owner(full: &Path, perms: Perms, owner: Option<Owner>) -> io::Result<()> {
    if let Some(o) = owner {
        std::os::unix::fs::chown(full, Some(o.uid), Some(o.gid))?;
    }
    // After the chown, which clears set-id bits on some systems.
    fs::set_permissions(full, fs::Permissions::from_mode(perms.bits()))
}

fn run(cmd: &Cmd) -> io::Result<()> {
    let out = Command::new(cmd.program.as_str())
        .args(&cmd.args)
        .env_clear()
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::other(format!(
        "{} exited with {}: {}",
        cmd.program,
        out.status,
        stderr.trim_end()
    )))
}

/// How long one attempt may take, and the pause between attempts.
const ATTEMPT: Duration = Duration::from_secs(2);
const PAUSE: Duration = Duration::from_millis(200);

fn wait_http(url: &HttpUrl, deadline: Duration) -> io::Result<()> {
    let end = Instant::now() + deadline;
    loop {
        let last = match get_status(url, ATTEMPT) {
            Ok(status) if (200..300).contains(&status) => return Ok(()),
            Ok(status) => format!("status {status}"),
            Err(e) => e.to_string(),
        };
        let now = Instant::now();
        if now >= end {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{url} did not answer 2xx within {deadline:?}; last: {last}"),
            ));
        }
        std::thread::sleep(PAUSE.min(end - now));
    }
}

/// One `GET`; the status code of the answer.
fn get_status(url: &HttpUrl, timeout: Duration) -> io::Result<u16> {
    let addr = (url.host(), url.port())
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::from(io::ErrorKind::AddrNotAvailable))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.path(),
        url.authority()
    );
    stream.write_all(request.as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    // `HTTP/1.1 200 OK`
    line.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP status line: {:?}", line.trim_end())))
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
