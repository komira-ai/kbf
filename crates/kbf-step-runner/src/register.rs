//! The registration socket: what a running service tells the updater, and no more
//! (docs/design/host-updates.md, 2.1 to 2.3).
//!
//! A service connects to the socket and sends one JSON line: its name, the version it runs and,
//! optionally, a loopback address and path that answer 2xx when it is ready. That is all a
//! registration carries; there is no command, no path to run, no right and no parameter of a
//! hook in it, so a process that reaches the socket can claim to be alive and can make the
//! updater run nothing. The connection is the liveness: when it closes the service is
//! `stopped`.
//!
//! Who may register what is the declaration's ([`Declared`]): the peer's user id, from
//! `SO_PEERCRED`, must be the one the declaration names for that name, and the health address
//! must be loopback with a port the declaration grants.
//!
//! What the updater reports for a service ([`Registry::report`]):
//!
//! | registered | running version | reported |
//! |---|---|---|
//! | never | | `Absent` |
//! | connected | the staged version | `Healthy` |
//! | connected | another version | `Unhealthy(VersionMismatch)`: never believed |
//! | connection closed | | `Unhealthy(Stopped)` |
//! | loaded from disk, not reconnected | | as last known, `Unhealthy(Stopped)` after `registration_ttl_secs` |

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::artifact::host_owner;
use crate::clock::Clock;
use crate::record::write_atomic;

/// The registrations, in the updater's state directory.
pub const REGISTRATIONS_FILE: &str = "registrations.json";
/// Starts the thread of a connection.
pub type Spawn = Box<dyn Fn(Box<dyn FnOnce() + Send>) -> io::Result<()> + Send + Sync>;
/// Writes the registrations (the bytes of [`REGISTRATIONS_FILE`]) into the state directory,
/// durably.
pub type Writer = Box<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;
/// The connections a server serves at once unless told otherwise.
pub const MAX_CONNECTIONS: usize = 64;
/// The connections of one user served at once unless told otherwise.
pub const MAX_PER_UID: usize = 8;
/// The time a connection has, in total, to send its registration line. A test never waits for it.
pub const LINE_DEADLINE_SECS: u64 = 10;
/// The pause before accepting again after an error that passes (out of descriptors).
pub const ACCEPT_RETRY_MS: u64 = 50;
/// The longest line a registration may be.
pub const MAX_LINE: usize = 4096;

/// A loopback address and a path that answer 2xx when the service is ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthAddr {
    pub addr: String,
    pub path: String,
}

/// The message of a service: liveness and version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub health: Option<HealthAddr>,
}

/// What the declaration of a service allows it to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The user id the service runs as and registers as.
    pub uid: u32,
    /// The loopback ports a health address may name.
    pub health_ports: Vec<u16>,
    /// How long a registration loaded from disk stays believed.
    pub registration_ttl_secs: u64,
}

/// The declarations, as far as registration reads them.
pub trait Declared: Send + Sync {
    /// The grant of a declared service, `None` for a name nothing declares.
    fn grant(&self, name: &str) -> Option<Grant>;
}

/// Why a registration was refused; the token goes back to the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the registration is not one JSON object within the size limit")]
    Malformed,
    #[error("no declaration names that service")]
    UnknownService,
    #[error("the declaration names another user for that service")]
    WrongUser,
    #[error("the health address is not a loopback address with a path")]
    HealthAddress,
    #[error("the declaration does not grant that health port")]
    HealthPort,
    #[error("the server holds as many connections as it allows")]
    Busy,
}

impl Refused {
    pub const fn token(self) -> &'static str {
        match self {
            Refused::Malformed => "malformed",
            Refused::UnknownService => "unknown_service",
            Refused::WrongUser => "wrong_user",
            Refused::HealthAddress => "health_address",
            Refused::HealthPort => "health_port",
            Refused::Busy => "busy",
        }
    }
}

/// Why the registry or the server failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error("the registrations are not readable: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the socket directory {0:?} is not a directory of the expected owner closed to others")]
    SocketDir(PathBuf),
    #[error("a registration server already answers on {0:?}")]
    AlreadyServing(PathBuf),
    #[error(
        "{0:?} is in the way of the registration socket: it is not a stale socket of the updater"
    )]
    InTheWay(PathBuf),
}

/// What the updater reports for a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reported {
    /// Declared and never registered.
    Absent,
    Healthy,
    Unhealthy(Why),
}

/// Why a registered service is reported unhealthy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    Stopped,
    /// It runs another version than the one staged.
    VersionMismatch {
        running: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    Live,
    Stopped,
    /// Loaded from disk at this second of the monotonic clock, not yet reconnected.
    Unconfirmed {
        since: u64,
    },
}

struct Entry {
    uid: u32,
    version: String,
    health: Option<HealthAddr>,
    ttl_secs: u64,
    presence: Presence,
    conn: u64,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    name: String,
    uid: u32,
    version: String,
    health: Option<HealthAddr>,
    ttl_secs: u64,
    #[serde(default)]
    stopped: bool,
}

#[derive(Default)]
struct Inner {
    next_conn: u64,
    entries: BTreeMap<String, Entry>,
}

/// The accepted registrations.
pub struct Registry {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
    writer: Writer,
}

impl Registry {
    /// Loads the registrations of `dir`; every one is `unconfirmed` until its service connects.
    pub fn load(
        dir: &Path,
        clock: Arc<dyn Clock>,
        _declared: &dyn Declared,
    ) -> Result<Registry, RegistryError> {
        let since = clock.mono_secs();
        let mut inner = Inner::default();
        match fs::read(dir.join(REGISTRATIONS_FILE)) {
            Ok(bytes) => {
                let saved: Vec<Saved> = serde_json::from_slice(&bytes)
                    .map_err(|e| RegistryError::Corrupt(e.to_string()))?;
                for s in saved {
                    inner.entries.insert(
                        s.name,
                        Entry {
                            uid: s.uid,
                            version: s.version,
                            health: s.health,
                            ttl_secs: s.ttl_secs,
                            presence: if s.stopped {
                                Presence::Stopped
                            } else {
                                Presence::Unconfirmed { since }
                            },
                            conn: 0,
                        },
                    );
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Registry {
            dir: dir.to_path_buf(),
            clock,
            inner: Mutex::new(inner),
            writer: Box::new(|dir, bytes| write_atomic(dir, REGISTRATIONS_FILE, bytes)),
        })
    }

    /// The same registry, writing through `writer` (a test makes the disk slow or failing).
    pub fn with_writer(mut self, writer: Writer) -> Registry {
        self.writer = writer;
        self
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accepts a registration from a peer of user id `peer_uid`; returns the id of the
    /// connection that now holds it. A second registration of a name replaces the first.
    pub fn register(
        &self,
        peer_uid: u32,
        registration: &Registration,
        grant: &Grant,
    ) -> Result<u64, RegistryError> {
        if peer_uid != grant.uid {
            return Err(Refused::WrongUser.into());
        }
        if let Some(health) = &registration.health {
            check_health(health, grant)?;
        }
        let mut inner = self.inner();
        inner.next_conn += 1;
        let conn = inner.next_conn;
        let before = inner.entries.insert(
            registration.name.clone(),
            Entry {
                uid: grant.uid,
                version: registration.version.clone(),
                health: registration.health.clone(),
                ttl_secs: grant.registration_ttl_secs,
                presence: Presence::Live,
                conn,
            },
        );
        if let Err(e) = self.persist(&inner) {
            // Not kept, so not believed: the registration that stood is back, or none.
            inner.entries.remove(&registration.name);
            inner
                .entries
                .extend(before.map(|old| (registration.name.clone(), old)));
            return Err(e);
        }
        Ok(conn)
    }

    /// Writes the registrations, durably; a service whose connection closed is written stopped.
    fn persist(&self, inner: &Inner) -> Result<(), RegistryError> {
        let saved: Vec<Saved> = inner
            .entries
            .iter()
            .map(|(name, e)| Saved {
                name: name.clone(),
                uid: e.uid,
                version: e.version.clone(),
                health: e.health.clone(),
                ttl_secs: e.ttl_secs,
                stopped: e.presence == Presence::Stopped,
            })
            .collect();
        let bytes =
            serde_json::to_vec(&saved).map_err(|e| RegistryError::Corrupt(e.to_string()))?;
        Ok((self.writer)(&self.dir, &bytes)?)
    }

    /// The connection `conn` closed: its service is stopped, unless a later registration of
    /// the same name has replaced it.
    /// The stop is written, so a registration loaded after a restart is stopped too.
    pub fn closed(&self, name: &str, conn: u64) -> Result<(), RegistryError> {
        let mut inner = self.inner();
        let held = inner.entries.get_mut(name).filter(|e| e.conn == conn);
        let Some(entry) = held else {
            return Ok(());
        };
        entry.presence = Presence::Stopped;
        self.persist(&inner)
    }

    /// What to report for `name`, whose staged version is `expected`.
    pub fn report(&self, name: &str, expected: Option<&str>) -> Reported {
        let inner = self.inner();
        let Some(entry) = inner.entries.get(name) else {
            return Reported::Absent;
        };
        let stopped = match entry.presence {
            Presence::Live => false,
            Presence::Stopped => true,
            Presence::Unconfirmed { since } => {
                self.clock.mono_secs().saturating_sub(since) >= entry.ttl_secs
            }
        };
        if stopped {
            Reported::Unhealthy(Why::Stopped)
        } else if expected.is_some_and(|version| version != entry.version) {
            Reported::Unhealthy(Why::VersionMismatch {
                running: entry.version.clone(),
            })
        } else {
            Reported::Healthy
        }
    }

    /// The health address a service registered, for the updater's `GET`.
    pub fn health_addr(&self, name: &str) -> Option<HealthAddr> {
        self.inner().entries.get(name)?.health.clone()
    }
}

fn check_health(health: &HealthAddr, grant: &Grant) -> Result<(), Refused> {
    let addr: SocketAddr = health.addr.parse().map_err(|_| Refused::HealthAddress)?;
    let plain_path = health.path.starts_with('/')
        && health
            .path
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'?' && b != b'#');
    if !addr.ip().is_loopback() || !plain_path {
        return Err(Refused::HealthAddress);
    }
    if !grant.health_ports.contains(&addr.port()) {
        return Err(Refused::HealthPort);
    }
    Ok(())
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
    error: Option<&'static str>,
}

/// The registration socket.
pub struct Server {
    listener: UnixListener,
    registry: Arc<Registry>,
    declared: Arc<dyn Declared>,
    max_connections: usize,
    max_per_uid: usize,
    line_deadline: Duration,
    spawner: Spawn,
    slots: Mutex<Slots>,
}

/// The connections being served, in all and by user.
#[derive(Default)]
struct Slots {
    total: usize,
    by_uid: BTreeMap<u32, usize>,
}

/// A connection's place among the served ones, given back when it is dropped.
struct Slot {
    server: Arc<Server>,
    uid: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self
            .server
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        slots.total -= 1;
        let mine = slots.by_uid.entry(self.uid).or_default();
        *mine -= 1;
        if *mine == 0 {
            slots.by_uid.remove(&self.uid);
        }
    }
}

impl Server {
    /// Binds the socket at `path`.
    ///
    /// Its directory is checked with [`check_socket_dir`]: in production it is root's, group
    /// `updater-register`, mode 0750. The socket is made mode 0660 and owned by `group`. A stale
    /// socket file (nobody answers) is replaced; one that answers is [`RegistryError::AlreadyServing`].
    pub fn bind(
        path: &Path,
        owner: u32,
        group: u32,
        registry: Arc<Registry>,
        declared: Arc<dyn Declared>,
    ) -> Result<Server, RegistryError> {
        let dir = path.parent().unwrap_or(Path::new("/"));
        check_socket_dir(dir, owner, group, &host_owner)?;
        match UnixStream::connect(path) {
            Ok(_) => return Err(RegistryError::AlreadyServing(path.to_path_buf())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)?,
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
        rustix::fs::chown(path, None, Some(rustix::fs::Gid::from_raw(group)))
            .map_err(io::Error::from)?;
        Ok(Server {
            listener,
            registry,
            declared,
            max_connections: MAX_CONNECTIONS,
            max_per_uid: MAX_PER_UID,
            line_deadline: Duration::from_secs(LINE_DEADLINE_SECS),
            spawner: Box::new(|job| std::thread::Builder::new().spawn(job).map(|_| ())),
            slots: Mutex::default(),
        })
    }

    /// The most connections served at once; more are answered `busy` and closed.
    pub fn with_max_connections(mut self, max: usize) -> Server {
        self.max_connections = max;
        self
    }

    /// The most connections of one user served at once.
    pub fn with_max_per_uid(mut self, max: usize) -> Server {
        self.max_per_uid = max;
        self
    }

    /// The time a connection has to send its registration line, in total.
    pub fn with_line_deadline(mut self, deadline: Duration) -> Server {
        self.line_deadline = deadline;
        self
    }

    /// How a connection gets its thread (a test makes it fail).
    pub fn with_spawner(mut self, spawn: Spawn) -> Server {
        self.spawner = spawn;
        self
    }

    /// Accepts connections, one thread each, until `stop` is set. The caller sets it and then
    /// makes one connection, which wakes the accept.
    pub fn serve(self: &Arc<Server>, stop: &AtomicBool) -> io::Result<()> {
        self.serve_with(
            &mut || self.listener.accept().map(|(stream, _)| stream),
            stop,
        )
    }

    /// [`Server::serve`] over any source of connections. An error that passes (out of
    /// descriptors, an interrupted call) is waited out and the loop goes on; any other ends it.
    pub fn serve_with(
        self: &Arc<Server>,
        accept: &mut dyn FnMut() -> io::Result<UnixStream>,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        loop {
            let stream = match accept() {
                Ok(stream) => stream,
                Err(e) if is_transient(&e) => {
                    std::thread::sleep(Duration::from_millis(ACCEPT_RETRY_MS));
                    continue;
                }
                Err(e) => return Err(e),
            };
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            self.dispatch(stream);
        }
    }

    /// Gives a connection its slot and its thread, or answers it `busy`.
    fn dispatch(self: &Arc<Server>, stream: UnixStream) {
        // A peer whose user cannot be read counts under no user in particular.
        let uid = peer_uid(&stream).unwrap_or(u32::MAX);
        let Some(slot) = self.admit(uid) else {
            let _ = reply(&stream, Some(Refused::Busy.token()));
            return;
        };
        let spare = stream.try_clone().ok();
        let server = Arc::clone(self);
        let job = Box::new(move || {
            let _slot = slot;
            let _ = server.serve_connection(stream);
        });
        if (self.spawner)(job).is_err() {
            // The job, and with it the slot, is dropped; the peer is told.
            let _ = spare
                .as_ref()
                .map(|s| reply(s, Some(Refused::Busy.token())));
        }
    }

    fn admit(self: &Arc<Server>, uid: u32) -> Option<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let mine = slots.by_uid.get(&uid).copied().unwrap_or(0);
        if slots.total >= self.max_connections || mine >= self.max_per_uid {
            return None;
        }
        slots.total += 1;
        slots.by_uid.insert(uid, mine + 1);
        Some(Slot {
            server: Arc::clone(self),
            uid,
        })
    }

    /// The registration line: up to [`MAX_LINE`] bytes, ended by a newline or by the peer closing
    /// its writing side, within the line deadline. `None` when it is too long or too slow.
    fn read_line(&self, stream: &UnixStream) -> io::Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + self.line_deadline;
        let mut line = Vec::new();
        let mut chunk = [0u8; 512];
        let mut from = stream;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            stream.set_read_timeout(Some(left))?;
            // A read that fails (the time ran out, the peer vanished) is the end of the line.
            let n = from.read(&mut chunk).unwrap_or(0);
            line.extend_from_slice(&chunk[..n]);
            if line.len() > MAX_LINE {
                return Ok(None);
            }
            if let Some(end) = line.iter().position(|b| *b == b'\n') {
                line.truncate(end);
                return Ok(Some(line));
            }
            if n == 0 {
                return Ok(Some(line));
            }
        }
    }

    /// Serves one connection: reads the registration, answers it, and holds the registration
    /// until the peer closes.
    pub fn serve_connection(&self, stream: UnixStream) -> Result<(), RegistryError> {
        let peer = peer_uid(&stream)?;
        let registration = self
            .read_line(&stream)?
            .and_then(|line| String::from_utf8(line).ok())
            .and_then(|text| serde_json::from_str::<Registration>(&text).ok());
        let outcome = match registration {
            None => Err(Refused::Malformed.into()),
            Some(registration) => self.accept(peer, registration),
        };
        let (name, conn) = match outcome {
            Ok(held) => held,
            Err(RegistryError::Refused(refused)) => {
                return reply(&stream, Some(refused.token()));
            }
            Err(e) => {
                reply(&stream, Some("internal"))?;
                return Err(e);
            }
        };
        reply(&stream, None)?;
        // The connection is the liveness: wait for the peer to close it. What it sends meanwhile
        // is read and dropped, up to one more line's worth; a peer that sends more is cut off.
        stream.set_read_timeout(None)?;
        let mut sink = [0u8; 256];
        let mut rest = &stream;
        let mut budget = MAX_LINE;
        while budget > 0 {
            match rest.read(&mut sink) {
                Ok(n) if n > 0 => budget = budget.saturating_sub(n),
                _ => break,
            }
        }
        self.registry.closed(&name, conn)
    }

    fn accept(
        &self,
        peer: u32,
        registration: Registration,
    ) -> Result<(String, u64), RegistryError> {
        let grant = self
            .declared
            .grant(&registration.name)
            .ok_or(Refused::UnknownService)?;
        let conn = self.registry.register(peer, &registration, &grant)?;
        Ok((registration.name, conn))
    }
}

/// Checks the directory of the registration socket: a real directory owned by `owner` and
/// closed to others (mode without any `o` bit), reading the owner of a directory (user id, group
/// id) from `read`.
pub fn check_socket_dir(
    dir: &Path,
    owner: u32,
    _group: u32,
    read: &dyn Fn(&fs::Metadata) -> (u32, u32),
) -> Result<(), RegistryError> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || read(&meta).0 != owner || meta.mode() & 0o027 != 0 {
        return Err(RegistryError::SocketDir(dir.to_path_buf()));
    }
    Ok(())
}

fn reply(stream: &UnixStream, error: Option<&'static str>) -> Result<(), RegistryError> {
    let mut line = serde_json::to_vec(&Reply {
        ok: error.is_none(),
        error,
    })
    .unwrap_or_default();
    line.push(b'\n');
    let mut out = stream;
    Ok(out.write_all(&line)?)
}

/// Whether an accept error is one that passes: an interrupted call, a connection that went away,
/// or the host running out of descriptors or memory for a moment.
fn is_transient(e: &io::Error) -> bool {
    use rustix::io::Errno;
    let passing = [Errno::MFILE, Errno::NFILE, Errno::NOBUFS, Errno::NOMEM];
    let kinds = [
        io::ErrorKind::Interrupted,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::WouldBlock,
    ];
    kinds.contains(&e.kind())
        || e.raw_os_error()
            .is_some_and(|code| passing.iter().any(|p| p.raw_os_error() == code))
}

/// The user id of the process that made the connection, as the kernel recorded it
/// (`SO_PEERCRED`).
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

/// Without `SO_PEERCRED` no peer can be told from another: none is admitted.
#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}
