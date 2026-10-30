//! The listeners of a build node: the main one (`[server] listen`), and the two that jobs reach it
//! on, `bridge_listen` and `listen_unix`.
//!
//! All of them are accepted by one [`Listeners`], which is the source of connections of one
//! [`Server`]: a connection from any of them is served by the same front, with the same keys,
//! the same checks and the same cache, and the drain, the header timeout, the idle timeouts and
//! the limits on bodies are the server's, so they hold for every listener exactly as for the
//! main one. `GET /healthz`, `/readyz` and `/metricz` are answered on each.
//!
//! - `bridge_listen = "ADDR:PORT"` is a TCP listener on one address: the address of the job
//!   network's bridge on the host. It is plain HTTP, as the main listener is. It names one
//!   address, so `0.0.0.0` and `::` are refused; a wildcard would be open on every network the
//!   host is on.
//! - `listen_unix = "/path/to.sock"` is a Unix-domain socket, made with mode 0660 before it
//!   listens (a client cannot connect in between), and with the group `listen_unix_group` if one
//!   is given, a name from `/etc/group`. A file at the path that is a socket is replaced (the
//!   socket of a process that did not clean up); a file of any other type, a symbolic link
//!   included, is never replaced, and the start fails. The socket is removed when the listeners
//!   stop accepting, which is the start of the drain, unless the file at the path is no longer
//!   the one this process made (a process that started since has replaced it).
//!
//! Both are for the build-node shape only.
//!
//! # Signing over a socket
//!
//! A request to the socket is signed as to any other listener. The server checks the signature
//! over the `Host` header as the client sent it, and holds that header to no value, so a client
//! sends any host name it likes and signs it: `localhost` by convention, with path-style
//! addressing (`/<bucket>/<key>`), no TLS, and the keys the main listener takes. Nothing about
//! the signature changes. What authorizes a connection to the socket is the mode, owner and
//! group of the file, and who can reach it, not an address.
//!
//! A connection over the socket has no peer address; a line of the log about one names
//! `127.0.0.1:0`.

use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};

use rustix::fs::{Gid, chown};
use rustix::net::{
    AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, listen, socket_with,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};

use super::cli::Error;
use super::config::{ConfigError, invalid};
use super::{Drained, Incoming, Server};

/// The mode of the socket file.
const SOCKET_MODE: u32 = 0o660;

/// The queue of connections the socket may hold that are not accepted yet (the system caps it).
const BACKLOG: i32 = 1024;

/// Where the groups of the system are named.
const GROUP_FILE: &str = "/etc/group";

/// The address that a connection over the socket is given as its peer, which only the log shows.
const UNIX_PEER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

/// The listeners that the settings name besides the main one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct JobListeners {
    /// `bridge_listen`: one address, not a wildcard.
    pub bridge: Option<SocketAddr>,
    /// `listen_unix` and `listen_unix_group`.
    pub unix: Option<UnixSocket>,
}

/// The Unix-domain socket of the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnixSocket {
    /// The absolute path of the socket.
    pub path: PathBuf,
    /// The group that owns the socket file, if one is named.
    pub group: Option<String>,
    /// The file the group is looked up in: `/etc/group`; the tests point it at a file of their own.
    pub group_file: PathBuf,
}

impl JobListeners {
    /// The listeners the keys of `[server]` name, if they are keys a start can use. `main` is the
    /// address of `listen`.
    ///
    /// # Errors
    /// A [`ConfigError`] that names the key: a wildcard or the main address for `bridge_listen`, a
    /// path that is not absolute or names no file for `listen_unix`, a group that is empty or has
    /// no socket to own.
    pub(super) fn new(
        main: SocketAddr,
        bridge: Option<SocketAddr>,
        path: Option<PathBuf>,
        group: Option<String>,
    ) -> Result<Self, ConfigError> {
        if let Some(address) = bridge {
            if address.ip().to_canonical().is_unspecified() {
                let why = "it must name one address; 0.0.0.0 and :: are open on every network";
                return Err(invalid("server.bridge_listen", why));
            }
            if address == main && address.port() != 0 {
                let why = "it is the address of listen; give another";
                return Err(invalid("server.bridge_listen", why));
            }
        }
        let unix = match (path, group) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(invalid(
                    "server.listen_unix_group",
                    "it is the group of the socket of listen_unix, which is not given",
                ));
            }
            (Some(path), group) => {
                if !path.is_absolute() || path.file_name().is_none() {
                    let why = "it must be the absolute path of a socket file";
                    return Err(invalid("server.listen_unix", why));
                }
                if group.as_deref() == Some("") {
                    let why = "it must name a group, or be left out";
                    return Err(invalid("server.listen_unix_group", why));
                }
                let group_file = PathBuf::from(GROUP_FILE);
                Some(UnixSocket {
                    path,
                    group,
                    group_file,
                })
            }
        };
        Ok(Self { bridge, unix })
    }

    /// What the start line says of these listeners after `listen=`: the fields `bridge=` and
    /// `unix=` for the ones that are set, each with a space before it.
    pub(super) fn announce(&self) -> String {
        let bridge = self.bridge.map(|address| format!(" bridge={address}"));
        let unix = self
            .unix
            .as_ref()
            .map(|socket| format!(" unix={}", socket.path.display()));
        bridge.into_iter().chain(unix).collect()
    }
}

/// Where a start is listening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bound {
    /// The main address.
    pub main: SocketAddr,
    /// The bridge address.
    pub bridge: Option<SocketAddr>,
    /// The socket.
    pub unix: Option<PathBuf>,
}

/// A byte stream of a connection, of whichever listener it came from.
pub(super) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// A connection of any listener.
type Conn = Box<dyn Duplex>;

/// The socket file of a listener, removed when it is dropped if it is still the file that was
/// made: a process that started since and replaced the file owns the new one.
struct SocketFile {
    path: PathBuf,
    /// The device and the inode of the file that was made.
    made: (u64, u64),
}

impl SocketFile {
    /// The file at `path`, which the process has just made.
    fn claim(path: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.to_owned(),
            made: (meta.dev(), meta.ino()),
        })
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        // A file system may give the number of a removed file to the next it makes: the type
        // tells a socket from the file that took its place.
        let ours = std::fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.file_type().is_socket() && (meta.dev(), meta.ino()) == self.made
        });
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A listener and what it must clean up.
enum Source {
    Tcp(TcpListener),
    /// The listener drops before the file is removed: a field is dropped in the order it is
    /// declared.
    Unix {
        listener: UnixListener,
        _file: SocketFile,
    },
}

impl Source {
    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(Conn, SocketAddr)>> {
        match self {
            Source::Tcp(listener) => listener
                .poll_accept(cx)
                .map_ok(|(stream, peer)| (Box::new(stream) as Conn, peer)),
            Source::Unix { listener, .. } => listener
                .poll_accept(cx)
                .map_ok(|(stream, _)| (Box::new(stream) as Conn, UNIX_PEER)),
        }
    }
}

/// The listeners of a start, taken by [`Server::serve_listeners`]. They are taken in turn: the
/// one after the last that gave a connection is asked first, so that a busy listener does not
/// starve the others.
pub(super) struct Listeners {
    sources: Vec<Source>,
    next: usize,
    bound: Bound,
}

/// Binds `address` and says where, naming `what` in the error.
async fn bind_tcp(address: SocketAddr, what: &str) -> Result<(TcpListener, SocketAddr), String> {
    let made = async {
        let listener = TcpListener::bind(address).await?;
        let local = listener.local_addr()?;
        Ok((listener, local))
    };
    let made: io::Result<_> = made.await;
    made.map_err(|why| format!("binding {what}{address}: {why}"))
}

impl Listeners {
    /// Binds `main` and the listeners of `jobs`. A listener that cannot be made is the error, and
    /// the ones made before it are closed.
    ///
    /// # Errors
    /// The address or the socket that could not be made, and why.
    pub(super) async fn bind(main: SocketAddr, jobs: &JobListeners) -> Result<Self, Error> {
        let (first, address) = bind_tcp(main, "").await?;
        let mut sources = vec![Source::Tcp(first)];
        let mut bound = Bound {
            main: address,
            bridge: None,
            unix: None,
        };
        if let Some(bridge) = jobs.bridge {
            let (listener, address) = bind_tcp(bridge, "bridge_listen ").await?;
            sources.push(Source::Tcp(listener));
            bound.bridge = Some(address);
        }
        if let Some(socket) = &jobs.unix {
            let (listener, file) = bind_unix(socket)?;
            sources.push(Source::Unix {
                listener,
                _file: file,
            });
            bound.unix = Some(socket.path.clone());
        }
        Ok(Self {
            sources,
            next: 0,
            bound,
        })
    }

    /// Where the listeners are.
    pub(super) fn bound(&self) -> &Bound {
        &self.bound
    }
}

impl Incoming for Listeners {
    type Stream = Conn;

    fn poll_incoming(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(Conn, SocketAddr)>> {
        let count = self.sources.len();
        for step in 0..count {
            let at = (self.next + step) % count;
            if let Poll::Ready(accepted) = self.sources[at].poll_accept(cx) {
                self.next = (at + 1) % count;
                return Poll::Ready(accepted);
            }
        }
        Poll::Pending
    }
}

impl Server {
    /// [`Server::serve`] over the connections of every one of `listeners`: the drain begins when
    /// [`Server::begin_drain`] is called, runs as for one listener, and closes them all.
    pub(super) async fn serve_listeners(self: &Arc<Self>, listeners: Listeners) -> Drained {
        self.serve_from(listeners).await
    }
}

/// The id of the group `name` in `file`, a file in the format of `/etc/group`.
fn group_id(name: &str, file: &Path) -> Result<u32, String> {
    let shown = file.display();
    let text = std::fs::read_to_string(file).map_err(|why| format!("reading {shown}: {why}"))?;
    let found = text.lines().find_map(|line| {
        let (entry, rest) = line.split_once(':')?;
        let gid = rest.split(':').nth(1)?;
        // All ones is the id that means "no change" to chown.
        gid.parse::<u32>()
            .ok()
            .filter(|gid| entry == name && *gid != u32::MAX)
    });
    found.ok_or_else(|| format!("listen_unix_group {name:?} is not a group in {shown}"))
}

/// Removes the file at `path` if it is a socket, and refuses to if it is anything else.
fn clear_stale(path: &Path) -> Result<(), String> {
    let shown = path.display();
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => {
            return Err(format!(
                "listen_unix {shown}: the file exists and is not a socket; it is not replaced"
            ));
        }
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(why) => Err(why),
    };
    removed.map_err(|why| format!("listen_unix {shown}: {why}"))
}

/// Makes the socket of `socket`: mode 0660 and its group are set after the file is made and
/// before the socket listens, so that no client connects to it with another mode.
fn bind_unix(socket: &UnixSocket) -> Result<(UnixListener, SocketFile), String> {
    let path = socket.path.as_path();
    let gid = match &socket.group {
        Some(name) => Some(group_id(name, &socket.group_file)?),
        None => None,
    };
    clear_stale(path)?;
    make_socket(path, gid).map_err(|why| format!("listen_unix {}: {why}", path.display()))
}

/// The steps of [`bind_unix`] that touch the file system and the socket, in the order that
/// keeps the socket closed to clients until its mode and group are set.
fn make_socket(path: &Path, gid: Option<u32>) -> io::Result<(UnixListener, SocketFile)> {
    let flags = SocketFlags::NONBLOCK | SocketFlags::CLOEXEC;
    let fd = socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None)?;
    bind(&fd, &SocketAddrUnix::new(path)?)?;
    let file = SocketFile::claim(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    if let Some(gid) = gid {
        chown(path, None, Some(Gid::from_raw_unchecked(gid)))
            .map_err(|why| io::Error::other(format!("changing the group: {why}")))?;
    }
    listen(&fd, BACKLOG)?;
    let listener = UnixListener::from_std(std::os::unix::net::UnixListener::from(fd))?;
    Ok((listener, file))
}

#[cfg(test)]
#[path = "listeners_tests.rs"]
mod tests;
