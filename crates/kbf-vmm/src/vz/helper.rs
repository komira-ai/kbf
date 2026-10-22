//! The VM helper (`kbf-vmm vm-run`): runs one VM on its own serial dispatch queue and
//! reports on stdout, one JSON [`Event`] per line, until its stdin closes or the guest
//! stops. The main thread only waits on channels; it is never needed by the framework.

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtualMachine, VZVirtualMachineDelegate,
};

use super::config::{self, ConsoleFds};
use super::{Queued, describe};
use crate::spec::{Event, VmSpec};

/// Prints one event line.
fn emit(e: &Event) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", e.to_line());
    let _ = out.flush();
}

struct ObserverIvars {
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
            // SAFETY: `destinationPort` and `close` are plain accessors on a connection the
            // framework keeps alive for this call.
            let port = unsafe { connection.destinationPort() };
            emit(&Event::Vsock { port });
            // SAFETY: as above; the helper does not use the connection afterwards.
            unsafe { connection.close() };
            Bool::YES
        }
    }
);

impl Observer {
    fn new(done: Sender<()>, guest_stopped: Arc<AtomicBool>, began: Instant) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ObserverIvars {
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
            let observer = Observer::new(dtx, gs, began);
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
        Ok(Err((message, code))) => return fail("start", message, code),
        Err(_) => return fail("start", "no start completion in 120 s".to_owned(), None),
    }

    // The parent closing stdin is the request to stop.
    let stdin_done = done_tx;
    std::thread::spawn(move || {
        let mut sink = [0u8; 256];
        let mut stdin = std::io::stdin().lock();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
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
            // SAFETY: on the VM's queue; `stopWithCompletionHandler` is the force stop.
            unsafe { running.get().vm.stopWithCompletionHandler(&block) };
        });
        match stop_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err((message, code))) => return fail("stop", message, code),
            Err(_) => return fail("stop", "no stop completion in 30 s".to_owned(), None),
        }
    }
    emit(&Event::Stopped { ms: ms() });
    Ok(())
}
