//! `kbf-updater root` as a host runs it: the start of every service, then the agent's socket.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Config, Parts, RootPart, boot, flags, socket, text};

/// Tells the service manager the root part is ready (`READY=1` on `socket`, the unit's
/// `NOTIFY_SOCKET`: a path, or an abstract name after `@`), so units ordered after a `Type=notify`
/// root part start only once its start pass is over. `false` when there is no socket (not run by
/// a service manager that asks).
///
/// # Errors
/// The datagram could not be sent.
pub fn notify_ready(socket: Option<&OsStr>) -> io::Result<bool> {
    let Some(socket) = socket else {
        return Ok(false);
    };
    let address = match socket.as_bytes().strip_prefix(b"@") {
        Some(name) => abstract_address(name)?,
        None => SocketAddr::from_pathname(socket)?,
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address)?;
    Ok(true)
}

/// The address of the abstract socket `name` (Linux's; systemd names one that way).
#[cfg(target_os = "linux")]
fn abstract_address(name: &[u8]) -> io::Result<SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    SocketAddr::from_abstract_name(name)
}

/// No other system has abstract sockets.
#[cfg(not(target_os = "linux"))]
fn abstract_address(_: &[u8]) -> io::Result<SocketAddr> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "abstract sockets are Linux's",
    ))
}

/// [`notify_ready`], with what fails written to standard error: the root part serves the agent
/// whether or not the service manager heard it. `true` when it told one.
pub fn announce_ready(socket: Option<&OsStr>) -> bool {
    notify_ready(socket).unwrap_or_else(|e| {
        eprintln!("kbf-updater root: telling the service manager it is ready: {e}");
        false
    })
}

/// Stops a running root part.
#[derive(Clone, Debug)]
pub struct Stop {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
}

impl Stop {
    /// Ends the root part: it answers no more requests and returns.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        // Wakes the accept.
        let _ = UnixStream::connect(&self.socket);
    }
}

/// `kbf-updater root` with its flags and `config` ([`Config::of`] the flags, on a host): loads the
/// declarations, serves the registrations, runs the start pass of every service, then binds the
/// agent's socket, calls `ready` and answers the agent until stopped. Exit 1 with the reason when
/// it cannot start.
pub fn run_config<'a>(
    flags: &flags::Flags,
    config: Config,
    parts: Parts,
    ready: impl FnOnce(Stop) + 'a,
) -> (u8, String) {
    run_ready(flags, config, parts, Box::new(ready))
}

/// [`run_config`] with `ready` boxed: one copy of the start, whatever the caller's `ready` is.
fn run_ready(
    flags: &flags::Flags,
    config: Config,
    parts: Parts,
    ready: Box<dyn FnOnce(Stop) + '_>,
) -> (u8, String) {
    match serve(flags, config, parts, ready) {
        Ok(()) => (0, String::new()),
        Err(why) => (1, format!("kbf-updater root: {why}\n")),
    }
}

fn serve(
    flags: &flags::Flags,
    config: Config,
    parts: Parts,
    ready: Box<dyn FnOnce(Stop) + '_>,
) -> Result<(), String> {
    let accounts = Arc::clone(&parts.accounts);
    let agent = accounts.user(&flags.socket_user).ok_or_else(|| {
        format!(
            "--socket-user {:?} is not an account of this host",
            flags.socket_user
        )
    })?;
    let clock = Arc::clone(&parts.clock);
    let state = config.state.clone();
    let root = Arc::new(RootPart::new(config, parts).map_err(text)?);
    // A declaration installed for an update that never began goes back before anything reads
    // the declarations, even one the load refuses.
    for (service, why) in root.recover_installs() {
        eprintln!("kbf-updater root: {service}: the kept declaration was not put back: {why}");
    }
    for refused in root.refused() {
        eprintln!(
            "kbf-updater root: refused {} ({}): {}",
            refused.service,
            refused.file.display(),
            refused.reason
        );
    }
    if let Some(register) = &flags.register {
        let gid = accounts.group(&register.group).ok_or_else(|| {
            format!(
                "--register-group {:?} is not a group of this host",
                register.group
            )
        })?;
        root.serve_registrations(&register.socket, gid)
            .map_err(text)?;
    }
    // After a boot every record is read first, the services refused at the load included: a
    // record the boot cut off is read as killed before any update is resumed. The boot id is
    // written only once every record was read, so a start that could not read one reads it after
    // a boot again; a service whose record was not read is not started in this pass.
    let unread = if boot::boot_changed(&state, &*clock) {
        root.read_after_boot()
    } else {
        Vec::new()
    };
    for (service, why) in &unread {
        eprintln!("kbf-updater root: {service}: the record was not read after the boot: {why}");
    }
    for service in root.services() {
        if unread.iter().any(|(name, _)| *name == service) {
            continue;
        }
        let started = root.start(&service);
        eprintln!(
            "kbf-updater root: {service}: {}",
            serde_json::to_string(&started).unwrap_or_default()
        );
    }
    if unread.is_empty() {
        boot::commit_boot(&state, &*clock).map_err(text)?;
    }
    let listener = socket::bind(&flags.socket, agent.uid, agent.gid).map_err(text)?;
    let stop = Stop {
        flag: Arc::new(AtomicBool::new(false)),
        socket: flags.socket.clone(),
    };
    eprintln!(
        "kbf-updater root: serving the agent at {}",
        flags.socket.display()
    );
    ready(stop.clone());
    let mut connections = Vec::new();
    for stream in listener.incoming() {
        if stop.flag.load(Ordering::Acquire) {
            break;
        }
        let root = Arc::clone(&root);
        connections.extend(stream.ok().map(|stream| {
            std::thread::spawn(move || socket::serve_connection(&stream, &|r| root.answer(r)))
        }));
    }
    // Every request answered before the root part is gone, and its lock with it.
    for connection in connections {
        let _ = connection.join();
    }
    Ok(())
}
