//! The assertion that a pool user holds nothing a lease left, for real: the baseline is
//! recorded when the slots start, a clean user passes every probe, a state planted as
//! the user (a process, a launchd job, a System V semaphore, an attached disk image, a
//! file in the home, a file in `/private/tmp`, a cron job) turns the probe that looks
//! for it red, and the clean that follows removes it and the assertion is green again.
//! A slot that fails the assertion at slot-ready is set aside and comes back once a
//! clean passes the assertion.
//!
//! Every plant is made by the user's own processes, outside any sandbox, as a job that
//! left the sandbox would; no test runs `simctl` or another CoreSimulator tool as the
//! user, and neither does the assertion.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_assert`, with the users
//! of `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use kbf_driver_native::slots::macos::Slots;
use kbf_driver_native::slots::{AS_USER_ARG, SlotUser};
use support::pool::{Scratch, Users, home_of, wait_until};

fn assert_root() {
    support::pool::assert_root("slot_assert");
}

/// A pool of one user, started the way the daemon starts it: the clean, the warm-up and
/// the baseline.
fn start(name: &str) -> (Users, Scratch, Slots) {
    assert_root();
    let users = Users::new(name, 1);
    let scratch = Scratch::new(name);
    let pool = users.pool();
    let slots = Slots::start(&pool, Duration::from_secs(10)).expect("slots");
    (users, scratch, slots)
}

/// Waits up to a minute for `done`: the recovery thread waits 5 s before its first pass.
fn wait_a_minute(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = std::time::Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(
            std::time::Instant::now() < give_up,
            "{what}: not within a minute"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `program` with `args` as `user`, outside any sandbox, with a plain environment; its
/// stdout, and whether it exited 0.
fn as_user(user: &SlotUser, program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program)
        .args(args)
        .uid(user.uid)
        .gid(user.gid)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", home_of(user))
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `program` with `args` as root; its output, and whether it exited 0.
fn root(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// What a test plants as the user, and the line the assertion must then print.
struct Control {
    /// Names the user and the scratch directory.
    name: &'static str,
    /// The start of the line that must be among the red ones (`<probe>: + ...`).
    red: &'static str,
    /// Whether the cheap probes, run at slot-ready, must see it too.
    cheap: bool,
}

/// The positive control of one probe. With a pool of one user: the assertion is green;
/// `plant` makes its instance as the user; the assertion is red with a line that starts
/// with `control.red` (and so is the slot-ready check, if `cheap`); the slot is given
/// back, which cleans it and asserts again, and it is offered; taken again, the
/// assertion is green. `plant` returns what to say when it fails to plant, and a
/// substring the red line must carry.
async fn red_then_green(control: Control, plant: impl FnOnce(&SlotUser, &Scratch) -> String) {
    let (users, scratch, slots) = start(control.name);
    let user = users.0[0].clone();
    assert_eq!(
        slots.count(),
        1,
        "the start cleaned and recorded the baseline"
    );
    let slot = slots.take().await.expect("a slot");
    let before = slot.assert_everything().await.expect("ran");
    assert!(
        before.is_empty(),
        "red before anything was planted: {before:?}"
    );
    let ready = slot.assert_ready(&scratch.0).await.expect("ran");
    assert!(
        ready.is_empty(),
        "red at slot-ready before anything was planted: {ready:?}"
    );

    let carries = plant(&user, &scratch);
    let red = slot.assert_everything().await.expect("ran");
    assert!(
        red.iter()
            .any(|l| l.starts_with(control.red) && l.contains(&carries)),
        "the plant ({carries:?}) was not seen as {:?}: {red:?}",
        control.red
    );
    let ready = slot.assert_ready(&scratch.0).await.expect("ran");
    assert_eq!(
        ready.iter().any(|l| l.starts_with(control.red)),
        control.cheap,
        "the slot-ready check (cheap probes) on {:?}: {ready:?}",
        control.red
    );

    // The clean removes it, and the assertion that follows it is green: the slot is free.
    slot.release().await;
    wait_until("the slot is clean and offered again", || slots.count() == 1);
    let slot = slots.take().await.expect("the slot again");
    let after = slot.assert_everything().await.expect("ran");
    assert!(after.is_empty(), "red after the clean: {after:?}");
    slot.release().await;
    drop(users);
}

/// Catches an assertion that never runs against the baseline, or whose baseline is not
/// of a clean user: a used-once user that has been cleaned, taken for a lease, checked at
/// slot-ready, given back and taken again is green at every look. A baseline that held
/// something a later clean does not leave, a probe that reads its own tool's output as a
/// finding, or a slot-ready check that disagrees with the clean's would turn this red.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_user_is_green_at_every_look() {
    let (_users, scratch, slots) = start("assert-green");
    assert_eq!(slots.count(), 1);
    for round in 0..3 {
        let slot = slots.take().await.expect("a slot");
        let all = slot.assert_everything().await.expect("ran");
        assert!(all.is_empty(), "round {round}: {all:?}");
        let ready = slot.assert_ready(&scratch.0).await.expect("ran");
        assert!(ready.is_empty(), "round {round}: {ready:?}");
        slot.release().await;
        wait_until("offered again", || slots.count() == 1);
    }
}

/// Catches a process probe that cannot see a lingering process of the user (the clean
/// kills what it is shown; one it is never shown would reach the next lease).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_lingering_process_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-proc",
        red: "processes: + ",
        cheap: true,
    };
    red_then_green(control, |user, _| {
        let sleeper = support::pool::sleeper(Some(user));
        let pid = sleeper.id().to_string();
        // Reaped by the clean's kill; the child handle is dropped without waiting.
        std::mem::forget(sleeper);
        format!("{pid} /bin/sleep")
    })
    .await;
}

/// A KeepAlive agent labelled `label` that runs `sleep 300`, for the Background session a
/// pool user's domain is.
fn agent(label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>300</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Background</string>
</dict>
</plist>
"#
    )
}

/// Bootstraps the agent `label` into `user/<uid>` from the user's home, as the user
/// through `launchctl asuser` and the `as-user` drop of `drop_to`, and waits until
/// `launchctl print` lists it.
fn bootstrap(user: &SlotUser, drop_to: &str, label: &str) {
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let (made, said) = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    assert!(made, "mkdir LaunchAgents: {said}");
    let plist = agents.join(format!("{label}.plist"));
    std::fs::write(&plist, agent(label)).expect("agent plist");
    std::os::unix::fs::chown(&plist, Some(user.uid), Some(user.gid)).expect("chown");
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let domain = format!("user/{}", user.uid);
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let path = plist.to_string_lossy().into_owned();
    let service = format!("{domain}/{label}");
    let (booted, said) = root(
        "/bin/launchctl",
        &[
            "asuser",
            &uid,
            drop_to,
            AS_USER_ARG,
            &uid,
            &gid,
            "--",
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
    );
    wait_until(&format!("{label} is listed ({booted}: {said})"), || {
        root("/bin/launchctl", &["print", &service]).0
    });
}

/// Catches a launchd probe that cannot see a job a lease bootstrapped into the user's
/// domain (with `LimitLoadToSessionType=Background`, which a pool user's domain accepts),
/// or that judges the job by a label chosen to look like launchd's own: this one is named
/// `com.apple.` and lives in the user's home, so it is not launchd's.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_launch_agent_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-agent",
        red: "launchd: + job ",
        cheap: true,
    };
    red_then_green(control, |user, scratch| {
        bootstrap(
            user,
            &scratch.as_user_binary(),
            "com.apple.kbf-assert-agent",
        );
        "com.apple.kbf-assert-agent".to_owned()
    })
    .await;
}

/// Catches an IPC probe that cannot see a System V semaphore the user made (`ipcs`
/// lists it by owner and creator).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_semaphore_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-sem",
        red: "ipc: + s ",
        cheap: true,
    };
    red_then_green(control, |user, _| {
        let id = semaphore_of(user);
        format!("s {id}")
    })
    .await;
}

/// Makes a System V semaphore set as `user`, from a forked child that becomes the user
/// (a process of the test cannot change its own uid), and returns its id.
fn semaphore_of(user: &SlotUser) -> i32 {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a writable array of two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: the child calls only async-signal-safe functions (setgid, setuid, semget,
    // write, _exit) before it exits, as a fork of a threaded process must.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        // SAFETY: async-signal-safe calls only; `_exit` never returns.
        unsafe {
            if libc::setgid(user.gid) != 0 || libc::setuid(user.uid) != 0 {
                libc::_exit(1);
            }
            let id = libc::semget(libc::IPC_PRIVATE, 1, 0o600 | libc::IPC_CREAT);
            if id < 0 {
                libc::_exit(2);
            }
            let bytes = id.to_ne_bytes();
            libc::write(fds[1], bytes.as_ptr().cast(), bytes.len());
            libc::_exit(0);
        }
    }
    let mut bytes = [0_u8; 4];
    let mut status = 0;
    // SAFETY: `fds[0]` is the read end of the pipe, `bytes` a writable buffer of its
    // length, and `pid` this process's child.
    let (read, waited) = unsafe {
        libc::close(fds[1]);
        let read = libc::read(fds[0], bytes.as_mut_ptr().cast(), bytes.len());
        libc::close(fds[0]);
        (read, libc::waitpid(pid, &raw mut status, 0))
    };
    assert_eq!(waited, pid, "waitpid");
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "semget as the user: status {status:#x}"
    );
    assert_eq!(read, 4, "the child wrote the id");
    i32::from_ne_bytes(bytes)
}

/// Catches a mount probe that cannot see an image the user attached with no mount point
/// (it is in no `mount` line, only in `hdiutil info`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_image_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-image",
        red: "mounts: + image ",
        cheap: true,
    };
    red_then_green(control, |user, scratch| {
        let image: PathBuf = scratch.0.join("assert.dmg");
        let path = image.to_string_lossy().into_owned();
        let (made, said) = root(
            "/usr/bin/hdiutil",
            &[
                "create",
                "-quiet",
                "-size",
                "1m",
                "-fs",
                "HFS+",
                "-volname",
                "kbfassert",
                &path,
            ],
        );
        assert!(made, "hdiutil create: {said}");
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let (attached, said) = as_user(
            user,
            "/usr/bin/hdiutil",
            &["attach", "-quiet", "-nomount", &path],
        );
        assert!(attached, "hdiutil attach as the user: {said}");
        "attached by the uid".to_owned()
    })
    .await;
}

/// Catches a home probe that cannot see a file left in the home.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home",
        red: "home: + entry ",
        cheap: true,
    };
    red_then_green(control, |user, _| {
        let file = home_of(user).join("kbf-assert-file");
        std::fs::write(&file, b"left").expect("file");
        std::os::unix::fs::chown(&file, Some(user.uid), Some(user.gid)).expect("chown");
        "kbf-assert-file".to_owned()
    })
    .await;
}

/// Removes a path when dropped, so a failed test leaves nothing in a shared place.
struct Remove(PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Catches a shared-places probe that cannot see a file the user owns in
/// `/private/tmp` (the sweep of the clean covers it; the probe is what shows it did).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_private_tmp_is_seen_then_cleaned() {
    let name = format!("kbf-assert-tmp-{}", std::process::id());
    let _guard = Remove(Path::new("/private/tmp").join(&name));
    let control = Control {
        name: "assert-tmp",
        red: "shared: + ",
        cheap: false,
    };
    red_then_green(control, |user, _| {
        let path = format!("/private/tmp/{name}");
        std::fs::write(&path, b"left").expect("file");
        std::os::unix::fs::chown(&path, Some(user.uid), Some(user.gid)).expect("chown");
        path
    })
    .await;
}

/// Catches a schedules probe that cannot see a cron job of the user.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_cron_job_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-cron",
        red: "schedules: + ",
        cheap: false,
    };
    red_then_green(control, |user, _| {
        let mut crontab = Command::new("/usr/bin/crontab")
            .arg("-")
            .uid(user.uid)
            .gid(user.gid)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home_of(user))
            .env("USER", &user.name)
            .env("LOGNAME", &user.name)
            .current_dir("/")
            .stdin(Stdio::piped())
            .spawn()
            .expect("crontab");
        crontab
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"0 0 1 1 * /usr/bin/true\n")
            .expect("the job");
        assert!(crontab.wait().expect("crontab").success(), "crontab");
        format!("/private/var/at/tabs/{}", user.name)
    })
    .await;
}

/// Catches a slot-ready check that does not set the slot aside (a lease would run on a
/// user that holds what the last one left), and a recovery that frees the slot without
/// asserting: a process planted after the clean is seen by the cheap probes, the slot is
/// set aside and the count is 0, and once the recovery thread has cleaned and asserted
/// it the slot is offered again with the process gone.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_slot_that_fails_the_check_at_slot_ready_is_set_aside_and_comes_back() {
    let (users, scratch, slots) = start("assert-aside");
    let user = users.0[0].clone();
    let slot = slots.take().await.expect("a slot");
    let sleeper = support::pool::sleeper(Some(&user));
    let pid = i32::try_from(sleeper.id()).expect("pid");
    std::mem::forget(sleeper);
    let red = slot.assert_ready(&scratch.0).await.expect("ran");
    assert!(
        red.iter().any(|l| l.starts_with("processes: + ")),
        "{red:?}"
    );
    slot.set_aside(&format!("assertion: {}", red.join("; ")));
    assert_eq!(
        slots.count(),
        0,
        "a slot that fails the check is not counted"
    );
    wait_a_minute("the recovery cleans and asserts the slot", || {
        slots.count() == 1
    });
    assert!(
        !kbf_daemon::uid::processes(user.uid)
            .unwrap_or_default()
            .contains(&pid),
        "the recovery's clean left the process"
    );
    let slot = slots.take().await.expect("the slot again");
    let after = slot.assert_everything().await.expect("ran");
    assert!(after.is_empty(), "{after:?}");
    slot.release().await;
}
