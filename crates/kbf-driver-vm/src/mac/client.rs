//! The real [`VmSession`]: the root helper's socket and the kernel. macOS only; every
//! rule that decides anything is in the host over the trait, tested against a fake.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use kbf_driver_native::{Record, boot_id, procs};
use kbf_mac_session::client::{Client, Running};

use super::session::{RunExit, RunFds, VmSession};

/// `CLONE_NOOWNCOPY` (`sys/clonefile.h`): the clone belongs to the caller, not to the
/// source's owner.
const CLONE_NOOWNCOPY: u32 = 0x0002;

/// The helper's socket at a path.
#[derive(Clone, Debug)]
pub struct ClientSession {
    client: Client,
}

impl ClientSession {
    /// A session over the helper's socket at `socket`.
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            client: Client::new(socket),
        }
    }
}

fn other(why: impl std::fmt::Display) -> io::Error {
    io::Error::other(why.to_string())
}

fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds NUL"))
}

impl VmSession for ClientSession {
    type Running = Running;
    type Stream = UnixStream;

    /// `clonefile(2)` of each file of the flat image directory, owned by the caller, then
    /// the immutable flag a sealed image's files carry cleared.
    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()> {
        std::fs::create_dir(clone)?;
        for entry in std::fs::read_dir(image)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a regular file", entry.path().display()),
                ));
            }
            let target = clone.join(entry.file_name());
            let (from, to) = (c_path(&entry.path())?, c_path(&target)?);
            // SAFETY: both are NUL-terminated paths that outlive the call.
            if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOOWNCOPY) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above; flags 0 clears the user flags of the caller's own file.
            if unsafe { libc::chflags(to.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn session_ready(&self) -> io::Result<bool> {
        self.client.role_session_ready().map_err(other)
    }

    fn run(&self, argv: &[String], fds: RunFds<'_>) -> io::Result<Running> {
        self.client
            .role_run(argv, &[], [fds.stdin, fds.stdout, fds.stderr, fds.cwd])
            .map_err(other)
    }

    fn pid(&self, running: &Running) -> u32 {
        running.pid
    }

    fn wait(&self, running: Running) -> io::Result<RunExit> {
        let exit = running.wait().map_err(other)?;
        Ok(RunExit {
            code: exit.code,
            signal: exit.signal,
        })
    }

    fn reap(&self, record: &Record) -> io::Result<()> {
        self.client
            .role_reap(record.pgid, record.start, &record.boot)
            .map_err(other)
    }

    fn boot_id(&self) -> io::Result<String> {
        boot_id()
    }

    fn process_start(&self, pid: i32) -> io::Result<Option<u64>> {
        Ok(procs::process(pid).map(|process| process.start))
    }

    /// Not yet: `kbf-vmm vm-run` serves no relay to the guest agent (see the module
    /// documentation of [`crate::mac`]).
    fn connect(&self, _name: &str) -> io::Result<UnixStream> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kbf-vmm vm-run has no relay to the guest agent yet",
        ))
    }
}
