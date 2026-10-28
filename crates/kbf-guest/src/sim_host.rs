//! The real [`SimHost`](crate::sim::SimHost), for a macOS guest.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use crate::frame::{Frame, SendError};
use crate::mkuser_host::recorded_user;
use crate::probe::Host;
use crate::probe_host::SystemHost;
use crate::sim::{ConsoleUser, OUT_DIR, Outcome, SCREENSHOT_PATH, SimHost, Wanted};
use crate::sim_run::{ProcessRunner, SystemClock, read_regular, read_wanted};
use crate::sim_xctest;
use crate::simpaths;

pub struct RealSimHost {
    runner: ProcessRunner<SystemClock>,
    host: SystemHost,
}

impl RealSimHost {
    #[must_use]
    pub fn new() -> Self {
        // A launch daemon starts with neither; `xcodebuild` and `simctl` want both.
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

impl Default for RealSimHost {
    fn default() -> Self {
        Self::new()
    }
}

fn names_in(dir: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// The login name of `uid`.
fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `pwd` and `buf` are valid buffers of the sizes passed; on success `found`
    // points into them and `pw_name` is a NUL-terminated string inside `buf`.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` succeeded and set `found`, so `pwd` is initialised and its
    // `pw_name` points at a NUL-terminated string in `buf`, which is still alive.
    let name = unsafe { std::ffi::CStr::from_ptr((*found).pw_name) };
    name.to_str().ok().map(str::to_owned)
}

impl SimHost for RealSimHost {
    fn is_root(&self) -> bool {
        // SAFETY: geteuid takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    fn runtime_images(&self) -> Vec<String> {
        names_in(simpaths::RUNTIME_DIR)
    }

    fn app_bundles(&self) -> Vec<String> {
        names_in(simpaths::APP_DIR)
    }

    fn xctest_entries(&self) -> Vec<String> {
        names_in(simpaths::TESTS_DIR)
    }

    fn clear_result_bundle(&self) -> Result<(), String> {
        sim_xctest::remove_no_follow(Path::new(sim_xctest::RESULT_PATH))
    }

    fn result_bundle_present(&self) -> bool {
        sim_xctest::is_real_dir(Path::new(sim_xctest::RESULT_PATH))
    }

    fn console_user(&self) -> Option<ConsoleUser> {
        // The owner of /dev/console is the user logged in at the console; root when nobody is.
        let uid = std::fs::metadata("/dev/console").ok()?.uid();
        if uid == 0 {
            return None;
        }
        Some(ConsoleUser {
            uid,
            name: user_name(uid)?,
        })
    }

    fn ready_user(&self) -> Option<String> {
        recorded_user(simpaths::USER_READY)
    }

    fn wanted_user(&self) -> Wanted {
        read_wanted(Path::new(simpaths::USER_WANTED))
    }

    fn prepare_out_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(OUT_DIR)
            .and_then(|()| {
                std::fs::set_permissions(OUT_DIR, std::fs::Permissions::from_mode(0o1777))
            })
            .map_err(|e| e.to_string())
    }

    fn read_screenshot(&self, cap: usize) -> Result<Vec<u8>, String> {
        read_regular(Path::new(SCREENSHOT_PATH), cap)
    }

    fn run(&self, argv: &[String], deadline: Duration) -> Outcome {
        self.runner.run(argv, deadline)
    }

    fn send(&self, port: u32, frame: &Frame) -> Result<(), SendError> {
        self.host.send_frame(port, frame)
    }

    fn pause(&self, d: Duration) {
        self.host.pause(d);
    }

    fn read_file(&self, path: &str, cap: usize) -> Result<Vec<u8>, String> {
        read_regular(Path::new(path), cap)
    }
}
