//! A signal seals the result document.
//!
//! `SIGTERM` and `SIGINT` end an applied deploy where it stands (the process exits with
//! 128 plus the signal). The document would stay `RUNNING`, which no reader counts as a pass
//! but which also never says that the run ended; so a handler writes it `red`, finished,
//! with the note `interrupted by SIGTERM` (or `SIGINT`) before the process exits. A document
//! that is already finished is left alone.
//!
//! The handler itself only writes the signal number to a pipe (all it may do); a thread
//! reads the pipe and does the rest. The thread and the handler are installed once per
//! process, and a signal that arrives with no deploy armed gets its default action again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

use kbf_gate::{ResultDoc, Status, system_clock, write_atomic};

/// The signals that end a deploy.
const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
static INSTALL: Once = Once::new();
static ARMED: OnceLock<Mutex<Vec<(u64, PathBuf)>>> = OnceLock::new();

fn armed() -> &'static Mutex<Vec<(u64, PathBuf)>> {
    ARMED.get_or_init(|| Mutex::new(Vec::new()))
}

/// A deploy whose document `path` a signal seals, until dropped.
pub struct Armed(u64);

/// Arm the handlers for the deploy that writes `path`.
pub fn arm(path: &Path) -> Armed {
    INSTALL.call_once(install);
    let mut list = armed()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = list.iter().map(|(id, _)| *id).max().map_or(0, |m| m + 1);
    list.push((id, path.to_owned()));
    Armed(id)
}

impl Drop for Armed {
    fn drop(&mut self) {
        let mut list = armed()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        list.retain(|(id, _)| *id != self.0);
    }
}

/// The calling thread's `errno`.
#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location; the caller upholds the contract.
    unsafe { libc::__errno_location() }
}

/// The calling thread's `errno`.
#[cfg(not(target_os = "linux"))]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno location; the caller upholds the contract.
    unsafe { libc::__error() }
}

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: errno is read and put back around write(2), which is async-signal-safe, so
        // the code the signal interrupted does not see a changed errno; `byte` lives for the
        // call.
        unsafe {
            let saved = *errno();
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
            *errno() = saved;
        }
    }
}

/// Held by whoever finishes a result document: the deploy's own seal and the signal's. Both
/// write through the same temporary name, and a document finished by one must be left alone
/// by the other, so they take turns. The signal's holds it until the process exits.
pub(super) fn exclusive() -> MutexGuard<'static, ()> {
    static SEAL: Mutex<()> = Mutex::new(());
    SEAL.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn install() {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` holds the two descriptors pipe(2) fills in.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    PIPE_WRITE.store(fds[1], Ordering::Relaxed);
    let read_end = fds[0];
    for signal in SIGNALS {
        // SAFETY: a zeroed sigaction with a handler that only calls write(2).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(signal, &raw const action, std::ptr::null_mut());
        }
    }
    std::thread::spawn(move || watch(read_end));
}

fn watch(read_end: libc::c_int) {
    loop {
        let mut byte = 0u8;
        // SAFETY: reads one byte into `byte`.
        let n = unsafe { libc::read(read_end, std::ptr::from_mut(&mut byte).cast(), 1) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return;
        }
        let signal = libc::c_int::from(byte);
        let paths: Vec<PathBuf> = armed()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(_, p)| p.clone())
            .collect();
        if paths.is_empty() {
            // No deploy is running here: the signal does what it always does.
            // SAFETY: restores the default action and raises the signal again.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            continue;
        }
        let _held = exclusive();
        let outcomes: Vec<Outcome> = paths.iter().map(|p| seal(p, signal)).collect();
        // Every document already finished: the deploy ended by itself and is leaving; the
        // signal has nothing to interrupt. Otherwise the process ends here, lock held, so the
        // deploy's own seal cannot write after this one.
        if outcomes.iter().all(|o| *o == Outcome::AlreadyFinished) {
            continue;
        }
        std::process::exit(128 + signal);
    }
}

/// What [`seal`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The document was `RUNNING` and is now red.
    Sealed,
    /// The document was finished by the deploy itself; left alone.
    AlreadyFinished,
    /// No readable document (the deploy had not started it, or it is not one).
    NoDocument,
}

/// [`seal`] under [`exclusive`].
#[cfg(test)]
pub(super) fn seal_exclusive(path: &Path, signal: libc::c_int) -> Outcome {
    let _held = exclusive();
    seal(path, signal)
}

/// Finish the document at `path` as red because `signal` ended the run, if it is still
/// `RUNNING`. A document that is finished, absent or unreadable is left as it is. The caller
/// holds [`exclusive`].
fn seal(path: &Path, signal: libc::c_int) -> Outcome {
    let Ok(bytes) = std::fs::read(path) else {
        return Outcome::NoDocument;
    };
    let Ok(mut doc) = ResultDoc::from_bytes(&bytes) else {
        return Outcome::NoDocument;
    };
    if doc.status != Status::Running {
        return Outcome::AlreadyFinished;
    }
    let name = match signal {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    };
    doc.status = Status::Red;
    doc.finished = Some(system_clock());
    doc.note = Some(format!("interrupted by {name}"));
    if let Ok(out) = doc.to_bytes() {
        let _ = write_atomic(path, &out);
    }
    Outcome::Sealed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves a signal seals a document that is still `RUNNING` as red with the signal named,
    /// and leaves a finished one, an absent one and a garbled one alone. Catches a seal that
    /// overwrites a green verdict with red, or one that gives up on a running document.
    ///
    /// Mutants planted: the status check dropped (a finished document is rewritten).
    #[test]
    fn a_running_document_is_sealed_red_and_a_finished_one_is_left() {
        use kbf_gate::{CommitId, Header, Run, SetHash, Stage};
        let (_, dir) = crate::testkit::host_root("deploy-interrupt");
        let header = || Header {
            stage: Stage::Gamma,
            commit: CommitId::parse(&"a".repeat(40)).unwrap(),
            set_hash: SetHash::of_sums(b"x"),
            run_id: "r".to_owned(),
            attempt: 1,
        };
        let running = dir.join("running.json");
        let run = Run::start(&running, header(), system_clock).unwrap();
        assert_eq!(seal_exclusive(&running, libc::SIGTERM), Outcome::Sealed);
        let doc = ResultDoc::from_bytes(&std::fs::read(&running).unwrap()).unwrap();
        assert_eq!(doc.status, Status::Red);
        assert!(doc.finished.is_some());
        assert_eq!(doc.note.as_deref(), Some("interrupted by SIGTERM"));
        drop(run);

        let done = dir.join("done.json");
        Run::start(&done, header(), system_clock)
            .unwrap()
            .extend("no decision")
            .unwrap();
        let before = std::fs::read(&done).unwrap();
        assert_eq!(
            seal_exclusive(&done, libc::SIGINT),
            Outcome::AlreadyFinished
        );
        assert_eq!(std::fs::read(&done).unwrap(), before);

        assert_eq!(
            seal_exclusive(&dir.join("absent.json"), libc::SIGTERM),
            Outcome::NoDocument
        );
        let garbled = dir.join("garbled.json");
        std::fs::write(&garbled, b"not json").unwrap();
        assert_eq!(seal_exclusive(&garbled, libc::SIGTERM), Outcome::NoDocument);
        assert_eq!(std::fs::read(&garbled).unwrap(), b"not json");
    }
}
