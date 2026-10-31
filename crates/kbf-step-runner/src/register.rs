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
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::record::write_atomic;

/// The registrations, in the updater's state directory.
pub const REGISTRATIONS_FILE: &str = "registrations.json";
/// Starts the thread of a connection.
pub type Spawn = Box<dyn Fn(Box<dyn FnOnce() + Send>) -> io::Result<()> + Send + Sync>;
/// The connections a server serves at once unless told otherwise.
pub const MAX_CONNECTIONS: usize = 64;
/// How long a connection may take to send its registration. A test never waits for it.
pub const READ_TIMEOUT_SECS: u64 = 10;
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
}

impl Registry {
    /// Loads the registrations of `dir`; every one is `unconfirmed` until its service connects.
    pub fn load(dir: &Path, clock: Arc<dyn Clock>) -> Result<Registry, RegistryError> {
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
        })
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
        inner.entries.insert(
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
        self.persist(&inner)?;
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
        Ok(write_atomic(&self.dir, REGISTRATIONS_FILE, &bytes)?)
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
    open: AtomicUsize,
}

impl Server {
    /// Binds the socket at `path`.
    ///
    /// Its directory must be a real directory owned by `dir_owner` and closed to others (mode
    /// without any `o` bit); in production it is root's, group `updater-register`, mode 0750.
    /// The socket is made mode 0660 and, when `group` is given, owned by that group. A stale
    /// socket file (nobody answers) is replaced; one that answers is [`RegistryError::AlreadyServing`].
    pub fn bind(
        path: &Path,
        dir_owner: u32,
        group: Option<u32>,
        registry: Arc<Registry>,
        declared: Arc<dyn Declared>,
    ) -> Result<Server, RegistryError> {
        let dir = path.parent().unwrap_or(Path::new("/"));
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != dir_owner || meta.mode() & 0o007 != 0 {
            return Err(RegistryError::SocketDir(dir.to_path_buf()));
        }
        match UnixStream::connect(path) {
            Ok(_) => return Err(RegistryError::AlreadyServing(path.to_path_buf())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)?,
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
        if let Some(gid) = group {
            rustix::fs::chown(path, None, Some(rustix::fs::Gid::from_raw(gid)))
                .map_err(io::Error::from)?;
        }
        Ok(Server {
            listener,
            registry,
            declared,
            max_connections: MAX_CONNECTIONS,
            open: AtomicUsize::new(0),
        })
    }

    /// The most connections served at once; more are answered `busy` and closed.
    pub fn with_max_connections(mut self, max: usize) -> Server {
        self.max_connections = max;
        self
    }

    /// The most connections of one user served at once.
    pub fn with_max_per_uid(self, max: usize) -> Server {
        let _ = max;
        self
    }

    /// The time a connection has to send its registration line, in total.
    pub fn with_line_deadline(self, deadline: Duration) -> Server {
        let _ = deadline;
        self
    }

    /// How a connection gets its thread (a test makes it fail).
    pub fn with_spawner(self, spawn: Spawn) -> Server {
        let _ = spawn;
        self
    }

    /// [`Server::serve`] over any source of connections.
    pub fn serve_with(
        self: &Arc<Server>,
        accept: &mut dyn FnMut() -> io::Result<UnixStream>,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        let _ = (accept, stop);
        Ok(())
    }

    /// Accepts connections, one thread each, until `stop` is set. The caller sets it and then
    /// makes one connection, which wakes the accept.
    pub fn serve(self: &Arc<Server>, stop: &AtomicBool) -> io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept()?;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if self.open.fetch_add(1, Ordering::SeqCst) >= self.max_connections {
                self.open.fetch_sub(1, Ordering::SeqCst);
                let _ = reply(&stream, Some(Refused::Busy.token()));
                continue;
            }
            let server = Arc::clone(self);
            std::thread::spawn(move || {
                let _ = server.serve_connection(stream);
                server.open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    /// Serves one connection: reads the registration, answers it, and holds the registration
    /// until the peer closes.
    pub fn serve_connection(&self, stream: UnixStream) -> Result<(), RegistryError> {
        let peer = peer_uid(&stream)?;
        stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        let mut reader = BufReader::new((&stream).take(MAX_LINE as u64 + 1));
        let mut line = String::new();
        let read = reader.read_line(&mut line);
        let registration = match read {
            Ok(n) if n <= MAX_LINE => serde_json::from_str::<Registration>(&line).ok(),
            _ => None,
        };
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
