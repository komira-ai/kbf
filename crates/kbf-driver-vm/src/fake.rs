//! [`FakeHost`]: a [`VmHost`] with no hypervisor. A "VM" is the real `kbf-guest` agent
//! serving in a thread of this process, with the shares' host directories as its shares
//! and a Unix socket pair as the virtio socket, so the driver's side of the protocol is
//! the one a real guest sees. A clone is a plain copy of the image directory.
//!
//! It keeps the macOS limit of two running VMs (configurable), can delay or withhold
//! its guests' readiness, can crash a running VM, and records every spec it started
//! and every stop it was asked for.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kbf_guest::server::{Agent, Config, HELLO_TIMEOUT};
use kbf_guest::wire::TOKEN_LEN;

use crate::host::{HostError, INPUTS_TAG, OUTPUTS_TAG, VmHost, VmSpec};
use crate::{GUI_SESSION, TOKEN_FILE};

/// How many macOS guests the licence allows on one Mac.
pub const MACOS_VM_LIMIT: usize = 2;

/// A hypervisor-less [`VmHost`]; see the module documentation.
#[derive(Debug)]
pub struct FakeHost {
    limit: usize,
    /// How long after `start` a guest can be reached; `None` for never.
    ready_after: Option<Duration>,
    session: String,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    running: BTreeMap<String, Booted>,
    specs: Vec<VmSpec>,
    stopped: Vec<String>,
    clones: usize,
}

#[derive(Debug)]
struct Booted {
    started: Instant,
    /// Hands new connections to the agent's thread; `None` once the VM crashed.
    conns: Option<mpsc::Sender<UnixStream>>,
    /// The host's end of every connection made, to end them on a stop or a crash.
    open: Vec<UnixStream>,
    agent: Option<JoinHandle<()>>,
}

impl Booted {
    /// Ends every connection and waits for the agent, which kills its command's group
    /// when its connection ends.
    fn end(&mut self) {
        for stream in self.open.drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.conns = None;
        if let Some(agent) = self.agent.take() {
            // A panic in the agent's thread has been printed already.
            let _ = agent.join();
        }
    }
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    /// Two VMs at most, each reachable at once, in the `Aqua` session.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: MACOS_VM_LIMIT,
            ready_after: Some(Duration::ZERO),
            session: GUI_SESSION.to_owned(),
            state: Mutex::new(State::default()),
        }
    }

    /// Guests can be reached only `after` their VM started.
    #[must_use]
    pub fn ready_after(mut self, after: Duration) -> Self {
        self.ready_after = Some(after);
        self
    }

    /// Guests can never be reached: every boot times out.
    #[must_use]
    pub fn never_ready(mut self) -> Self {
        self.ready_after = None;
        self
    }

    /// Guests report `session` in `Ready`.
    #[must_use]
    pub fn session(mut self, session: &str) -> Self {
        session.clone_into(&mut self.session);
        self
    }

    /// Every spec started, in order, refused ones included.
    #[must_use]
    pub fn specs(&self) -> Vec<VmSpec> {
        self.lock().specs.clone()
    }

    /// The VMs running now, crashed ones included until they are stopped.
    #[must_use]
    pub fn running(&self) -> Vec<String> {
        self.lock().running.keys().cloned().collect()
    }

    /// Every VM `stop` was called for, in order.
    #[must_use]
    pub fn stopped(&self) -> Vec<String> {
        self.lock().stopped.clone()
    }

    /// How many clones were made.
    #[must_use]
    pub fn clones_made(&self) -> usize {
        self.lock().clones
    }

    /// Crashes the running VM `name`: every stream to its guest ends and its command is
    /// killed, as when the guest dies; the VM keeps its slot until it is stopped.
    ///
    /// # Panics
    /// `name` is not running.
    pub fn crash(&self, name: &str) {
        let mut booted = {
            let mut state = self.lock();
            let booted = state.running.get_mut(name).expect("the VM is running");
            Booted {
                started: booted.started,
                conns: booted.conns.take(),
                open: std::mem::take(&mut booted.open),
                agent: booted.agent.take(),
            }
        };
        booted.end();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update is one insert, push or removal: consistent under a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl VmHost for FakeHost {
    type Stream = UnixStream;

    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()> {
        copy_dir(image, clone)?;
        self.lock().clones += 1;
        Ok(())
    }

    fn start(&self, spec: &VmSpec) -> Result<(), HostError> {
        let mut state = self.lock();
        state.specs.push(spec.clone());
        if state.running.len() >= self.limit {
            return Err(HostError::LimitExceeded(self.limit));
        }
        if state.running.contains_key(&spec.name) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the VM is running").into());
        }
        let share = |tag| {
            spec.shares
                .iter()
                .find(|s| s.tag == tag)
                .map(|s| s.host.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a share is missing"))
        };
        let (inputs, outputs) = (share(INPUTS_TAG)?, share(OUTPUTS_TAG)?);
        // The guest reads its token from the inputs share, as `kbf-guest serve` does.
        let token = read_token(&inputs.join(TOKEN_FILE))?;
        let config = Config {
            token,
            inputs,
            outputs,
            session: self.session.clone(),
            hello_timeout: HELLO_TIMEOUT,
        };
        let (tx, rx) = mpsc::channel::<UnixStream>();
        let agent = std::thread::spawn(move || {
            let mut agent = Agent::new(config);
            // As `Agent::serve`: one connection at a time, until the one that ran.
            for stream in rx {
                if let Err(e) = agent.serve_connection(stream) {
                    eprintln!("fake guest: connection ended: {e}");
                }
                if agent.has_run() {
                    return;
                }
            }
        });
        state.running.insert(
            spec.name.clone(),
            Booted {
                started: Instant::now(),
                conns: Some(tx),
                open: Vec::new(),
                agent: Some(agent),
            },
        );
        Ok(())
    }

    fn connect(&self, name: &str) -> Result<UnixStream, HostError> {
        let mut state = self.lock();
        let not_running = || io::Error::new(io::ErrorKind::NotConnected, "the VM is not running");
        let booted = state.running.get_mut(name).ok_or_else(not_running)?;
        match self.ready_after {
            Some(after) if booted.started.elapsed() >= after => {}
            _ => return Err(HostError::NotReady),
        }
        let conns = booted.conns.as_ref().ok_or_else(not_running)?;
        let (host, guest) = UnixStream::pair()?;
        conns.send(guest).map_err(|_| not_running())?;
        booted.open.push(host.try_clone()?);
        Ok(host)
    }

    fn stop(&self, name: &str) -> io::Result<()> {
        let booted = {
            let mut state = self.lock();
            state.stopped.push(name.to_owned());
            state.running.remove(name)
        };
        if let Some(mut booted) = booted {
            booted.end();
        }
        Ok(())
    }
}

fn read_token(path: &Path) -> io::Result<[u8; TOKEN_LEN]> {
    let text = std::fs::read_to_string(path)?;
    hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a token"))
}

/// Copies the directory `from` to `to`, which must not exist: directories and regular
/// files only, as an image holds.
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
