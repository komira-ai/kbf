//! The local socket: a unix socket, mode 0600, owned by the one account that may use it.
//!
//! ```text
//! kbf-host-install --serve --socket PATH --socket-user NAME [--window-secs N] [--restart-unit UNIT]...
//! ```
//!
//! The program binds the socket itself (a umask of 0177, then a chown to the account), in a
//! directory that is root's and closed to others, so the file mode is the whole of who may
//! connect. One request line in, one JSON line out ([`super::request`], [`super::answer`]),
//! one connection at a time: an install can take minutes, and no other request is
//! answered meanwhile, which is the lock of the ssh path's install seen from the socket.
//! Between connections the loop asks [`Ab::tick`], which goes back to the release that ran
//! when a release on trial has not been confirmed by its deadline. The first tick after a
//! start is that check too, so a host that rebooted with a release on trial goes back at
//! once if the deadline passed while it was down.
//!
//! `--window-secs` is how long a release on trial has to be confirmed (120 to 1800, default
//! 600). `--restart-unit` names a unit that runs the release (repeatable; a Linux host
//! needs one at least, and a Mac restarts the daemon's job). The paths this program reads as
//! root are constants, as they are for the ssh path; no flag moves them.
//!
//! Every request but `query` and `revert` is audited, and is not run when the audit log
//! cannot be opened. A revert is never held back by it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::answer::{Answer, Code};
use super::backend::Backend;
use super::core::Ab;
use super::request::{LocalRequest, MAX_LINE_LEN};
use crate::audit::{self, Audit};
use crate::request::{BadCommand, Unit};
use crate::trust::Trust;

/// The window a release on trial has to be confirmed, unless a flag says otherwise.
pub const DEFAULT_WINDOW_SECS: u64 = 600;

/// How long the loop sleeps when no connection waits.
const IDLE: Duration = Duration::from_millis(200);

/// How long a client has to send its line, and to take the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub const USAGE: &str = "usage: kbf-host-install --serve --socket PATH --socket-user NAME \
[--window-secs N] [--restart-unit UNIT]...\n";

/// The settings of a served socket, from the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub socket: PathBuf,
    pub user: String,
    pub window_secs: u64,
    pub units: Vec<Unit>,
}

impl ServeConfig {
    /// Read the flags (after `--serve`). Each flag but `--restart-unit` is given once, as
    /// `--flag value` or `--flag=value`.
    ///
    /// # Errors
    /// What is wrong with them, in a line.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut socket = None;
        let mut user = None;
        let mut window = None;
        let mut units = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))?,
            };
            let once = |slot: &mut Option<String>| {
                if slot.replace(value.clone()).is_some() {
                    Err(format!("{name} is given twice"))
                } else {
                    Ok(())
                }
            };
            match name {
                "--socket" => once(&mut socket)?,
                "--socket-user" => once(&mut user)?,
                "--window-secs" => once(&mut window)?,
                "--restart-unit" => {
                    units.push(Unit::service(&value).map_err(|BadCommand| {
                        format!("--restart-unit {value:?} is not a kbf unit")
                    })?)
                }
                _ => return Err(format!("unknown argument {name}")),
            }
        }
        let socket = PathBuf::from(socket.ok_or("--socket is required")?);
        if !socket.is_absolute() {
            return Err("--socket must be an absolute path".to_owned());
        }
        let user = user.ok_or("--socket-user is required")?;
        let window_secs = match window {
            Some(text) => text
                .parse::<u64>()
                .ok()
                .filter(|secs| (120..=1800).contains(secs))
                .ok_or("--window-secs must be a number from 120 to 1800")?,
            None => DEFAULT_WINDOW_SECS,
        };
        Ok(Self {
            socket,
            user,
            window_secs,
            units,
        })
    }
}

/// The ids of the account `name` in the text of a passwd file.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn passwd_ids(passwd: &str, name: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let (user, _password, uid, gid) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        (user == name).then_some(())?;
        Some((uid.parse().ok()?, gid.parse().ok()?))
    })
}

/// Bind the socket at `path` for the account `ids`: its directory is `trust`'s and closed
/// to others, a stale socket file is removed (anything else in the way is not), the file is
/// made with mode 0600 and owned by the account.
///
/// # Errors
/// Why the socket cannot be made.
pub fn bind(trust: &Trust, path: &Path, ids: (u32, u32)) -> io::Result<UnixListener> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the socket has no directory"))?;
    trust
        .check_dir(parent)
        .map_err(|e| io::Error::other(format!("{}: {e}", parent.display())))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::other(
                "something that is not a socket is in the way",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Nobody can connect between the bind and the chown: the file is 0600 and root's.
    let before = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o177));
    let listener = UnixListener::bind(path);
    rustix::process::umask(before);
    let listener = listener?;
    chown(path, Some(ids.0), Some(ids.1))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// What a connection needs to be answered and audited.
pub struct Serving<'a, B: Backend> {
    pub ab: &'a Ab<B>,
    pub trust: &'a Trust,
    pub audit: &'a Path,
    /// Where the log of what happened goes (stderr, in production).
    pub log: &'a dyn Fn(&str),
}

impl<B: Backend> Serving<'_, B> {
    /// Read the request line from `stream`, answer it, and audit it.
    pub fn serve_one(&self, stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let line = read_line(&stream);
        let answer = self.answer(line.as_deref());
        let mut writer = &stream;
        let wrote = writeln!(writer, "{}", answer.json_line()).and_then(|()| writer.flush());
        if let Err(e) = wrote {
            (self.log)(&format!("the client is gone: {e}"));
        }
    }

    /// Answer a request line (none: not one that was understood).
    pub fn answer(&self, line: Option<&str>) -> Answer {
        let request = line.map_or(Err(BadCommand), LocalRequest::parse);
        let log = Audit::open(self.trust, self.audit);
        let answer = match &request {
            Err(BadCommand) => Answer::Refused(Code::BadCommand, "not a request".to_owned()),
            // A revert goes back to the release that ran; nothing holds it back.
            Ok(request @ (LocalRequest::Query | LocalRequest::Revert)) => self.ab.handle(request),
            Ok(request) if log.is_err() => Answer::Failed(
                Code::AuditUnavailable,
                format!(
                    "{}: the audit log cannot be opened; nothing was run",
                    request.verb()
                ),
            ),
            Ok(request) => self.ab.handle(request),
        };
        self.record(request.as_ref().ok(), line, &answer, log);
        answer
    }

    /// Append the audit line, and tell the log what a refusal or a failure said.
    pub fn record(
        &self,
        request: Option<&LocalRequest>,
        line: Option<&str>,
        answer: &Answer,
        log: io::Result<Audit>,
    ) {
        if let Some(detail) = answer.log_detail() {
            let verb = request.map_or("?", LocalRequest::verb);
            (self.log)(&format!(
                "{verb}: {}: {detail}",
                answer.audit_fields().1.unwrap_or("")
            ));
        }
        if matches!(request, Some(LocalRequest::Query)) {
            return;
        }
        let (outcome, reason) = answer.audit_fields();
        let (version, channel) = match request {
            Some(LocalRequest::Install { version, channel }) => (
                Some(version.as_str().to_owned()),
                Some(channel.as_str().to_owned()),
            ),
            Some(LocalRequest::Switch { version } | LocalRequest::Confirm { version }) => {
                (Some(version.as_str().to_owned()), None)
            }
            _ => (None, None),
        };
        let entry = audit::Entry {
            time: audit::now(),
            verb: request.map(LocalRequest::verb),
            version,
            channel,
            unit: None,
            outcome,
            reason,
            result: None,
            command: request
                .is_none()
                .then(|| line.map(audit::truncate_raw))
                .flatten(),
        };
        let logged = log
            .map_err(|e| e.to_string())
            .and_then(|mut log| log.append(&entry).map_err(|e| e.to_string()));
        if let Err(why) = logged {
            (self.log)(&format!("audit log unavailable: {why}"));
        }
    }

    /// Go back, if a release on trial is past its deadline; audited like a revert.
    pub fn tick(&self) {
        let Some(answer) = self.ab.tick() else {
            return;
        };
        (self.log)(&format!("the deadline passed: {}", answer.json_line()));
        let log = Audit::open(self.trust, self.audit);
        self.record(Some(&LocalRequest::Revert), None, &answer, log);
    }

    /// Serve until `stop` is set: the connections, and the ticks between them.
    pub fn run(&self, listener: &UnixListener, stop: &AtomicBool) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::Relaxed) {
            self.tick();
            match listener.accept() {
                Ok((stream, _)) => self.serve_one(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(IDLE),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The first line the client sent, within [`MAX_LINE_LEN`] bytes, without its end; none when
/// it sent none, a longer one, or text that is not UTF-8.
fn read_line(stream: &UnixStream) -> Option<String> {
    let mut line = String::new();
    let read = BufReader::new(stream.take(MAX_LINE_LEN as u64 + 1)).read_line(&mut line);
    read.ok()?;
    let line = line.strip_suffix('\n')?;
    (line.len() <= MAX_LINE_LEN).then(|| line.to_owned())
}
