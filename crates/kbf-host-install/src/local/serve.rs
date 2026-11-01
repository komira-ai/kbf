//! The local socket: a unix socket, mode 0600, owned by the one account that may use it.
//!
//! ```text
//! kbf-host-install --serve --socket PATH --socket-user NAME [--window-secs N]
//!                   [--layout installer|deb|rpm]
//!                   (--restart-unit UNIT... | --frontend-container [--frontend-config PATH])
//! ```
//!
//! The program binds the socket itself, in a directory that is root's and closed to others:
//! the file is made in a private directory beside the socket's (mode 0700), given its owner
//! and mode 0600, and moved into place, so the file mode is the whole of who may connect. The
//! process umask is never changed. The bind happens at `<directory>/.bind/s`, so the
//! socket's directory path needs 8 bytes more within the 107 bytes a unix socket path may
//! have (a socket name of 7 bytes or more costs nothing beyond its own path). One request line in, one JSON line out ([`super::request`], [`super::answer`]),
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
//! root are constants, as they are for the ssh path; no flag moves them. `--frontend-container`
//! serves a frontend that runs as a container instead of units ([`super::frontend`]); it takes
//! no `--restart-unit`.
//! `--frontend-config` names the container's configuration (default
//! `/etc/kbf/host-install-frontend.toml`); with another file, the helper keeps its record, its
//! A/B state and its lock under names made from the container's name, so that a host runs one
//! helper, with its own `--socket`, per container.
//!
//! `--layout` says how a Linux host keeps its releases: `installer` (the default) is the
//! hand-placed layout of `kbf-installer` (`releases/<commit>` with the links `current` and
//! `previous`); `deb` and `rpm` are a host whose units run the `kbf` package's `/usr/bin/kbf`,
//! updated by installing that package ([`super::package`]). A Mac takes none.
//!
//! Every request but `query` and `revert` is audited, and is not run when the audit log
//! cannot be opened. A revert is never held back by it.

use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::answer::{Answer, Code};
use super::backend::Backend;
use super::core::Ab;
use super::request::{LocalRequest, MAX_LINE_LEN};
use crate::audit::{self, Audit};
use crate::pace::{Now, Pace};
use crate::request::{BadCommand, Unit};
use crate::trust::Trust;
use kbf_installer::retain::{DEFAULT_KEEP, MIN_KEEP};

/// The window a release on trial has to be confirmed, unless a flag says otherwise.
pub const DEFAULT_WINDOW_SECS: u64 = 600;

/// The most releases `--keep-releases` keeps: the `query` that lists them stays small.
pub const MAX_KEEP_RELEASES: usize = 32;

const KEEP_RANGE: &str = "--keep-releases must be a number from 2 to 32";

/// How long the loop sleeps when no connection waits.
const IDLE: Duration = Duration::from_millis(200);

/// How long a client has, in all, to send its line, and for each write of the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the loop leaves a revert alone after one that failed.
pub(super) const REVERT_RETRY: Duration = Duration::from_secs(30);

pub const USAGE: &str = "usage: kbf-host-install --serve --socket PATH --socket-user NAME \
[--window-secs N] [--keep-releases N] [--layout installer|deb|rpm] \
(--restart-unit UNIT... | --frontend-container [--frontend-config PATH])\n";

/// How a host keeps the releases it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// The hand-placed layout of `kbf-installer` (a Mac's kit tree is its counterpart).
    #[default]
    Installer,
    /// The `kbf` package, as a `.deb`.
    Deb,
    /// The `kbf` package, as an `.rpm`.
    Rpm,
}

impl Layout {
    fn named(name: &str) -> Option<Self> {
        match name {
            "installer" => Some(Self::Installer),
            "deb" => Some(Self::Deb),
            "rpm" => Some(Self::Rpm),
            _ => None,
        }
    }
}

/// The settings of a served socket, from the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub socket: PathBuf,
    pub user: String,
    pub window_secs: u64,
    /// How many releases the host keeps besides current, previous, the pinned ones and
    /// `last_good`.
    pub keep_releases: usize,
    pub layout: Layout,
    pub units: Vec<Unit>,
    /// The host runs the frontend as a container, not as units.
    pub frontend_container: bool,
    /// The frontend container's configuration file.
    pub frontend_config: PathBuf,
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
        let mut keep = None;
        let mut layout = None;
        let mut units = Vec::new();
        let mut frontend_container = false;
        let mut frontend_config = None;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            if name == "--frontend-container" {
                if inline.is_some() || std::mem::replace(&mut frontend_container, true) {
                    return Err("--frontend-container takes no value and is given once".to_owned());
                }
                continue;
            }
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
                "--keep-releases" => once(&mut keep)?,
                "--layout" => once(&mut layout)?,
                "--frontend-config" => once(&mut frontend_config)?,
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
        let keep_releases = match keep {
            Some(text) => text
                .parse::<usize>()
                .ok()
                .filter(|n| (MIN_KEEP..=MAX_KEEP_RELEASES).contains(n))
                .ok_or(KEEP_RANGE)?,
            None => DEFAULT_KEEP,
        };
        if frontend_container && !units.is_empty() {
            return Err(
                "--frontend-container and --restart-unit are not given together".to_owned(),
            );
        }
        let layout = match layout {
            Some(name) => Layout::named(&name).ok_or("--layout must be installer, deb or rpm")?,
            None => Layout::default(),
        };
        if frontend_container && layout != Layout::Installer {
            return Err(
                "--frontend-container and --layout deb or rpm are not given together".to_owned(),
            );
        }
        let frontend_config = match frontend_config {
            Some(_) if !frontend_container => {
                return Err("--frontend-config needs --frontend-container".to_owned());
            }
            Some(path) if !Path::new(&path).is_absolute() => {
                return Err("--frontend-config must be an absolute path".to_owned());
            }
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(super::frontend::config::CONFIG),
        };
        Ok(Self {
            socket,
            user,
            window_secs,
            keep_releases,
            layout,
            units,
            frontend_container,
            frontend_config,
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
/// made with mode 0600 and owned by the account. The process umask is never changed.
///
/// The socket is bound at `<directory>/.bind/s` before it is moved to `path`, so that path
/// needs room for it within the unix socket path limit (107 bytes on Linux): the directory's
/// length plus 8, which is no more than `path` itself when the socket's name has 7 bytes or
/// more.
///
/// # Errors
/// Why the socket cannot be made.
pub fn bind(trust: &Trust, path: &Path, ids: (u32, u32)) -> io::Result<UnixListener> {
    // The file is moved into place by name, so only a bind at the staged path would notice a
    // path no client could connect to.
    if path.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(io::Error::other(format!(
            "the socket path is longer than the {SOCKET_PATH_MAX} bytes a unix socket may have"
        )));
    }
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
    // Nobody can connect between the bind and the chown: the file is made inside a directory
    // of mode 0700 that is root's, then given its owner and mode, then moved into place. The
    // process umask is not touched: it belongs to every thread, and a thread that makes a
    // directory while it is 0o177 makes one that nobody can enter.
    let staging = parent.join(STAGING);
    match std::fs::remove_dir_all(&staging) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let made = bind_staged(&staging, path, ids);
    let _ = std::fs::remove_dir_all(&staging);
    made
}

/// The name, in the socket's directory, of the private directory a socket is made in.
pub(super) const STAGING: &str = ".bind";

/// The longest path a unix socket may have on Linux (`sun_path` is 108 bytes, the last one a
/// NUL).
const SOCKET_PATH_MAX: usize = 107;

/// The name the socket has inside [`STAGING`]: a short one, so that the staged path is no
/// longer than the socket's own when the socket's name has 7 bytes or more.
const STAGED: &str = "s";

/// Bind the socket in `staging` (private to this process), give it its owner and mode 0600,
/// and move it to `path`.
fn bind_staged(staging: &Path, path: &Path, ids: (u32, u32)) -> io::Result<UnixListener> {
    path.file_name()
        .ok_or_else(|| io::Error::other("the socket has no name"))?;
    let made = staging.join(STAGED);
    let listener = UnixListener::bind(&made)?;
    chown(&made, Some(ids.0), Some(ids.1))?;
    std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&made, path)?;
    Ok(listener)
}

/// What a connection needs to be answered and audited.
pub struct Serving<'a, B: Backend> {
    pub ab: &'a Ab<B>,
    pub trust: &'a Trust,
    pub audit: &'a Path,
    /// Where the log of what happened goes (stderr, in production).
    pub log: &'a dyn Fn(&str),
    /// The clock of the loop's pauses and of a client's window.
    pub pace: &'a Pace,
}

impl<B: Backend> Serving<'_, B> {
    /// Read the request line from `stream`, answer it, and audit it.
    pub fn serve_one(&self, stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let line = read_line(&stream, IO_TIMEOUT, &self.pace.now);
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
            Some(LocalRequest::Switch { version } | LocalRequest::Confirm { version, .. }) => {
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

    /// Go back, if a release on trial is past its deadline; audited like a revert. Whether it
    /// tried and failed, so that the loop leaves it alone for a while.
    pub fn tick(&self) -> bool {
        let Some(answer) = self.ab.tick() else {
            return false;
        };
        (self.log)(&format!("the deadline passed: {}", answer.json_line()));
        let log = Audit::open(self.trust, self.audit);
        self.record(Some(&LocalRequest::Revert), None, &answer, log);
        !matches!(answer, Answer::Reverted { .. })
    }

    /// Serve until `stop` is set: the connections, and the ticks between them.
    pub fn run(&self, listener: &UnixListener, stop: &AtomicBool) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        let mut hold_revert_until = Duration::ZERO;
        while !stop.load(Ordering::Relaxed) {
            if (self.pace.now)() >= hold_revert_until && self.tick() {
                hold_revert_until = (self.pace.now)() + REVERT_RETRY;
            }
            match listener.accept() {
                Ok((stream, _)) => self.serve_one(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (self.pace.sleep)(IDLE),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The first line the client sent, within [`MAX_LINE_LEN`] bytes and within `window` in all
/// (a client that trickles bytes is cut off, not waited for), without its end; none when it
/// sent none in time, a longer one, or text that is not UTF-8.
pub(super) fn read_line(stream: &UnixStream, window: Duration, now: &Now) -> Option<String> {
    let end = now() + window;
    let mut line = Vec::new();
    let mut chunk = [0u8; 64];
    loop {
        // A zero timeout is an error: a deadline that has passed ends the wait there.
        let left = end.saturating_sub(now());
        let got = stream
            .set_read_timeout(Some(left))
            .and_then(|()| (&*stream).read(&mut chunk))
            .ok()?;
        let chunk = chunk.get(..got).filter(|c| !c.is_empty())?;
        let (head, found) = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or((chunk, false), |at| (&chunk[..at], true));
        line.extend_from_slice(head);
        if line.len() > MAX_LINE_LEN {
            return None;
        }
        if found {
            return String::from_utf8(line).ok();
        }
    }
}
