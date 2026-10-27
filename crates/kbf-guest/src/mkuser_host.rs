//! The real [`MkUserHost`](crate::mkuser::MkUserHost), for a macOS guest.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use crate::frame::{Frame, SendError};
use crate::mkuser::{MkUserHost, check_name};
use crate::probe::Host;
use crate::probe_host::SystemHost;
use crate::sim_run::{ProcessRunner, SystemClock, read_regular};

pub struct RealMkUserHost {
    runner: ProcessRunner<SystemClock>,
    host: SystemHost,
}

impl RealMkUserHost {
    #[must_use]
    pub fn new() -> Self {
        // A launch daemon starts with neither HOME nor PATH.
        let env = [
            ("HOME", "/var/root"),
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec();
        Self {
            runner: ProcessRunner::new(SystemClock::new(), env),
            host: SystemHost,
        }
    }
}

impl Default for RealMkUserHost {
    fn default() -> Self {
        Self::new()
    }
}

/// The user name a marker file holds: its first line, when that is a valid name. Anything
/// else (no file, a link, a large file, a bad name) is `None`.
#[must_use]
pub fn recorded_user(path: &str) -> Option<String> {
    let bytes = read_regular(Path::new(path), 256).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let name = text.lines().next()?.trim();
    check_name(name).ok().map(|()| name.to_owned())
}

/// Whether the system has a user called `name`.
fn has_user(name: &str) -> bool {
    user_ids(name).is_some()
}

/// The user id and the primary group id of the user called `name`.
fn user_ids(name: &str) -> Option<(u32, u32)> {
    let c_name = CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `c_name` is a NUL-terminated string; `pwd` and `buf` are valid buffers of the
    // sizes passed, and `found` receives a pointer into them or null. Nothing is read from
    // `pwd` afterwards: only whether a record was found matters.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: `found` is non-null, so `getpwnam_r` filled `pwd`.
    let pwd = unsafe { pwd.assume_init() };
    Some((pwd.pw_uid, pwd.pw_gid))
}

/// Makes `path` (a directory when `file` is false) if missing, and gives it to `user` with
/// `mode`. The path is opened without following a link and the owner and mode are set on
/// the open handle, so a link planted there is refused, not followed.
fn give(path: &str, user: &str, mode: u32, file: bool) -> Result<(), String> {
    let (uid, gid) = user_ids(user).ok_or_else(|| format!("no user {user}"))?;
    let p = Path::new(path);
    if !file
        && std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        std::fs::create_dir(p).map_err(|e| format!("create {path}: {e}"))?;
    }
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
        .map_err(|e| format!("open {path}: {e}"))?;
    let meta = handle.metadata().map_err(|e| format!("stat {path}: {e}"))?;
    if file != meta.is_file() || (!file && !meta.is_dir()) {
        return Err(format!(
            "{path} is not a {}",
            if file { "regular file" } else { "directory" }
        ));
    }
    std::os::unix::fs::fchown(&handle, Some(uid), Some(gid))
        .map_err(|e| format!("chown {path}: {e}"))?;
    handle
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {path}: {e}"))?;
    Ok(())
}

impl MkUserHost for RealMkUserHost {
    fn is_root(&self) -> bool {
        // SAFETY: geteuid takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    fn user_exists(&self, name: &str) -> bool {
        has_user(name)
    }

    fn ready_user(&self) -> Option<String> {
        recorded_user(crate::simpaths::USER_READY)
    }

    fn random_bytes(&self, n: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0u8; n];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|e| format!("read /dev/urandom: {e}"))?;
        Ok(bytes)
    }

    fn write_file(&self, path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
        let path = Path::new(path);
        let dir = path.parent().ok_or("no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        // Made under another name with its final mode, then renamed over the target: the
        // content is never there with a wider mode, and a link at the target is replaced,
        // not followed.
        let part = dir.join(format!(".kbf-mkuser-{}.part", std::process::id()));
        let _ = std::fs::remove_file(&part);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&part)
            .and_then(|mut f| {
                f.write_all(bytes)?;
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
                f.sync_all()
            })
            .and_then(|()| std::fs::rename(&part, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        written.map_err(|e| format!("write {}: {e}", path.display()))
    }

    fn give_to_user(&self, path: &str, user: &str, mode: u32, file: bool) -> Result<(), String> {
        give(path, user, mode, file)
    }

    fn run(&self, argv: &[String], deadline: Duration) -> crate::sim::Outcome {
        self.runner.run(argv, deadline)
    }

    fn stream(
        &self,
        argv: &[String],
        deadline: Duration,
        sink: &mut dyn FnMut(&[u8]),
    ) -> crate::sim::Streamed {
        self.runner.stream(argv, deadline, sink)
    }

    fn send(&self, port: u32, frame: &Frame) -> Result<(), SendError> {
        self.host.send_frame(port, frame)
    }

    fn pause(&self, d: Duration) {
        self.host.pause(d);
    }

    fn ask(&self, port: u32, frame: &Frame) -> Result<String, SendError> {
        self.host.ask_frame(port, frame)
    }

    fn read_file(&self, path: &str, cap: usize) -> Result<Vec<u8>, String> {
        read_regular(Path::new(path), cap)
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}
