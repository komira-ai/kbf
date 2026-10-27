//! The real [`Host`](crate::probe::Host), for a macOS guest.
//!
//! No Virtualization.framework here: the guest only uses what any process may.

use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::frame::{self, Frame, Kind, SendError};
use crate::probe::Host;
use crate::vsock::{AF_VSOCK, HOST_CID, SOCKADDR_VM_LEN, set_io_timeout, sockaddr_vm};

pub struct SystemHost;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// How long a read or write on the connection may block.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A failed step of connecting to the host.
#[derive(Debug)]
struct ConnectError {
    stage: &'static str,
    errno: i32,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed: errno {} ({})",
            self.stage,
            self.errno,
            std::io::Error::from_raw_os_error(self.errno)
        )
    }
}

/// Connects a stream socket to the host on vsock `port`, with a read and write timeout, so
/// a host that never answers cannot hold the probe.
fn connect_host(port: u32) -> Result<File, ConnectError> {
    let fail = |stage| ConnectError {
        stage,
        errno: errno(),
    };
    // SAFETY: plain socket(2) call; a descriptor it returns is owned by the `File` below.
    let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(fail("socket"));
    }
    // SAFETY: `fd` was just returned by socket(2) and nothing else owns it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let file = File::from(owned);
    set_io_timeout(fd, IO_TIMEOUT).map_err(|errno| ConnectError {
        stage: "setsockopt",
        errno,
    })?;
    let addr = sockaddr_vm(HOST_CID, port);
    // SAFETY: `addr` is a `struct sockaddr_vm`-sized buffer and its length is passed.
    if unsafe { libc::connect(fd, addr.as_ptr().cast(), SOCKADDR_VM_LEN as libc::socklen_t) } != 0 {
        return Err(fail("connect"));
    }
    Ok(file)
}

impl SystemHost {
    /// Sends `frame` to the host on `port` and returns the body of its `ack`: the host's
    /// answer to a question.
    ///
    /// # Errors
    /// As [`Host::send_frame`].
    pub fn ask_frame(&self, port: u32, frame: &Frame) -> Result<String, SendError> {
        let mut stream = connect_host(port).map_err(|e| SendError::Unavailable(e.to_string()))?;
        frame::ask(&mut stream, frame)
    }
}

impl Host for SystemHost {
    fn ids(&self) -> (u32, u32) {
        // SAFETY: getuid and geteuid take no arguments and cannot fail.
        unsafe { (libc::getuid(), libc::geteuid()) }
    }

    fn session(&self) -> String {
        crate::session::manager_name()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn process_running(&self, name: &str) -> bool {
        Command::new("/usr/bin/pgrep")
            .args(["-x", name])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn vsock(&self, port: u32) -> Value {
        match connect_host(port) {
            Ok(_) => json!({"socket_errno": null, "connect_errno": null, "connected": true}),
            Err(e) if e.stage == "socket" => json!({"socket_errno": e.errno, "connected": false}),
            Err(e) => json!({"socket_errno": null, "connect_errno": e.errno, "connected": false}),
        }
    }

    fn send_frame(&self, port: u32, frame: &Frame) -> Result<(), SendError> {
        let mut stream = connect_host(port).map_err(|e| SendError::Unavailable(e.to_string()))?;
        frame::exchange(&mut stream, frame)
    }

    fn console(&self, name: &str) -> Value {
        let mut candidates = Vec::new();
        let mut wrote = Vec::new();
        for entry in std::fs::read_dir("/dev").into_iter().flatten().flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !(file.starts_with("cu.") || file.starts_with("tty.")) {
                continue;
            }
            candidates.push(file.clone());
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .open(entry.path());
            if let Ok(mut f) = opened
                && f.write_all(format!("kbf-probe-hello {name} {file}\n").as_bytes())
                    .is_ok()
            {
                wrote.push(file);
            }
        }
        let registry = Command::new("/usr/sbin/ioreg")
            .args(["-l", "-w0"])
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.to_ascii_lowercase().contains("virtio") || l.contains(name))
                    .take(40)
                    .map(|l| l.trim().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        json!({"candidates": candidates, "wrote_to": wrote, "ioreg_virtio_lines": registry})
    }

    fn mount_tag(&self, tag: &str, mountpoint: &Path) -> Value {
        if let Err(e) = std::fs::create_dir_all(mountpoint) {
            return json!({"attempted": false, "mkdir_error": e.to_string()});
        }
        match Command::new("/sbin/mount_virtiofs")
            .arg(tag)
            .arg(mountpoint)
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) => {
                let ok = out.status.success();
                if ok {
                    let _ = Command::new("/sbin/umount").arg(mountpoint).status();
                }
                json!({
                    "attempted": true,
                    "exit_code": out.status.code(),
                    "stderr": String::from_utf8_lossy(&out.stderr).trim(),
                })
            }
            Err(e) => json!({"attempted": false, "spawn_error": e.to_string()}),
        }
    }

    fn automount(&self, dir: &Path) -> Value {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                let entries: Vec<String> = rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                json!({"dir_readable": true, "entries": entries})
            }
            Err(e) => json!({"dir_readable": false, "error": e.to_string()}),
        }
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

impl crate::mount::Mounter for SystemHost {
    fn mount(&self, tag: &str, mountpoint: &Path) -> Result<(), String> {
        std::fs::create_dir_all(mountpoint)
            .map_err(|e| format!("create {}: {e}", mountpoint.display()))?;
        let out = Command::new("/sbin/mount_virtiofs")
            .arg(tag)
            .arg(mountpoint)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run /sbin/mount_virtiofs: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "mount_virtiofs exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

impl crate::diag::DiagHost for SystemHost {
    fn run(&self, program: &str, args: &[&str]) -> Value {
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) => json!({
                "exit_code": out.status.code(),
                "stdout": String::from_utf8_lossy(&out.stdout).trim_end(),
                "stderr": String::from_utf8_lossy(&out.stderr).trim_end(),
            }),
            Err(e) => json!({"spawn_error": e.to_string()}),
        }
    }

    fn ids(&self) -> Value {
        // SAFETY: the four calls take no arguments and cannot fail.
        let (uid, euid, gid, egid) = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        json!({"uid": uid, "euid": euid, "gid": gid, "egid": egid})
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn vsock_round_trip(&self, port: u32) -> Value {
        let mut stream = match connect_host(port) {
            Ok(s) => s,
            Err(e) => {
                return json!({
                    "port": port,
                    "connected": false,
                    "round_trip": false,
                    "stage": e.stage,
                    "errno": e.errno,
                    "error": e.to_string(),
                });
            }
        };
        let test = Frame::new(Kind::Log, "vsock-test", "kbf-guest probe-diag test frame\n");
        let sent = frame::exchange(&mut stream, &test);
        json!({
            "port": port,
            "connected": true,
            "round_trip": sent.is_ok(),
            "error": sent.err().map(|e| e.to_string()),
        })
    }

    fn send_frame(&self, port: u32, frame: &Frame) -> Result<(), SendError> {
        <Self as Host>::send_frame(self, port, frame)
    }

    fn mount_cycle(&self, tag: &str, mountpoint: &Path, inside: &dyn Fn(&Path) -> Value) -> Value {
        use crate::mount::Mounter;
        if let Err(e) = self.mount(tag, mountpoint) {
            return json!({"mounted": false, "error": e});
        }
        let inside = inside(mountpoint);
        let unmounted = std::ffi::CString::new(mountpoint.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())
            .and_then(|path| {
                // SAFETY: `path` is a NUL-terminated string that outlives the call.
                if unsafe { libc::unmount(path.as_ptr(), 0) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error().to_string())
                }
            });
        json!({"mounted": true, "inside": inside, "unmount_error": unmounted.err()})
    }
}
