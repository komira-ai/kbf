//! [`FakeHost`]: a [`VmHost`] with no hypervisor. A "VM" is the real `kbf-guest` agent
//! serving in a thread of this process, with the shares' host directories as its shares
//! and a Unix socket pair as the virtio socket, so the driver's side of the protocol is
//! the one a real guest sees. A clone is a plain copy of the image directory.
//!
//! It keeps the macOS limit of two running VMs, can delay or withhold its guests'
//! readiness, crash a running VM, silence a guest before its `Ready` or once its
//! command has started, and fail stops; it records every spec it started and every
//! stop it was asked for, and every `Run` the driver sent.
//!
//! Nothing in it reads a clock. Readiness is counted in connects, and a stream's read
//! bound is recorded, not timed: a test says when a bound has run out
//! ([`FakeHost::expire_read_bounds`]).

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_guest::server::{Agent, Config, HELLO_TIMEOUT, Stream};
use kbf_guest::wire::{GuestMsg, HostMsg, RunRequest, TOKEN_LEN, read_frame, write_frame};

use crate::host::{HostError, INPUTS_TAG, OUTPUTS_TAG, VmHost, VmSpec};
use crate::{GUI_SESSION, TOKEN_FILE};

/// How many macOS guests the licence allows on one Mac.
pub const MACOS_VM_LIMIT: usize = 2;

/// A hypervisor-less [`VmHost`]; see the module documentation.
#[derive(Debug)]
pub struct FakeHost {
    limit: usize,
    /// How many connects find a guest still booting before one reaches it; `None` for
    /// never.
    ready_after: Option<usize>,
    session: String,
    /// Where guests fall silent, if anywhere.
    silence: Silence,
    /// How many stops succeed before every later one reports an error.
    stops_ok: usize,
    /// Guests expect a token other than the one the driver wrote.
    wrong_token: bool,
    /// Holds `clone_image` before it copies anything, if set.
    clone_gate: Option<CloneHold>,
    /// How many guest messages [`FakeHost::mute_after_start`] has withheld.
    withheld: Arc<AtomicUsize>,
    state: Mutex<State>,
}

/// The host's half of a [`CloneGate`].
#[derive(Debug)]
struct CloneHold {
    entered: Arc<AtomicBool>,
    release: Mutex<mpsc::Receiver<()>>,
}

/// The test's half of a gated `clone_image`: it says when a clone is held and lets it go.
/// A host whose gate is dropped unreleased lets the clone go on.
#[derive(Debug)]
pub struct CloneGate {
    entered: Arc<AtomicBool>,
    release: mpsc::Sender<()>,
}

impl CloneGate {
    /// Whether a `clone_image` is held at the gate.
    #[must_use]
    pub fn held(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Lets the held clone, or the next, go on.
    pub fn release(&self) {
        let _ = self.release.send(());
    }
}

/// Where a [`FakeHost`]'s guests stop sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// They send everything.
    Never,
    /// A connection is accepted and nothing is ever sent on it, not even `Hello`.
    BeforeReady,
    /// Nothing after `Started`.
    AfterStart,
}

#[derive(Debug, Default)]
struct State {
    running: BTreeMap<String, Booted>,
    specs: Vec<VmSpec>,
    stopped: Vec<String>,
    clones: usize,
    /// The host's end of every stream handed out, with its tap.
    taps: Vec<(Arc<Tap>, UnixStream)>,
}

/// What a [`FakeStream`] and the host share: the read bound the driver set, whether it
/// has run out, and everything the driver wrote.
#[derive(Debug, Default)]
struct Tap {
    bound: Mutex<Option<Duration>>,
    expired: AtomicBool,
    sent: Mutex<Vec<u8>>,
}

impl Tap {
    fn bound(&self) -> MutexGuard<'_, Option<Duration>> {
        self.bound.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sent(&self) -> MutexGuard<'_, Vec<u8>> {
        self.sent.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The driver's end of a connection to a fake guest: a Unix socket whose read bound is
/// only recorded. A bound that [`FakeHost::expire_read_bounds`] has run out ends a
/// blocked read as the OS's timeout does, with `WouldBlock`.
#[derive(Debug)]
pub struct FakeStream {
    inner: UnixStream,
    tap: Arc<Tap>,
}

impl Read for FakeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(0) if self.tap.expired.load(Ordering::SeqCst) => {
                Err(io::ErrorKind::WouldBlock.into())
            }
            read => read,
        }
    }
}

impl Write for FakeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.tap.sent().extend_from_slice(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Stream for FakeStream {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
            tap: Arc::clone(&self.tap),
        })
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.tap.bound() = timeout;
        Ok(())
    }

    fn shutdown(&self) -> io::Result<()> {
        self.inner.shutdown(std::net::Shutdown::Both)
    }
}

#[derive(Debug)]
struct Booted {
    /// How many connects were made to it.
    connects: usize,
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
            ready_after: Some(0),
            session: GUI_SESSION.to_owned(),
            silence: Silence::Never,
            stops_ok: usize::MAX,
            wrong_token: false,
            clone_gate: None,
            withheld: Arc::new(AtomicUsize::new(0)),
            state: Mutex::new(State::default()),
        }
    }

    /// Holds every `clone_image` before it copies, until the gate releases it: a lease
    /// caught in the blocking pool's clone-and-start step, on an event rather than a clock.
    #[must_use]
    pub fn gated_clone(mut self) -> (Self, CloneGate) {
        let (release, wait) = mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        self.clone_gate = Some(CloneHold {
            entered: Arc::clone(&entered),
            release: Mutex::new(wait),
        });
        (self, CloneGate { entered, release })
    }

    /// Guests are still booting for the first `connects` connects to their VM, and can
    /// be reached by the next: a boot that takes as many polls, whatever they last.
    #[must_use]
    pub fn ready_after_connects(mut self, connects: usize) -> Self {
        self.ready_after = Some(connects);
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

    /// Guests send nothing once they have reported `Started`, as a guest that hangs
    /// mid-run: the command runs, its report never arrives.
    #[must_use]
    pub fn mute_after_start(mut self) -> Self {
        self.silence = Silence::AfterStart;
        self
    }

    /// Guests accept a connection and send nothing on it, not even `Hello`, as a guest
    /// agent that hangs before its handshake: the VM is reachable, its `Ready` never
    /// arrives.
    #[must_use]
    pub fn mute_before_ready(mut self) -> Self {
        self.silence = Silence::BeforeReady;
        self
    }

    /// Guests expect a token other than the one the driver wrote, so they refuse the
    /// driver's `Hello` with `Refusal::Token`, as a guest booted with another lease's
    /// share would.
    #[must_use]
    pub fn wrong_token(mut self) -> Self {
        self.wrong_token = true;
        self
    }

    /// The first `ok` stops succeed; every later one stops the VM but reports an
    /// error, as a hypervisor that cannot confirm the stop.
    #[must_use]
    pub fn failing_stops_after(mut self, ok: usize) -> Self {
        self.stops_ok = ok;
        self
    }

    /// How many messages a guest sent after `Started` that [`FakeHost::mute_after_start`]
    /// kept from the driver: the report, once the command ended.
    #[must_use]
    pub fn withheld(&self) -> usize {
        self.withheld.load(Ordering::SeqCst)
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

    /// The read bound each stream to a guest has now (`None` for no bound), in the order
    /// the streams were made, clones of a stream counted once.
    #[must_use]
    pub fn read_bounds(&self) -> Vec<Option<Duration>> {
        (self.lock().taps.iter())
            .map(|(tap, _)| *tap.bound())
            .collect()
    }

    /// Runs out the read bound of every stream that has one: a read blocked on it, now
    /// or later, fails with `WouldBlock` as when the OS's timeout expires. A stream with
    /// no bound is not touched.
    pub fn expire_read_bounds(&self) {
        for (tap, host) in &self.lock().taps {
            if tap.bound().is_some() {
                tap.expired.store(true, Ordering::SeqCst);
                let _ = host.shutdown(std::net::Shutdown::Read);
            }
        }
    }

    /// Every `Run` the driver sent a guest, in order.
    ///
    /// # Panics
    /// The driver sent a frame that is not a message.
    #[must_use]
    pub fn requests(&self) -> Vec<RunRequest> {
        let mut runs = Vec::new();
        for (tap, _) in &self.lock().taps {
            let mut sent = Cursor::new(tap.sent().clone());
            while let Ok(frame) = read_frame(&mut sent) {
                if let HostMsg::Run(run) = HostMsg::decode(&frame).expect("a message") {
                    runs.push(run);
                }
            }
        }
        runs
    }

    /// Crashes the running VM `name`: every stream to its guest ends and its command is
    /// killed, as when the guest dies; the VM keeps its slot until it is stopped, and
    /// a connect to it fails, booted or not.
    ///
    /// # Panics
    /// `name` is not running.
    pub fn crash(&self, name: &str) {
        let mut booted = {
            let mut state = self.lock();
            let booted = state.running.get_mut(name).expect("the VM is running");
            Booted {
                connects: booted.connects,
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
    type Stream = FakeStream;

    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()> {
        if let Some(hold) = &self.clone_gate {
            hold.entered.store(true, Ordering::SeqCst);
            // A dropped gate is a release too.
            let _ = hold
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
        }
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
        let share = |tag| {
            let share = spec.shares.iter().find(|s| s.tag == tag);
            share
                .expect("the driver gives every VM both shares")
                .host
                .clone()
        };
        let (inputs, outputs) = (share(INPUTS_TAG), share(OUTPUTS_TAG));
        // The guest reads its token from the inputs share, as `kbf-guest serve` does.
        let mut token = read_token(&inputs.join(TOKEN_FILE))?;
        if self.wrong_token {
            token[0] ^= 1;
        }
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
                // Fails only setting the stream up; the driver sees its end either way.
                let _ = agent.serve_connection(stream);
                if agent.has_run() {
                    return;
                }
            }
        });
        state.running.insert(
            spec.name.clone(),
            Booted {
                connects: 0,
                conns: Some(tx),
                open: Vec::new(),
                agent: Some(agent),
            },
        );
        Ok(())
    }

    fn connect(&self, name: &str) -> Result<FakeStream, HostError> {
        let mut state = self.lock();
        let state = &mut *state;
        let not_running = || io::Error::new(io::ErrorKind::NotConnected, "the VM is not running");
        let booted = state.running.get_mut(name).ok_or_else(not_running)?;
        // A crashed VM is gone, not still booting.
        let conns = booted.conns.clone().ok_or_else(not_running)?;
        booted.connects += 1;
        match self.ready_after {
            Some(booting) if booted.connects > booting => {}
            _ => return Err(HostError::NotReady),
        }
        let (host, guest) = UnixStream::pair()?;
        booted.open.push(host.try_clone()?);
        let tap = Arc::new(Tap::default());
        let stream = FakeStream {
            inner: host.try_clone()?,
            tap: Arc::clone(&tap),
        };
        state.taps.push((tap, host));
        let guest = match self.silence {
            Silence::Never => guest,
            Silence::BeforeReady => {
                // Held open, never served: the stream ends when the VM stops.
                booted.open.push(guest);
                return Ok(stream);
            }
            Silence::AfterStart => {
                let (relay, agent_end) = UnixStream::pair()?;
                booted.open.push(agent_end.try_clone()?);
                mute_after_started(guest, relay, Arc::clone(&self.withheld))?;
                agent_end
            }
        };
        conns.send(guest).ok().ok_or_else(not_running)?;
        Ok(stream)
    }

    fn stop(&self, name: &str) -> io::Result<()> {
        let (booted, stops) = {
            let mut state = self.lock();
            state.stopped.push(name.to_owned());
            (state.running.remove(name), state.stopped.len())
        };
        if let Some(mut booted) = booted {
            booted.end();
        }
        if stops > self.stops_ok {
            return Err(io::Error::other("the hypervisor did not confirm the stop"));
        }
        Ok(())
    }
}

/// Relays between `host` (the host's side) and `agent` (the agent's): everything the
/// host sends, and what the agent sends up to and including `Started`; nothing after.
fn mute_after_started(
    host: UnixStream,
    agent: UnixStream,
    withheld: Arc<AtomicUsize>,
) -> io::Result<()> {
    let (mut host_in, mut agent_out) = (host.try_clone()?, agent.try_clone()?);
    std::thread::spawn(move || {
        let _ = io::copy(&mut host_in, &mut agent_out);
        let _ = agent_out.shutdown(std::net::Shutdown::Both);
    });
    let (mut agent_in, mut host_out) = (agent, host);
    std::thread::spawn(move || {
        let mut started = false;
        while let Ok(frame) = read_frame(&mut agent_in) {
            if started {
                withheld.fetch_add(1, Ordering::SeqCst);
            } else {
                started = matches!(GuestMsg::decode(&frame), Ok(GuestMsg::Started { .. }));
                let _ = write_frame(&mut host_out, &frame);
            }
        }
    });
    Ok(())
}

/// The token the driver wrote at `path`.
fn read_token(path: &Path) -> io::Result<[u8; TOKEN_LEN]> {
    let text = std::fs::read_to_string(path)?;
    let bytes = hex::decode(text.trim()).expect("the driver writes the token as hex");
    Ok(bytes.try_into().expect("the driver writes TOKEN_LEN bytes"))
}

/// Copies the directory `from` to `to`, which must not exist: directories and regular
/// files only, as an image holds.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a fake stream whose shutdown does not end the guest's side (a reader
    /// blocked on the other end would never return when a VM stops), and one that does
    /// not take a clone's shutdown for its own: every handle shares the one socket.
    #[test]
    fn a_shutdown_of_a_fake_stream_ends_the_other_end() {
        let (driver_end, mut guest_end) = UnixStream::pair().expect("pair");
        let stream = FakeStream {
            inner: driver_end,
            tap: Arc::new(Tap::default()),
        };
        let clone = stream.try_clone().expect("a clone");

        clone.shutdown().expect("shut down");

        let mut read = Vec::new();
        assert_eq!(guest_end.read_to_end(&mut read).expect("an end"), 0);
        assert!(Arc::ptr_eq(&stream.tap, &clone.tap));
    }
}
