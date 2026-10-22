//! Where the API socket comes from.
//!
//! Two ways, and the first is the one production uses:
//!
//! - **Socket activation** ([`Listen::Activated`]). systemd binds the socket, sets its owner
//!   and mode (`SocketUser=<proxy user>`, `SocketMode=0600`) and hands the listening
//!   descriptor to the service as fd 3, with `LISTEN_PID` and `LISTEN_FDS=1` in the
//!   environment. The updater, which runs as its own unprivileged user, cannot make a socket
//!   that the proxy's user owns; this is how the socket is the proxy's. [`Activation`]
//!   decides from the two variables whether the descriptor is for this process, and [`adopt`]
//!   refuses a descriptor that is not a listening unix stream socket.
//! - **Self-binding** ([`Listen::Bind`], the `--socket` flag), for tests and development:
//!   [`bind_socket`] binds the path with mode 0600, owned by the updater's own user.
//!
//! `LISTEN_PID` and `LISTEN_FDS` are set by the platform, not by an operator; they are the
//! one place the updater reads its environment.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The first descriptor systemd passes (`SD_LISTEN_FDS_START`).
pub const FIRST_ACTIVATED_FD: RawFd = 3;

/// Where the listener comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listen {
    /// Bind this path (mode 0600, the updater's own user).
    Bind(PathBuf),
    /// Take this descriptor, which the service manager bound and set the owner and mode of.
    Activated(RawFd),
}

/// What the service manager says about the sockets it passed: `LISTEN_PID` and `LISTEN_FDS`,
/// as the raw text (or unset).
#[derive(Clone, Debug, Default)]
pub struct Activation {
    pub pid: Option<String>,
    pub fds: Option<String>,
}

impl Activation {
    /// The two variables of this process's environment.
    pub fn from_env() -> Activation {
        Activation {
            pid: std::env::var("LISTEN_PID").ok(),
            fds: std::env::var("LISTEN_FDS").ok(),
        }
    }

    /// The descriptor passed to the process with id `me`: none if `LISTEN_PID` is unset; an
    /// error if it names another process (the descriptors are somebody else's, and a stale
    /// environment must not decide which socket the updater serves), if `LISTEN_FDS` is not
    /// exactly `1`, or if it is missing.
    pub fn passed_to(&self, me: u32) -> Result<Option<RawFd>, String> {
        let Some(pid) = self.pid.as_deref() else {
            return Ok(None);
        };
        if pid.parse::<u32>() != Ok(me) {
            return Err(format!(
                "LISTEN_PID is {pid:?} but this process is {me}: the sockets were passed to \
                 another process"
            ));
        }
        match self.fds.as_deref() {
            Some("1") => Ok(Some(FIRST_ACTIVATED_FD)),
            Some(other) => Err(format!(
                "LISTEN_FDS is {other:?}: the updater takes exactly one socket"
            )),
            None => Err("LISTEN_PID is set but LISTEN_FDS is not".to_owned()),
        }
    }
}

/// A listener, and where a connection can be made to wake it.
pub struct Opened {
    pub listener: UnixListener,
    /// The socket's path: the stop request connects to it.
    pub path: PathBuf,
    /// Whether the updater made the file, and so removes it when it stops. An activated
    /// socket belongs to the service manager.
    pub owned: bool,
}

/// Open the listener `listen` names.
pub fn open(listen: &Listen) -> Result<Opened, String> {
    match listen {
        Listen::Bind(path) => {
            let listener =
                bind_socket(path).map_err(|e| format!("socket {}: {e}", path.display()))?;
            Ok(Opened {
                listener,
                path: path.clone(),
                owned: true,
            })
        }
        Listen::Activated(fd) => {
            let listener = adopt(*fd)?;
            let addr = listener
                .local_addr()
                .map_err(|e| format!("activated socket: {e}"))?;
            let path = addr
                .as_pathname()
                .ok_or("the activated socket has no path in the file system")?
                .to_owned();
            Ok(Opened {
                listener,
                path,
                owned: false,
            })
        }
    }
}

/// One integer socket option of `fd`.
fn socket_option(fd: RawFd, option: libc::c_int) -> Result<libc::c_int, String> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live for the call and `len` is the size of `value`; a
    // descriptor that is not open makes the call fail with EBADF, not misbehave.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if rc == 0 {
        Ok(value)
    } else {
        Err(format!(
            "descriptor {fd} is not a socket: {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// Take ownership of `fd` as the listener, if it is a listening unix stream socket; any other
/// descriptor is an error and is left alone (not closed).
pub fn adopt(fd: RawFd) -> Result<UnixListener, String> {
    if socket_option(fd, libc::SO_DOMAIN)? != libc::AF_UNIX {
        return Err(format!("descriptor {fd} is not a unix socket"));
    }
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Err(format!("descriptor {fd} is not a stream socket"));
    }
    if socket_option(fd, libc::SO_ACCEPTCONN)? == 0 {
        return Err(format!("descriptor {fd} is not a listening socket"));
    }
    // SAFETY: the descriptor is open (the options above were read from it), and the
    // caller's contract is that it is this process's to own: systemd passed it to this pid
    // alone (`LISTEN_PID`), or a test made it for the call.
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

/// Bind the API socket at `path`: a stale socket file left by a dead updater is replaced, a
/// live one or any other kind of file is an error. The socket appears at `path` with mode 0600
/// and never with another: it is bound inside a private directory (`.<pid>`, mode 0700,
/// beside `path`), `chmod`ed there and renamed into place, so that nobody can reach it while
/// it has the process's umask mode. (`fchmod` on a socket's descriptor changes the socket
/// object, not the file the path names, and a umask is the whole process's, which threads
/// that create files and directories at the same time would feel.) The owner is the
/// updater's own user.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
    let other = |text: &str| std::io::Error::other(text.to_owned());
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(other("another process listens at the socket path"));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => return Err(other("the socket path exists and is not a socket")),
        // Missing, or unreadable: the bind says which.
        Err(_) => {}
    }
    path.file_name()
        .ok_or_else(|| other("the socket path has no file name"))?;
    let private = path.with_file_name(format!(".{}", std::process::id()));
    let tmp = private.join("s");
    // Leftovers of an updater with this pid that died here.
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir(&private);
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let placed = UnixListener::bind(&tmp).and_then(|listener| {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        Ok(listener)
    });
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir(&private);
    placed
}
