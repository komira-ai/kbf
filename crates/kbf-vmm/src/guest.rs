//! Running a guest that carries `kbf-guest`: the shared folder it finds, the files the
//! guest writes back, and the launch daemons that start the guest side offline.
//!
//! The base image has no user, no agent and no network. The only way into it without a
//! person at a console is a file written while the guest is off (probe P0), so the
//! daemons below are the offline payload; without them the guest runs nothing of ours and
//! every probe that needs a guest answer says `needs-manual`.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::probe::{Ctx, Vm};
use crate::system::OfflineFile;

/// Where the system's own automount shows the share once a user session exists. The
/// probes read it to report whether it is usable without one; they do not depend on it.
pub const AUTOMOUNT_DIR: &str = "/Volumes/My Shared Files/kbf";
/// Where the root mount daemon mounts [`SHARE_TAG`], so the guest side needs no user
/// session.
pub const GUEST_MOUNT: &str = "/private/var/kbf-mnt";
/// The share as the guest daemons use it: the mount's root, which is the host's share
/// directory itself (it holds `kbf-guest` and `results`).
pub const GUEST_SHARE: &str = GUEST_MOUNT;
/// What the daemons wait for before they start: the results directory exists in the
/// share and nowhere else (the mount point itself exists before the mount).
pub const GUEST_RESULTS: &str = "/private/var/kbf-mnt/results";
/// The virtiofs tag the system's automount uses. The system mounts that device itself, at
/// `/Volumes/My Shared Files`; the first E1 run had the mount daemon mount it again and got
/// `Resource busy` 101 times. Nothing of ours mounts it, and `kbf-guest mount-share`
/// refuses it.
pub const AUTOMOUNT_TAG: &str = "com.apple.virtio-fs.automount";
/// The virtiofs tag of the share directory that the mount daemon mounts at
/// [`GUEST_MOUNT`]: a tag the system does not mount on its own.
pub const SHARE_TAG: &str = "kbfshare";
/// Where the offline write puts a copy of `kbf-guest` on the guest's own disk (inside the
/// Data volume, without a leading slash: [`BOOTSTRAP_OFFLINE`]). The mount daemon runs
/// it, and so does every probe daemon, because the copy on the share cannot be reached
/// before the share is mounted (launchd checks a job's program path when it loads the job).
pub const BOOTSTRAP: &str = "/usr/local/libexec/kbf-guest";
const BOOTSTRAP_OFFLINE: &str = "usr/local/libexec/kbf-guest";
/// How many times the mount daemon tries the mount, and the pause between two tries. After
/// the last try it logs the failure and exits non-zero, once (launchd does not restart it).
pub const MOUNT_ATTEMPTS: u32 = 40;
pub const MOUNT_DELAY_MS: u64 = 3000;
/// The most a result file may hold before the host refuses to read it.
pub const MAX_RESULT_BYTES: u64 = 1 << 20;
/// The custom virtiofs tag of the empty share the probe mounts itself, to answer G3.
pub const TAG: &str = "kbfprobe";
/// The vsock port the host listens on and the guest tries.
pub const VSOCK_PORT: u32 = 5000;
/// The console port name the host offers and the guest tries.
pub const CONSOLE_NAME: &str = "kbf.probe";

pub const RESULTS: &str = "results";
pub const GUEST_BIN: &str = "kbf-guest";
/// Results written by the daemon that runs as root (the control arm).
pub const ROOT_RESULT: &str = "guest-probe-root.json";
/// Results written by the daemon that runs as the unprivileged user `nobody`.
pub const USER_RESULT: &str = "guest-probe-user.json";
/// Markers: a daemon ran (`system`), the login window exists (`login`), and, when the
/// image has an auto-login user with the agent installed, the Aqua session (`aqua`).
pub const READY_SYSTEM: &str = "ready-system";
pub const READY_LOGIN: &str = "ready-login";
pub const READY_AQUA: &str = "ready-aqua";
/// The label of the daemon that needs nothing else in the payload.
pub const LOCAL_LABEL: &str = "com.kbf.probe.local";
/// The file launchd writes the output of the `com.kbf.probe.local` daemon to, on the
/// guest's own disk. The daemon is one system tool and depends on nothing else in the
/// payload (no mount, no share, no kbf-guest, no shell), so the file's presence after a
/// boot proves launchd loaded our plists, its modification time says when, and `image
/// harvest` reads it.
pub const LOCAL_MARKER: &str = "/private/var/log/kbf-probe-local";
/// The label of the root daemon that runs `kbf-guest probe-diag`, and the file launchd
/// writes its JSON to (on the guest's own disk, because the share refuses writes).
pub const DIAG_LABEL: &str = "com.kbf.probe.diag";
pub const DIAG_OUTPUT: &str = "/private/var/log/kbf-probe-diag";
/// Where the daemons that run as `nobody` keep their logs: `nobody` cannot create a file
/// in `/var/log`, and the second E1 run left no log of theirs there.
pub const USER_LOG_DIR: &str = "/private/var/tmp";

/// The host side of the share for one VM.
#[derive(Debug, Clone)]
pub struct Share {
    pub dir: PathBuf,
    pub tagged: PathBuf,
}

impl Share {
    #[must_use]
    pub fn results(&self) -> PathBuf {
        self.dir.join(RESULTS)
    }
}

/// Makes fresh share directories for the VM `name`, with the results directory and, when
/// given, the guest binary. A directory a previous run left is removed first (a symbolic
/// link is removed, not followed). The results and tagged directories are world-writable
/// because the guest's unprivileged daemons write into them.
///
/// # Errors
/// A directory or the copy could not be made.
pub fn prepare_share(ctx: &Ctx<'_>, name: &str) -> Result<Share, String> {
    let dir = ctx.work.join(format!("{name}-share"));
    let tagged = ctx.work.join(format!("{name}-tagged"));
    let results = dir.join(RESULTS);
    remove_leftover(&dir);
    remove_leftover(&tagged);
    make_dir(&dir, 0o755)?;
    make_dir(&tagged, 0o777)?;
    make_dir(&results, 0o777)?;
    if let Some(bin) = &ctx.guest_binary {
        copy_exclusive(bin, &dir.join(GUEST_BIN))
            .map_err(|e| format!("copy {} into the share: {e}", bin.display()))?;
    }
    Ok(Share { dir, tagged })
}

/// Makes the new directory `path` with exactly `mode` (not reduced by the umask).
pub(crate) fn make_dir(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::create_dir(path)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)))
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Removes whatever is at `path` without following a symbolic link. A removal that fails
/// is not reported here: the `create_dir` that follows fails on what is left.
pub(crate) fn remove_leftover(path: &Path) {
    let _ = match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
}

/// Copies `src` to a new file `dst` that must not exist (`O_EXCL`), mode 0755.
fn copy_exclusive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(dst)?;
    std::io::copy(&mut from, &mut to)?;
    to.set_permissions(std::fs::Permissions::from_mode(0o755))
}

/// Reads a result file the guest wrote. The share is guest-writable and the host reads
/// it, so a symbolic link, a pipe, a device or a huge file is refused rather than read.
///
/// # Errors
/// The path is not a regular file, is too large, or cannot be read.
pub fn read_result(path: &Path) -> Result<String, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    let mut text = String::new();
    file.take(MAX_RESULT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_RESULT_BYTES {
        return Err(format!("larger than {MAX_RESULT_BYTES} bytes"));
    }
    Ok(text)
}

/// What a guest run found.
#[derive(Debug)]
pub struct GuestRun {
    /// Result files that came back, by file name.
    pub results: BTreeMap<String, Value>,
    /// Result files that did not come back within the time.
    pub missing: Vec<String>,
    /// Vsock connections and console writes the host saw.
    pub traffic: Value,
}

/// Waits for each of `wanted` (file names under the results directory) and reads them.
pub fn collect(ctx: &Ctx<'_>, vm: &mut Vm, share: &Share, wanted: &[&str]) -> GuestRun {
    let mut results = BTreeMap::new();
    let mut missing = Vec::new();
    for name in wanted {
        let path = share.results().join(name);
        if vm
            .wait_file(&path, ctx.timing.guest_timeout, ctx.timing.tick)
            .is_none()
        {
            missing.push((*name).to_owned());
            continue;
        }
        let value = read_result(&path)
            .and_then(|t| {
                serde_json::from_str(&t).map_err(|e| {
                    let head: String = t.chars().take(200).collect();
                    format!("not JSON ({e}): {head}")
                })
            })
            .unwrap_or_else(|e| serde_json::json!({"unreadable": e}));
        results.insert((*name).to_owned(), value);
    }
    // Let a late connection or console write arrive before the traffic is read.
    vm.watch(ctx.timing.tick, ctx.timing.tick, false);
    GuestRun {
        results,
        missing,
        traffic: vm.guest_traffic(),
    }
}

/// The reason text for a probe whose guest wrote nothing.
#[must_use]
pub fn no_agent_reason(have_binary: bool, missing: &[String]) -> String {
    let binary = if have_binary {
        ""
    } else {
        "no --guest-binary was given, so the share holds no kbf-guest; "
    };
    format!(
        "{binary}the guest wrote no {}. Possible causes: the base image has no launch daemons (install them \
         as root with `kbf-vmm image setup --base <dir>`, or build the image with `--offline-setup`); the share \
         did not mount in the guest (see /var/log/com.kbf.probe.mount.log in the guest); the guest could not \
         write into the results directory (see /var/log/com.kbf.probe.*.log); or the guest is slower than the \
         wait. To run by hand, log in once and run \
         `{GUEST_SHARE}/{GUEST_BIN} probe-guest --out {GUEST_SHARE}/{RESULTS}/<file>`",
        missing.join(", ")
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launch daemon plist that runs `args` at load, as `user` when given. With `restart`
/// launchd starts it again every few seconds until it succeeds (the probes waiting for the
/// share); without, it runs once and a failure stays in its log.
#[must_use]
pub fn daemon_plist(
    label: &str,
    user: Option<&str>,
    args: &[String],
    stdout: Option<&str>,
    restart: bool,
) -> String {
    let log_dir = if user.is_some() {
        USER_LOG_DIR
    } else {
        "/var/log"
    };
    let keep_alive = if restart {
        "  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ThrottleInterval</key>\n  <integer>3</integer>\n"
    } else {
        ""
    };
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", escape(a)))
        .collect();
    let user = user
        .map(|u| format!("  <key>UserName</key>\n  <string>{}</string>\n", escape(u)))
        .unwrap_or_default();
    let stdout = stdout
        .map(|p| {
            format!(
                "  <key>StandardOutPath</key>\n  <string>{}</string>\n",
                escape(p)
            )
        })
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n{user}  <key>RunAtLoad</key>\n  <true/>\n{keep_alive}  <key>StandardErrorPath</key>\n  <string>{log_dir}/{label}.log</string>\n{stdout}</dict>\n</plist>\n"
    )
}

/// The command of a probe daemon: the copy of `kbf-guest` on the guest's own disk. launchd
/// checks the program path when it loads a job, and the share is not mounted yet then; a
/// job whose program is on the share can fail to start and never run again.
fn guest_args(sub: &[&str]) -> Vec<String> {
    let mut v = vec![BOOTSTRAP.to_owned()];
    v.extend(sub.iter().map(|s| (*s).to_owned()));
    v
}

/// The files the offline write puts into the guest: the Setup Assistant marker, the mount
/// point directory, the copy of `kbf-guest` the mount daemon runs (when `bootstrap` holds
/// its bytes), and seven launch daemons (the share mounter, a probe as root, a probe as
/// `nobody`, the system marker, the login marker, the share diagnosis, and the local
/// marker that needs none of the others).
#[must_use]
pub fn offline_payload(bootstrap: Option<Vec<u8>>) -> Vec<OfflineFile> {
    let out = |name: &str| format!("{GUEST_SHARE}/{RESULTS}/{name}");
    let wait = GUEST_RESULTS;
    let mut files = vec![
        OfflineFile {
            path: "private/var/db/.AppleSetupDone".to_owned(),
            contents: Vec::new(),
            mode: 0o644,
        },
        // Creates the mount point; the mount hides the file.
        OfflineFile {
            path: "private/var/kbf-mnt/.keep".to_owned(),
            contents: Vec::new(),
            mode: 0o644,
        },
    ];
    files.extend(bootstrap.map(|contents| OfflineFile {
        path: BOOTSTRAP_OFFLINE.to_owned(),
        contents,
        mode: 0o755,
    }));
    // (label, user, arguments, restarted by launchd until it succeeds)
    let daemons: [(&str, Option<&str>, Vec<String>, bool); 7] = [
        (
            "com.kbf.probe.mount",
            None,
            [
                BOOTSTRAP,
                "mount-share",
                "--tag",
                SHARE_TAG,
                "--mountpoint",
                GUEST_MOUNT,
                "--attempts",
                &MOUNT_ATTEMPTS.to_string(),
                "--delay-ms",
                &MOUNT_DELAY_MS.to_string(),
            ]
            .map(str::to_owned)
            .to_vec(),
            false,
        ),
        (
            "com.kbf.probe.root",
            None,
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(ROOT_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--automount-dir",
                AUTOMOUNT_DIR,
                "--mountpoint",
                "/private/tmp/kbfmnt-root",
            ]),
            true,
        ),
        (
            "com.kbf.probe.user",
            Some("nobody"),
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(USER_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--automount-dir",
                AUTOMOUNT_DIR,
                "--mountpoint",
                "/private/tmp/kbfmnt-user",
            ]),
            true,
        ),
        (
            "com.kbf.probe.system",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--out",
                &out(READY_SYSTEM),
            ]),
            true,
        ),
        (
            "com.kbf.probe.login",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--wait-process",
                "loginwindow",
                "--out",
                &out(READY_LOGIN),
            ]),
            true,
        ),
        (
            // From the guest's own disk, so that it reports even when the mount fails.
            DIAG_LABEL,
            None,
            [
                BOOTSTRAP,
                "probe-diag",
                "--share",
                GUEST_MOUNT,
                "--wait-path",
                GUEST_RESULTS,
            ]
            .map(str::to_owned)
            .to_vec(),
            false,
        ),
        (
            LOCAL_LABEL,
            None,
            [
                "/usr/sbin/sysctl",
                "kern.boottime",
                "kern.osversion",
                "kern.osproductversion",
            ]
            .map(str::to_owned)
            .to_vec(),
            false,
        ),
    ];
    for (label, user, args, restart) in daemons {
        files.push(OfflineFile {
            path: format!("Library/LaunchDaemons/{label}.plist"),
            contents: daemon_plist(
                label,
                user,
                &args,
                match label {
                    LOCAL_LABEL => Some(LOCAL_MARKER),
                    DIAG_LABEL => Some(DIAG_OUTPUT),
                    _ => None,
                },
                restart,
            )
            .into_bytes(),
            mode: 0o644,
        });
    }
    files
}

/// [`offline_payload`] with the `kbf-guest` at `binary` as the mount daemon's copy.
///
/// # Errors
/// The file could not be read.
pub fn offline_payload_from(binary: &Path) -> Result<Vec<OfflineFile>, String> {
    let bytes = std::fs::read(binary).map_err(|e| format!("read {}: {e}", binary.display()))?;
    Ok(offline_payload(Some(bytes)))
}

/// The reason `image base --offline-setup` and `image setup` give when no guest binary was
/// named: the mount daemon runs a copy of it from the guest's own disk.
pub const NEEDS_GUEST_BINARY: &str = "needs --guest-binary: the guest's mount daemon runs a copy of kbf-guest from the guest's own disk (the copy on the share cannot be reached before the share is mounted)";

/// True when `dir` holds the guest binary.
#[must_use]
pub fn has_guest_binary(share: &Share) -> bool {
    share.dir.join(GUEST_BIN).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, ctx, holds, tmp, work_with_base};

    /// The text of the plist of daemon `label` in `files`.
    fn plist_of(files: &[OfflineFile], label: &str) -> String {
        let path = format!("Library/LaunchDaemons/{label}.plist");
        let f = files.iter().find(|f| f.path == path).expect("daemon");
        String::from_utf8(f.contents.clone()).expect("utf8")
    }

    /// Catches plist text that is not escaped (a path with `&` would corrupt the file
    /// launchd reads), a missing user key for the unprivileged daemons, one present for
    /// the root control, and a restart policy that does not follow the `restart` flag.
    #[test]
    fn a_daemon_plist_escapes_and_names_its_user() {
        let user = daemon_plist(
            "com.kbf.x",
            Some("nobody"),
            &["/a&b".into(), "<c>".into()],
            None,
            true,
        );
        assert!(
            holds(&[
                user.contains("<string>/a&amp;b</string>"),
                user.contains("&lt;c&gt;")
            ]),
            "{user}"
        );
        assert!(
            user.contains("<key>UserName</key>\n  <string>nobody</string>"),
            "{user}"
        );
        assert!(user.contains(&format!("<string>{USER_LOG_DIR}/com.kbf.x.log</string>")));
        let root = daemon_plist(
            "com.kbf.y",
            None,
            &["/bin/true".into()],
            Some("/o&p"),
            false,
        );
        assert!(!root.contains("UserName"), "{root}");
        assert!(
            root.contains("<key>StandardOutPath</key>\n  <string>/o&amp;p</string>"),
            "{root}"
        );
        assert!(!user.contains("StandardOutPath"), "{user}");
        assert!(holds(&[
            user.contains("<key>KeepAlive</key>"),
            user.contains("<key>ThrottleInterval</key>"),
            !root.contains("KeepAlive"),
            !root.contains("ThrottleInterval"),
            root.contains("<key>RunAtLoad</key>"),
        ]));
    }

    /// Catches a payload that lacks the marker that skips Setup Assistant, the mount point
    /// directory, the copy of `kbf-guest` the mounter runs, a daemon the guest would never
    /// start, a daemon that waits for a path the mounter does not make, or a root control
    /// that runs as nobody (the G3 verdict compares the two users).
    #[test]
    fn the_offline_payload_holds_the_marker_the_mount_point_the_binary_and_seven_daemons() {
        let files = offline_payload(Some(b"bin".to_vec()));
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths[0], "private/var/db/.AppleSetupDone");
        assert_eq!(paths[1], "private/var/kbf-mnt/.keep");
        assert_eq!(paths[2], "usr/local/libexec/kbf-guest");
        assert_eq!(
            (files[2].contents.as_slice(), files[2].mode),
            (&b"bin"[..], 0o755)
        );
        assert_eq!(BOOTSTRAP, format!("/{}", paths[2]));
        assert_eq!(paths.len(), 10);
        assert!(paths[3..].iter().all(|p| holds(&[
            p.starts_with("Library/LaunchDaemons/com.kbf.probe."),
            p.ends_with(".plist")
        ])));
        let root = plist_of(&files, "com.kbf.probe.root");
        let user = plist_of(&files, "com.kbf.probe.user");
        let system = plist_of(&files, "com.kbf.probe.system");
        let login = plist_of(&files, "com.kbf.probe.login");
        assert!(holds(&[
            !root.contains("UserName"),
            root.contains(ROOT_RESULT),
            root.contains(AUTOMOUNT_DIR),
        ]));
        assert!(holds(&[
            user.contains("<string>nobody</string>"),
            user.contains(USER_RESULT)
        ]));
        assert!(system.contains(READY_SYSTEM));
        assert!(holds(&[
            login.contains("loginwindow"),
            login.contains(READY_LOGIN)
        ]));
        assert!(files.iter().all(|f| f.mode == 0o644 || f.path == paths[2]));
        // The daemons wait for, and write into, the paths the mounter provides, but run
        // the copy on the guest's own disk: launchd checks the program path when it loads
        // a job, before the mount exists (the fifth E1 run: the headless guest's system job
        // never started, "access(.../kbf-mnt/kbf-guest, X_OK) failed with errno 2").
        assert_eq!(GUEST_SHARE, GUEST_MOUNT);
        assert!(GUEST_RESULTS.starts_with(GUEST_MOUNT));
        for text in [&root, &user, &system, &login] {
            assert!(text.contains(&format!("<string>{BOOTSTRAP}</string>")));
            assert!(!text.contains(&format!("{GUEST_SHARE}/{GUEST_BIN}")));
            assert!(text.contains(&format!("<string>{GUEST_RESULTS}</string>")));
            assert!(text.contains(&format!("{GUEST_SHARE}/{RESULTS}/")));
            assert!(
                text.contains("<key>KeepAlive</key>"),
                "waiting daemons restart"
            );
        }
        // Without the binary the copy is not written; the daemons still are.
        let bare = offline_payload(None);
        assert_eq!(bare.len(), 9);
        assert!(bare.iter().all(|f| f.path != paths[2]));
    }

    /// Catches the defect the first E1 run showed: the mount daemon mounting the automount
    /// tag (the system has it mounted: `Resource busy`, 101 times) instead of the share's
    /// own tag, running the copy on the share it cannot reach yet, restarting forever
    /// instead of failing once after a bound, or mounting somewhere the daemons do not read.
    #[test]
    fn the_mount_daemon_mounts_its_own_tag_from_the_guest_disk_and_stops_after_a_bound() {
        let plist = plist_of(&offline_payload(Some(Vec::new())), "com.kbf.probe.mount");
        let arg = |a: &str| format!("<string>{a}</string>");
        let ordered = [
            BOOTSTRAP,
            "mount-share",
            "--tag",
            SHARE_TAG,
            "--mountpoint",
            GUEST_MOUNT,
            "--attempts",
            &MOUNT_ATTEMPTS.to_string(),
            "--delay-ms",
            &MOUNT_DELAY_MS.to_string(),
        ]
        .map(arg)
        .concat();
        assert!(
            plist.contains(&ordered.replace("</string><string>", "</string>\n    <string>")),
            "{plist}"
        );
        assert!(holds(&[
            !plist.contains(AUTOMOUNT_TAG),
            !plist.contains(&format!("{GUEST_SHARE}/{GUEST_BIN}")),
            !plist.contains("/sbin/mount_virtiofs"),
            !plist.contains("UserName"),
            !plist.contains("KeepAlive"),
            plist.contains("<key>RunAtLoad</key>"),
        ]));
        assert_ne!(SHARE_TAG, AUTOMOUNT_TAG);
        assert_ne!(SHARE_TAG, TAG);
        assert!((1..=1000).contains(&MOUNT_ATTEMPTS));
        // The bound is finite and the whole wait is minutes, not hours.
        assert!(u64::from(MOUNT_ATTEMPTS) * MOUNT_DELAY_MS <= 600_000);
    }

    /// Catches a local-marker daemon that depends on the share or a shell (it must run
    /// when the mount never happens), runs as a user that cannot write the log directory,
    /// writes somewhere the harvest allowlist does not read, and a log name the harvest's
    /// pattern would miss.
    #[test]
    fn the_local_daemon_needs_nothing_and_writes_where_harvest_reads() {
        let files = offline_payload(Some(Vec::new()));
        let plist = plist_of(&files, LOCAL_LABEL);
        assert!(
            holds(&[
                plist.contains("<string>com.kbf.probe.local</string>"),
                plist.contains("<string>/usr/sbin/sysctl</string>"),
                plist.contains("<string>kern.boottime</string>"),
                // The label, the tool and its three keys, and the two log paths: no shell.
                plist.matches("<string>").count() == 7,
                !plist.contains("UserName"),
                !plist.contains(GUEST_SHARE),
                !plist.contains(GUEST_BIN),
                plist.contains(&format!(
                    "<key>StandardOutPath</key>\n  <string>{LOCAL_MARKER}</string>"
                )),
                plist.contains("<key>RunAtLoad</key>"),
            ]),
            "{plist}"
        );
        // Only the local and the diagnosis daemons have their output redirected.
        for f in files.iter().filter(|f| f.path.ends_with(".plist")) {
            let text = String::from_utf8(f.contents.clone()).expect("utf8");
            assert_eq!(
                text.contains("StandardOutPath"),
                f.path.contains(LOCAL_LABEL) || f.path.contains(DIAG_LABEL),
                "{}",
                f.path
            );
        }
        assert_eq!(
            crate::harvest::ALLOWLIST[0],
            crate::harvest::Item::File(LOCAL_MARKER.trim_start_matches('/'))
        );
        // Every daemon log the others write is read by the harvest's name pattern.
        for f in files.iter().filter(|f| f.path.ends_with(".plist")) {
            let name = f.path.trim_start_matches("Library/LaunchDaemons/");
            let label = name.trim_end_matches(".plist");
            assert!(label.starts_with("com.kbf.probe."), "{label}");
        }
    }

    /// Catches a diagnosis daemon that needs the share (it must report when the mount
    /// fails), runs as a user that the share may refuse, would loop, or sends its JSON
    /// somewhere the harvest does not read, and logs of the `nobody` daemons in a
    /// directory `nobody` cannot write (the second E1 run left none).
    #[test]
    fn the_diagnosis_runs_from_the_guest_disk_and_the_user_daemons_log_where_they_can() {
        let files = offline_payload(Some(Vec::new()));
        let diag = plist_of(&files, DIAG_LABEL);
        assert!(
            holds(&[
                diag.contains(&format!("<string>{BOOTSTRAP}</string>")),
                diag.contains("<string>probe-diag</string>"),
                diag.contains(&format!("<string>{GUEST_RESULTS}</string>")),
                !diag.contains(&format!("{GUEST_SHARE}/{GUEST_BIN}")),
                !diag.contains("UserName"),
                !diag.contains("KeepAlive"),
                diag.contains(&format!(
                    "<key>StandardOutPath</key>\n  <string>{DIAG_OUTPUT}</string>"
                )),
            ]),
            "{diag}"
        );
        assert_eq!(
            crate::harvest::ALLOWLIST[6],
            crate::harvest::Item::File(DIAG_OUTPUT.trim_start_matches('/'))
        );
        for label in ["user", "system", "login"] {
            let p = plist_of(&files, &format!("com.kbf.probe.{label}"));
            assert!(
                p.contains(&format!(
                    "<string>{USER_LOG_DIR}/com.kbf.probe.{label}.log</string>"
                )),
                "{p}"
            );
        }
        assert!(
            plist_of(&files, "com.kbf.probe.root")
                .contains("<string>/var/log/com.kbf.probe.root.log</string>")
        );
        assert_eq!(
            crate::harvest::ALLOWLIST[7],
            crate::harvest::Item::Matching {
                dir: USER_LOG_DIR.trim_start_matches('/'),
                prefix: "com.kbf.probe.",
                suffix: ".log"
            }
        );
    }

    /// Catches a binary read from the wrong place or a missing file that is not reported.
    #[test]
    fn the_payload_carries_the_bytes_of_the_binary_given() {
        let dir = tmp("guest-payload-from");
        let bin = dir.join("kbf-guest");
        std::fs::write(&bin, "the-bytes").expect("bin");
        let files = offline_payload_from(&bin).expect("payload");
        assert_eq!(files[2].contents, b"the-bytes");
        let e = offline_payload_from(&dir.join("none")).unwrap_err();
        assert!(e.contains("none"), "{e}");
        assert!(NEEDS_GUEST_BINARY.contains("--guest-binary"));
    }

    /// Catches a share that is not made, a guest binary that is not copied into it, a
    /// results directory the guest's unprivileged daemons cannot write, and a leftover
    /// directory (or a symbolic link a guest left) that is reused instead of replaced.
    #[test]
    fn the_share_gets_a_results_directory_and_the_guest_binary() {
        let work = work_with_base("guest-share");
        let sys = FakeSystem::new();
        let mut c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        assert!(holds(&[share.results().is_dir(), share.tagged.is_dir()]));
        assert!(!has_guest_binary(&share));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&share.results()), 0o777);
        assert_eq!(mode(&share.tagged), 0o777);
        let bin = work.join("kbf-guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        c.guest_binary = Some(bin);
        let share = prepare_share(&c, "vm2").expect("share");
        assert!(has_guest_binary(&share));
        assert_eq!(mode(&share.dir.join(GUEST_BIN)), 0o755);
        c.guest_binary = Some(work.join("missing"));
        assert!(prepare_share(&c, "vm3").unwrap_err().contains("copy"));
        // A work directory that is a file cannot hold a share.
        c.work = work.join("base").join("disk.img");
        assert!(prepare_share(&c, "vm4").unwrap_err().contains("create"));
    }

    /// Catches a share directory reused with what the last guest left in it, and one
    /// that is a symbolic link being followed (the host would then write through it).
    #[test]
    fn a_leftover_share_is_replaced_and_a_symbolic_link_is_not_followed() {
        let work = work_with_base("guest-share-leftover");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        std::fs::write(share.results().join("old.json"), "x").expect("old");
        let again = prepare_share(&c, "vm").expect("again");
        assert!(!again.results().join("old.json").exists());
        // A guest swapped the share directory for a link to a directory outside.
        let outside = work.join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("keep"), "k").expect("keep");
        std::fs::remove_dir_all(&again.dir).expect("rm");
        std::os::unix::fs::symlink(&outside, &again.dir).expect("link");
        prepare_share(&c, "vm").expect("replaced");
        assert!(outside.join("keep").exists(), "the target was left alone");
        // A file where the directory goes is removed too.
        std::fs::remove_dir_all(&again.tagged).expect("rm tagged");
        std::fs::write(&again.tagged, "f").expect("file");
        assert!(prepare_share(&c, "vm").expect("share").tagged.is_dir());
    }

    /// Catches a result reader that follows a symbolic link, opens a pipe, reads a
    /// directory, or reads without a limit.
    #[test]
    fn a_result_is_read_only_from_a_small_regular_file() {
        let dir = tmp("guest-read-result");
        let f = dir.join("r");
        std::fs::write(&f, "ok").expect("f");
        assert_eq!(read_result(&f).expect("read"), "ok");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&f, &link).expect("link");
        assert!(read_result(&link).is_err(), "a link is not followed");
        assert!(read_result(&dir.join("missing")).is_err());
        assert!(read_result(&dir).unwrap_err().contains("regular"));
        let big = dir.join("big");
        std::fs::write(
            &big,
            vec![b'a'; usize::try_from(MAX_RESULT_BYTES).expect("n") + 1],
        )
        .expect("big");
        assert!(read_result(&big).unwrap_err().contains("larger"));
        let limit = dir.join("limit");
        std::fs::write(
            &limit,
            vec![b'a'; usize::try_from(MAX_RESULT_BYTES).expect("n")],
        )
        .expect("limit");
        assert!(read_result(&limit).is_ok());
        std::fs::write(dir.join("bin"), [0xff, 0xfe]).expect("bin");
        assert!(read_result(&dir.join("bin")).is_err());
    }

    /// Catches a collector that reports a file it never saw, parses nothing, or lets a
    /// file that is not JSON (or not a file) stop the run.
    #[test]
    fn collect_reads_the_files_that_came_back() {
        let work = work_with_base("guest-collect");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        std::fs::write(share.results().join("a.json"), r#"{"k":1}"#).expect("a");
        std::fs::write(share.results().join("b.json"), "not json").expect("b");
        std::fs::create_dir(share.results().join("c.json")).expect("c");
        let mut vm = crate::probe::Vm::spawn(
            &sys,
            "vm",
            &crate::spec::VmSpec::minimal(work.join("base")),
            None,
        )
        .expect("vm");
        let run = collect(
            &c,
            &mut vm,
            &share,
            &["a.json", "b.json", "c.json", "d.json"],
        );
        assert_eq!(run.results["a.json"]["k"], 1);
        assert!(
            run.results["b.json"]["unreadable"]
                .as_str()
                .expect("text")
                .contains("not JSON")
        );
        assert!(run.results["c.json"]["unreadable"].is_string());
        assert_eq!(run.missing, vec!["d.json".to_owned()]);
        assert_eq!(run.traffic["vsock_connections"], serde_json::json!([]));
    }

    /// Catches a reason that does not tell the operator what to do, or that blames the
    /// image when the binary was simply not given.
    #[test]
    fn the_no_agent_reason_names_the_fix() {
        let with = no_agent_reason(true, &["x.json".into()]);
        assert!(
            holds(&[
                with.contains("x.json"),
                with.contains("image setup --base"),
                with.contains("mount"),
                !with.contains("--guest-binary")
            ]),
            "{with}"
        );
        let without = no_agent_reason(false, &["x.json".into(), "y.json".into()]);
        assert!(
            holds(&[
                without.contains("--guest-binary"),
                without.contains("x.json, y.json")
            ]),
            "{without}"
        );
    }
}
