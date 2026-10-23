//! The real [`Host`](crate::probe::Host), for a macOS guest.
//!
//! No Virtualization.framework here: the guest only uses what any process may.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::probe::Host;

pub struct SystemHost;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
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
        // macOS has no public AF_VSOCK; 40 is the Linux value, tried to see what the
        // kernel answers. The socket call is the question; a connect is tried only if it
        // succeeds, with a BSD-layout address (length byte, family byte) for the host
        // (CID 2).
        const AF_VSOCK: libc::c_int = 40;
        // SAFETY: plain socket(2) call; the descriptor is closed below.
        let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return json!({"socket_errno": errno(), "connected": false});
        }
        let mut addr = [0u8; 16];
        addr[0] = 16;
        addr[1] = AF_VSOCK as u8;
        addr[4..8].copy_from_slice(&port.to_le_bytes());
        addr[8..12].copy_from_slice(&2u32.to_le_bytes());
        // SAFETY: `addr` is a 16-byte buffer and its length is passed.
        let rc = unsafe { libc::connect(fd, addr.as_ptr().cast(), 16) };
        let connect_errno = (rc != 0).then(errno);
        // SAFETY: `fd` is the descriptor opened above.
        unsafe { libc::close(fd) };
        json!({"socket_errno": null, "connect_errno": connect_errno, "connected": rc == 0})
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
