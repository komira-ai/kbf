//! The VM helper (`kbf-vmm vm-run`): runs one VM on its own serial dispatch queue and
//! reports on stdout, one JSON [`Event`] per line, until its stdin closes or the guest
//! stops. The line `shutdown` on stdin asks the guest to shut down: the helper sends the
//! guest's daemon a `shutdown` command over vsock and, when no ack comes, makes the
//! framework's `requestStopWithError` (the guest's power-button request); the helper then
//! waits for the guest to stop. Closing stdin force-stops the VM. The main
//! thread only waits on channels; it is never needed by the framework.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class, msg_send};
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtualMachine, VZVirtualMachineDelegate,
};

use super::config::{self, ConsoleFds};
use super::{Queued, Shared, describe};
use crate::sink::{self, Limiter, MAX_CONNECTIONS, Results};
use crate::spec::{Event, SHUTDOWN_COMMAND, VmSpec};

/// Prints one event line.
fn emit(e: &Event) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", e.to_line());
    let _ = out.flush();
}

/// Where connections are served from: the host's results directory, opened once, and the
/// bound on how many run at once.
struct Sink {
    results: Arc<Results>,
    limiter: Limiter,
}

struct ObserverIvars {
    /// Present when the spec asks for the guest's frames to be written to files.
    sink: Option<Sink>,
    /// Tells the main thread the guest stopped by itself.
    done: Mutex<Sender<()>>,
    guest_stopped: Arc<AtomicBool>,
    began: Instant,
}

define_class!(
    // The delegate of the VM and of its vsock listeners. The framework calls it on the
    // VM's queue.
    #[unsafe(super(NSObject))]
    #[name = "KbfVmmObserver"]
    #[ivars = ObserverIvars]
    struct Observer;

    unsafe impl NSObjectProtocol for Observer {}

    unsafe impl VZVirtualMachineDelegate for Observer {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            let iv = self.ivars();
            emit(&Event::State {
                state: "stopped".to_owned(),
                ms: iv.began.elapsed().as_millis() as u64,
            });
            iv.guest_stopped.store(true, Ordering::SeqCst);
            if let Ok(tx) = iv.done.lock() {
                let _ = tx.send(());
            }
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let iv = self.ivars();
            let (message, code) = describe(error);
            emit(&Event::Error {
                stage: "running".to_owned(),
                message,
                code,
            });
            iv.guest_stopped.store(true, Ordering::SeqCst);
            if let Ok(tx) = iv.done.lock() {
                let _ = tx.send(());
            }
        }
    }

    unsafe impl VZVirtioSocketListenerDelegate for Observer {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> Bool {
            // SAFETY: `destinationPort` is a plain accessor on a connection the framework
            // keeps alive for this call.
            let port = unsafe { connection.destinationPort() };
            emit(&Event::Vsock { port });
            let Some(sink) = &self.ivars().sink else {
                // SAFETY: as above; the helper does not use the connection afterwards.
                unsafe { connection.close() };
                return Bool::YES;
            };
            let Some(permit) = sink.limiter.try_acquire() else {
                eprintln!(
                    "kbf-vmm: vsock port {port}: refused, {MAX_CONNECTIONS} connections are open"
                );
                return Bool::NO;
            };
            // SAFETY: `fileDescriptor` is a plain accessor; the descriptor stays open for as
            // long as the connection object lives, which `keep` below extends to the end of
            // the serving thread.
            let fd = unsafe { connection.fileDescriptor() };
            // SAFETY: retaining and releasing an Objective-C object is atomic, and the
            // thread only drops this reference (it uses the raw descriptor, never a
            // message to the object), so releasing it on that thread is sound.
            let keep = unsafe { Shared::new(connection.retain()) };
            let results = sink.results.clone();
            std::thread::spawn(move || {
                serve_connection(fd, &results);
                drop(keep);
                drop(permit);
            });
            Bool::YES
        }
    }
);

/// Answers the frames on the connection `fd` until it ends, writing them into `results`.
/// A connection whose timeouts cannot be set is dropped unread.
fn serve_connection(fd: RawFd, results: &Results) {
    match sink::serve_socket(fd, results, &mut std::io::stderr()) {
        Ok(served) => eprintln!(
            "kbf-vmm: vsock connection ended ({:?}): {} stored, {} refused",
            served.end, served.stored, served.refused
        ),
        Err(e) => eprintln!("kbf-vmm: vsock {e}"),
    }
}

impl Observer {
    fn new(
        done: Sender<()>,
        guest_stopped: Arc<AtomicBool>,
        began: Instant,
        sink: Option<Sink>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ObserverIvars {
            sink,
            done: Mutex::new(done),
            guest_stopped,
            began,
        });
        // SAFETY: `init` on NSObject's subclass with all ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

/// What lives as long as the VM; only touched on the VM's queue.
struct Running {
    vm: Retained<VZVirtualMachine>,
    _observer: Retained<Observer>,
    _listeners: Vec<Retained<VZVirtioSocketListener>>,
}

/// Reads a console pipe and reports what the guest wrote.
fn spawn_console_reader(c: &ConsoleFds) {
    let (fd, name): (RawFd, String) = (c.from_guest, c.name.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `fd` is a pipe read end the helper owns until it exits; `buf` is valid
            // for `buf.len()` bytes.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(n) = usize::try_from(n) else { break };
            if n == 0 {
                break;
            }
            emit(&Event::Console {
                port: name.clone(),
                text: String::from_utf8_lossy(&buf[..n]).into_owned(),
            });
        }
    });
}

type Holder = Arc<Mutex<Option<Queued<Running>>>>;

fn fail(stage: &str, message: String, code: Option<String>) -> Result<(), String> {
    emit(&Event::Error {
        stage: stage.to_owned(),
        message: message.clone(),
        code,
    });
    Err(message)
}

/// How long the guest's daemon has to answer the `shutdown` command (connect, send, ack).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Asks the guest to shut down: first the `shutdown` command over vsock to the guest's
/// daemon (`guest_command`), because a macOS guest at the login window ignores the
/// framework's stop request; when the command gets no ack, that stop request
/// ([`request_power_button`]). Either way the guest stopping is reported by the delegate,
/// and the command's outcome by an [`Event::ShutdownCommand`].
///
/// UNCERTAIN until a Mac runs it: that `connectToPort:completionHandler:` reaches a daemon
/// listening in the guest on [`kbf_guest::shutdown::PORT`], and that the connection's
/// descriptor behaves as a plain stream socket (as the one the host accepts does).
fn request_guest_shutdown(
    queue: &DispatchRetained<DispatchQueue>,
    holder: &Holder,
    ms: impl Fn() -> u64 + Copy + Send + 'static,
) {
    let (q, h) = (queue.clone(), holder.clone());
    queue.exec_async(move || {
        let device = {
            let Ok(slot) = h.lock() else { return };
            let Some(running) = slot.as_ref() else { return };
            // SAFETY: on the VM's queue, the only place the VM is touched.
            let devices = unsafe { running.get().vm.socketDevices() };
            devices
                .iter()
                .find_map(|d| d.downcast::<VZVirtioSocketDevice>().ok())
        };
        let Some(device) = device else {
            command_failed("the VM has no virtio socket device".to_owned(), &q, &h, ms);
            return;
        };
        let (q2, h2) = (q.clone(), h.clone());
        let block = RcBlock::new(
            move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                // SAFETY: a non-null `conn` or `err` is live for the duration of this call; the
                // connection is retained so that its descriptor stays open for the thread
                // below, which only drops it (see `Shared`).
                let connection = match unsafe { conn.as_ref() } {
                    Some(c) => Ok(unsafe { (c.fileDescriptor(), Shared::new(c.retain())) }),
                    None => Err(unsafe { err.as_ref() }.map_or_else(
                        || "no connection and no error".to_owned(),
                        |e| describe(e).0,
                    )),
                };
                let (q3, h3) = (q2.clone(), h2.clone());
                // Off the VM's queue: the exchange blocks on the guest.
                std::thread::spawn(move || {
                    let result =
                        connection
                            .map_err(|e| format!("connect: {e}"))
                            .and_then(|(fd, keep)| {
                                let sent = send_command(fd);
                                drop(keep);
                                sent
                            });
                    match result {
                        Ok(()) => emit(&Event::ShutdownCommand {
                            acked: true,
                            detail: "acked".to_owned(),
                            ms: ms(),
                        }),
                        Err(detail) => command_failed(detail, &q3, &h3, ms),
                    }
                });
            },
        );
        // SAFETY: on the VM's queue, which `connectToPort` requires.
        unsafe { device.connectToPort_completionHandler(kbf_guest::shutdown::PORT, &block) };
    });
}

/// Sends the command on the connection `fd` under a timeout.
fn send_command(fd: RawFd) -> Result<(), String> {
    kbf_guest::vsock::set_io_timeout(fd, COMMAND_TIMEOUT)
        .map_err(|errno| format!("setsockopt failed: errno {errno}"))?;
    // SAFETY: `fd` is open for as long as the connection object lives, which the caller
    // keeps until this returns; `ManuallyDrop` stops this `File` closing it (the
    // connection object owns it).
    let mut stream = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    crate::guest_command::send_shutdown(&mut *stream)
}

/// Reports that the command got no ack, and falls back to the stop request.
fn command_failed(
    detail: String,
    queue: &DispatchRetained<DispatchQueue>,
    holder: &Holder,
    ms: impl Fn() -> u64 + Send + 'static,
) {
    emit(&Event::ShutdownCommand {
        acked: false,
        detail,
        ms: ms(),
    });
    request_power_button(queue, holder, ms);
}

/// Asks the guest to shut down with the framework's stop request, on the VM's queue, and
/// reports the request (or why it failed). The guest stopping is reported by the delegate.
fn request_power_button(
    queue: &DispatchRetained<DispatchQueue>,
    holder: &Holder,
    ms: impl Fn() -> u64 + Send + 'static,
) {
    let h = holder.clone();
    queue.exec_async(move || {
        let Ok(slot) = h.lock() else { return };
        let Some(running) = slot.as_ref() else { return };
        // SAFETY: on the VM's queue, the only place the VM is touched.
        match unsafe { running.get().vm.requestStopWithError() } {
            Ok(()) => emit(&Event::State {
                state: "shutdown-requested".to_owned(),
                ms: ms(),
            }),
            Err(e) => {
                let (message, code) = describe(&e);
                emit(&Event::Error {
                    stage: "shutdown".to_owned(),
                    message,
                    code,
                });
            }
        }
    });
}

/// Releases the VM, its delegate and its listeners on the VM's queue, where the framework
/// expects every call on them, instead of on whichever thread drops the last reference.
fn release_on_queue(queue: &DispatchQueue, holder: &Holder) {
    let h = holder.clone();
    queue.exec_sync(move || {
        if let Ok(mut slot) = h.lock() {
            drop(slot.take().map(Queued::into_inner));
        }
    });
}

/// Runs the VM `spec` describes.
///
/// # Errors
/// The configuration is invalid or the start failed (also reported as an event).
pub fn run(spec: &VmSpec) -> Result<(), String> {
    let began = Instant::now();
    let ms = move || began.elapsed().as_millis() as u64;
    let built = match config::build(spec) {
        Ok(b) => b,
        Err(e) => return fail("configure", e, None),
    };
    emit(&Event::Configured { ms: ms() });
    for c in &built.consoles {
        spawn_console_reader(c);
    }

    let queue = DispatchQueue::new("com.kbf.vmm.vm", DispatchQueueAttr::SERIAL);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let guest_stopped = Arc::new(AtomicBool::new(false));
    let holder: Holder = Arc::new(Mutex::new(None));
    let (start_tx, start_rx) = mpsc::channel::<Result<(), (String, Option<String>)>>();

    let cfg = Queued::new(built.config);
    let ports = spec.vsock_ports.clone();
    // Opened before the VM starts, without following a link; every file is made through
    // this descriptor.
    let results = match spec.results_dir.as_deref().map(Results::open).transpose() {
        Ok(r) => r.map(Arc::new),
        Err(e) => return fail("results", e, None),
    };
    let q = queue.clone();
    let (h, gs, dtx) = (holder.clone(), guest_stopped.clone(), done_tx.clone());
    queue.exec_async(move || {
        // SAFETY: this closure runs on the VM's serial queue, the only place the VM is
        // touched. The configuration is only read here; the observer and listeners are
        // stored in `Running`, which outlives the VM's use of them.
        unsafe {
            let vm = VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                cfg.get(),
                &q,
            );
            let sink = results.map(|results| Sink {
                results,
                limiter: Limiter::new(MAX_CONNECTIONS),
            });
            let observer = Observer::new(dtx, gs, began, sink);
            vm.setDelegate(Some(ProtocolObject::from_ref(&*observer)));
            let mut listeners = Vec::new();
            for dev in vm.socketDevices().iter() {
                let Ok(dev) = dev.downcast::<VZVirtioSocketDevice>() else {
                    continue;
                };
                for port in &ports {
                    let l = VZVirtioSocketListener::new();
                    l.setDelegate(Some(ProtocolObject::from_ref(&*observer)));
                    dev.setSocketListener_forPort(&l, *port);
                    listeners.push(l);
                }
            }
            let tx = start_tx;
            let block = RcBlock::new(move |err: *mut NSError| {
                // A non-null `err` is a live NSError for the duration of the call.
                let r = match err.as_ref() {
                    None => Ok(()),
                    Some(e) => Err(describe(e)),
                };
                let _ = tx.send(r);
            });
            vm.startWithCompletionHandler(&block);
            if let Ok(mut slot) = h.lock() {
                *slot = Some(Queued::new(Running {
                    vm,
                    _observer: observer,
                    _listeners: listeners,
                }));
            }
        }
    });

    match start_rx.recv_timeout(Duration::from_secs(120)) {
        Ok(Ok(())) => emit(&Event::Started { ms: ms() }),
        Ok(Err((message, code))) => {
            release_on_queue(&queue, &holder);
            return fail("start", message, code);
        }
        Err(_) => {
            release_on_queue(&queue, &holder);
            return fail("start", "no start completion in 120 s".to_owned(), None);
        }
    }

    // The line `shutdown` asks the guest to shut down; the parent closing stdin is the
    // request to force-stop.
    let stdin_done = done_tx;
    let (shutdown_queue, shutdown_holder) = (queue.clone(), holder.clone());
    std::thread::spawn(move || {
        let mut stdin = BufReader::new(std::io::stdin().lock());
        let mut line = String::new();
        loop {
            line.clear();
            if !matches!(stdin.read_line(&mut line), Ok(n) if n > 0) {
                break;
            }
            if line.trim() == SHUTDOWN_COMMAND {
                request_guest_shutdown(&shutdown_queue, &shutdown_holder, ms);
            }
        }
        let _ = stdin_done.send(());
    });
    let _ = done_rx.recv();

    if !guest_stopped.load(Ordering::SeqCst) {
        let (stop_tx, stop_rx) = mpsc::channel::<Result<(), (String, Option<String>)>>();
        let h = holder.clone();
        queue.exec_async(move || {
            let Ok(slot) = h.lock() else { return };
            let Some(running) = slot.as_ref() else { return };
            let block = RcBlock::new(move |err: *mut NSError| {
                // SAFETY: as in the start completion above.
                let r = match unsafe { err.as_ref() } {
                    None => Ok(()),
                    Some(e) => Err(describe(e)),
                };
                let _ = stop_tx.send(r);
            });
            // SAFETY: on the VM's queue. `stopWithCompletionHandler` is a force stop, not a
            // request to the guest to shut down: the guest sees an unclean shutdown.
            unsafe { running.get().vm.stopWithCompletionHandler(&block) };
        });
        match stop_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err((message, code))) => {
                release_on_queue(&queue, &holder);
                return fail("stop", message, code);
            }
            Err(_) => {
                release_on_queue(&queue, &holder);
                return fail("stop", "no stop completion in 30 s".to_owned(), None);
            }
        }
    }
    release_on_queue(&queue, &holder);
    emit(&Event::Stopped { ms: ms() });
    Ok(())
}
