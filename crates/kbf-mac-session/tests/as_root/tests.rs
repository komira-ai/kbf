//! The root tests, one per behaviour (the module docs of `as_root.rs` say how they run).
//! Each says which defect it catches.

use std::os::unix::fs::MetadataExt as _;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

/// Catches: a grant key file someone other than root can write accepted at start, which
/// would let that user make administrators. The helper must stop before it serves.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn a_grant_key_file_the_runner_owns_stops_the_helper() {
    let (_serial, mac) = mac();
    let keys = format!("{SCRATCH}/user-keys");
    let pubkey = mac.client(&["pubkey", "07"]);
    assert!(pubkey.ok(), "pubkey: {}", pubkey.all());
    write_root(&keys, &pubkey.stdout);
    std::os::unix::fs::chown(&keys, Some(mac.runner.uid), Some(mac.runner.gid))
        .expect("give the key file to the runner");
    let mut args = mac.helper_args();
    let at = |flag: &str| args.iter().position(|a| a == flag).expect(flag) + 1;
    let (socket, state, grant) = (at("--socket"), at("--state-dir"), at("--grant-keys"));
    args[socket] = format!("{SCRATCH}/refused-run/socket");
    args[state] = format!("{SCRATCH}/refused-state");
    args[grant] = keys;
    let mut child = Command::new(&mac.helper)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the helper");
    // A helper that accepted the file would serve until killed.
    let give_up = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("wait").is_none() && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let o = child.wait_with_output().expect("the helper's output");
    let out = Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    };
    println!("{}", out.all());
    let want = format!("owned by uid {}", mac.runner.uid);
    refused(&out, &want, "a start with the runner's key file");
    assert!(
        !exists(&format!("{SCRATCH}/refused-run/socket")),
        "the refused helper bound its socket"
    );
}

/// Catches: a verb served before the boot reset (a lease user of an earlier boot could
/// be used), a reset that leaves a stale session's auto-login, kcpassword or intent
/// record (the Mac would log a lease user in at the next boot), or a second reset.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_boot_reset_comes_first_and_clears_a_stale_session() {
    let (_serial, mac) = mac();
    let first = &mac.first;
    refused(
        &first.before_reset,
        "boot reset has not run",
        "kill-uid before the reset",
    );
    says(&first.reset, "kept none deleted ", "the first boot reset");
    assert!(
        !first.auto_login.ok(),
        "auto-login survived the reset: {}",
        first.auto_login.all()
    );
    assert!(
        first.survived.is_empty(),
        "survived the reset: {:?}",
        first.survived
    );
    refused(&first.reset_again, "runs once", "a second boot reset");
}

/// Catches: a baseline taken with no window or with one opened in this boot (a leak
/// planted before the reboot would become the baseline), a baseline whose counts are not
/// what is there (Background Task Management parsed wrong, a folder not walked), and a
/// scan that is not clean right after it; `reboot-dirty` must then be refused.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_baseline_is_taken_only_in_its_window_and_counts_what_is_there() {
    let (_serial, mac) = mac();
    let first = &mac.first;
    refused(
        &first.scan_unrecorded,
        "no baseline was recorded",
        "scan before a baseline",
    );
    refused(
        &first.baseline_closed,
        "window is closed",
        "baseline with no window",
    );
    let this_boot = format!("opened in this boot ({})", first.boot);
    refused(
        &first.baseline_this_boot,
        &this_boot,
        "baseline in the window's boot",
    );
    assert!(first.baseline.ok(), "baseline: {}", first.baseline.all());
    let classes: Vec<&str> = first.baseline.said().lines().collect();
    for want in [
        format!("background-tasks {}", first.btm),
        format!("launch-agents {}", first.agents),
        format!("launch-daemons {}", first.daemons),
        "lease-users 0".to_owned(),
        "auto-login 0".to_owned(),
    ] {
        assert!(
            classes.contains(&want.as_str()),
            "want {want:?}; the baseline: {classes:?}"
        );
    }
    says(&first.scan, "clean", "the scan right after the baseline");
    let reboot = first
        .reboot_dirty
        .as_ref()
        .expect("sent after the clean scan");
    refused(
        reboot,
        "did not come back dirty",
        "reboot-dirty after a clean scan",
    );
}

/// Catches (S4.3): a socket or socket directory other users can reach, or of the wrong
/// group. `nobody` is refused by the file system, before the helper sees it.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_socket_admits_only_its_group() {
    let (_serial, mac) = mac();
    says(
        &root("/usr/bin/stat", &["-f", "%Lp %Sg", RUN_DIR]),
        "750 admin",
        "the socket directory",
    );
    says(
        &root("/usr/bin/stat", &["-f", "%Lp %Sg", SOCKET]),
        "660 admin",
        "the socket",
    );
    let client = path_str(&mac.client);
    let nobody = root(
        "/usr/bin/sudo",
        &["-n", "-u", "nobody", client, "kill", SOCKET, "4.1"],
    );
    refused(&nobody, "ermission denied", "nobody");
}

/// Catches: a lease user made an administrator, in another group, with a home others
/// can read, listed at the login window, or holding a secure token.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn user_create_makes_a_hidden_non_admin_user_with_a_private_home() {
    let (_serial, mac) = mac();
    let lease = mac.create("5.1", None);
    let name = lease.name();
    says(&root("/usr/bin/id", &["-u", &name]), &lease.uid(), "id -u");
    says(&root("/usr/bin/id", &["-gn", &name]), "staff", "id -gn");
    assert!(!is_admin(&name), "{name} is an administrator");
    let meta = fs::symlink_metadata(home(&name)).expect("the home folder");
    assert_eq!(
        (meta.uid(), meta.mode() & 0o7777),
        (lease.uid, 0o700),
        "the home folder's owner and mode"
    );
    let path = format!("/Users/{name}");
    let hidden = succeeds("/usr/bin/dscl", &[".", "-read", &path, "IsHidden"]);
    assert_eq!(
        hidden.split_whitespace().last(),
        Some("1"),
        "IsHidden: {hidden}"
    );
    let token = root("/usr/sbin/sysadminctl", &["-secureTokenStatus", &name]);
    assert!(
        token.all().contains("DISABLED"),
        "a secure token: {}",
        token.all()
    );
    lease.end();
}

/// Catches: a user-create that leaves the baseline window open (a later boot could
/// re-baseline a leak), a scan that misses a lease user, and a baseline taken while one
/// exists.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn a_lease_user_makes_the_scan_dirty_and_closes_the_baseline_window() {
    let (_serial, mac) = mac();
    let _closed = OnPanic(|| {
        let _ = fs::remove_file(window());
    });
    // As an apply in an earlier boot leaves it: user-create must close it.
    write_root(&window(), "ci-earlier-boot\n");
    let lease = mac.create("6.1", None);
    assert!(
        !exists(&window()),
        "user-create left the baseline window open"
    );
    let scan = mac.client(&["scan", SOCKET]);
    assert!(scan.ok(), "scan: {}", scan.all());
    let lines: Vec<&str> = scan.said().lines().collect();
    assert_eq!(
        lines.first(),
        Some(&"dirty"),
        "a lease user exists: {lines:?}"
    );
    let named = format!("+ lease-users uid {}", lease.uid);
    assert!(lines.contains(&named.as_str()), "want {named:?}: {lines:?}");
    refused(
        &mac.client(&["baseline", SOCKET]),
        "lease 6.1's user exists",
        "baseline with a lease user",
    );
    lease.end();
}

/// Catches: an action run as another user, outside the lease directory, with another
/// home, or its exit code lost.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn run_runs_as_the_lease_user_in_the_lease_directory() {
    let (_serial, mac) = mac();
    // The daemon opens the lease directory and the lease user works in it (on a node an
    // inherited ACL entry gives the daemon access; here it is open to all).
    let dir = scratch_dir("lease-7", 0o777);
    let lease = mac.create("7.1", None);
    let steps = ["run", "/usr/bin/id", "-u", ";", "pwd", ";", "env", "HOME"];
    let out = lease.run(&dir, &mac.act(&steps));
    let want = format!("{}\n{dir}\n{}\nexit 0", lease.uid, home(&lease.name()));
    says(&out, &want, "run");
    says(
        &lease.run(&dir, &mac.act(&["exit", "7"])),
        "exit 7",
        "the exit code",
    );
    lease.end();
}

/// Catches (S4.3 control 1): an action that reaches the helper's socket, so a lease
/// could ask for another lease's user.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn an_action_cannot_reach_the_helper() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-8", 0o777);
    let lease = mac.create("8.1", None);
    let client = path_str(&mac.client);
    let out = lease.run(&dir, &[client, "kill", SOCKET, "8.1"]);
    let all = out.all();
    assert!(
        all.contains("exit 4"),
        "the lease user reached the socket: {all}"
    );
    assert!(
        all.contains("ermission denied"),
        "not refused by the socket's mode: {all}"
    );
    lease.end();
}

/// Catches (S10): a caller accepted whose code is not the daemon's (any cdhash
/// accepted), and the S4.3 attack of connecting as one binary and then executing the
/// genuine one, checked at the request rather than at accept. The genuine client over
/// the same path is the control.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn only_the_daemon_s_code_is_served() {
    let (_serial, mac) = mac();
    let lease = mac.create("20.1", None);
    let client = path_str(&mac.client);
    refused(
        &mac.impostor(&["kill", SOCKET, "20.1"]),
        "caller refused",
        "the impostor",
    );
    // The impostor connects, waits for the helper's answer, then executes the genuine
    // client, which carries out the whole exchange on that connection: refused, because
    // the helper checked the impostor at accept.
    refused(
        &mac.impostor(&["connect-then-exec", SOCKET, client, "kill", SOCKET, "20.1"]),
        "caller refused",
        "connect, then execute the client",
    );
    // The impostor writes its request at once, then executes the genuine client, which
    // only reads: refused at accept, or (had the exec won the race to accept) for the
    // nonce.
    refused(
        &mac.impostor(&["write-then-exec", SOCKET, "20.1", client, "replies"]),
        "caller refused",
        "write, then execute the client",
    );
    // What the audit token says across an exec on this macOS (no helper involved).
    mac.client(&["exec-probe"]);
    let genuine = mac.client(&["kill", SOCKET, "20.1"]);
    assert!(
        genuine.ok(),
        "the genuine client was refused: {}",
        genuine.all()
    );
    lease.end();
}

/// Catches: user-delete that removes a user while a process of its uid remains (or
/// says so without naming it), a kill-uid that misses one, and a sweep that leaves the
/// user's state (crontab, files in shared folders, a locked file, its temporary folder,
/// launchd's per-uid override) or follows a link out of it.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn user_delete_waits_for_the_uid_s_processes_and_sweeps_without_following_links() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-10", 0o777);
    let shared = home("Shared");
    let sentinel = "/private/var/kbf-ci-sentinel";
    fs::create_dir_all(sentinel).expect("the sentinel directory");
    write_root(&format!("{sentinel}/keep"), "");
    let lease = mac.create("10.1", None);
    let (uid, name) = (lease.uid(), lease.name());
    let override_plist = format!("/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist");
    let target = format!("user/{uid}/kbf.ci.planted");
    let override_plist = if root("/bin/launchctl", &["disable", &target]).ok()
        && exists(&override_plist)
    {
        println!("planted {override_plist}");
        Some(override_plist)
    } else {
        println!(
            "warning: launchctl wrote no per-uid override here; that sweep item is not exercised"
        );
        None
    };
    // The lease user plants its state: a crontab (from a file every user may read),
    // files in the shared folder, a link out of it, a locked file, a file in its
    // temporary folder (where `getconf` says, as that user), and a process left running.
    let crontab = format!("{SCRATCH}/crontab-10");
    write_root(&crontab, "* * * * * /usr/bin/true\n");
    let temp = lease.run(&dir, &["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"]);
    let temp = temp
        .said()
        .strip_suffix("\nexit 0")
        .unwrap_or_else(|| panic!("getconf DARWIN_USER_TEMP_DIR: {}", temp.all()));
    let owned = format!("{shared}/kbf-ci-owned");
    let sub = format!("{shared}/kbf-ci-dir/sub");
    let in_sub = format!("{sub}/f");
    let link = format!("{shared}/kbf-ci-link");
    let locked = "/private/tmp/kbf-ci-locked";
    let in_temp = format!("{temp}kbf-ci-temp");
    let plant: [&[&str]; 8] = [
        &["run", "/usr/bin/crontab", &crontab, ";"],
        &["run", "/usr/bin/touch", &owned, ";"],
        &["mkdir", &sub, ";"],
        &["run", "/usr/bin/touch", &in_sub, ";"],
        &["symlink", sentinel, &link, ";"],
        &["run", "/usr/bin/touch", locked, ";"],
        &["run", "/usr/bin/chflags", "uchg", locked, ";"],
        &["run", "/usr/bin/touch", &in_temp],
    ];
    says(
        &lease.run(&dir, &mac.act(&plant.concat())),
        "exit 0",
        "plant",
    );
    let sleep = ["spawn-child", "run", "/bin/sleep", "600"];
    says(
        &lease.run_quiet(&dir, &mac.act(&sleep)),
        "exit 0",
        "start a sleep",
    );
    let tabs = format!("/private/var/at/tabs/{name}");
    assert!(exists(&tabs), "no crontab was planted");
    let temp = root(
        "/usr/bin/find",
        &[
            "/private/var/folders",
            "-user",
            &uid,
            "-name",
            "kbf-ci-temp",
        ],
    );
    assert_eq!(
        temp.said().lines().count(),
        1,
        "no temporary file: {}",
        temp.all()
    );
    let sleeper = pgrep(&["-U", &uid, "sleep"]);
    let [sleeper] = sleeper.as_slice() else {
        panic!("not one sleep process: {sleeper:?}");
    };

    // user-delete waits about five seconds for the process to exit, then refuses.
    let out = lease.call("delete");
    refused(
        &out,
        &format!("processes of uid {uid} remain after "),
        "user-delete",
    );
    assert!(out.all().contains(" looks: "), "{}", out.all());
    assert!(
        out.all().contains(&format!("{sleeper} (sleep)")),
        "{}",
        out.all()
    );
    assert!(user_exists(&name), "the refused delete removed the user");

    lease.kill();
    let left = pgrep(&["-U", &uid]);
    assert!(
        left.is_empty(),
        "processes of {uid} survived kill-uid: {left:?}"
    );

    says(&lease.call("delete"), "existed", "user-delete");
    assert!(!user_exists(&name), "the user record survived");
    let mut gone = vec![
        home(&name),
        tabs,
        format!("{shared}/kbf-ci-owned"),
        format!("{shared}/kbf-ci-dir"),
        format!("{shared}/kbf-ci-link"),
        "/private/tmp/kbf-ci-locked".to_owned(),
    ];
    gone.extend(override_plist);
    let survived: Vec<&String> = gone.iter().filter(|path| exists(path)).collect();
    assert!(survived.is_empty(), "survived the sweep: {survived:?}");
    let files = root("/usr/bin/find", &["/private/var/folders", "-user", &uid]);
    assert_eq!(
        files.said(),
        "",
        "files of {uid} remain in /private/var/folders"
    );
    assert!(
        exists(&format!("{sentinel}/keep")),
        "the sweep followed the planted link"
    );
    says(&lease.call("delete"), "absent", "a repeated delete");
}

/// Catches: a lease id (and so a user name) used twice, which would hand a new lease
/// the name of one the server already ended.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn a_lease_id_is_never_used_twice() {
    let (_serial, mac) = mac();
    let lease = mac.create("10.2", None);
    lease.end();
    refused(
        &mac.client(&["create", SOCKET, "10.2"]),
        "never reused",
        "create again",
    );
    refused(&lease.call("kill"), "was deleted", "kill-uid after delete");
}

/// Catches: an administrator made without a grant, or with a grant for another lease,
/// a forged one, one for another Mac, or one used twice; and a lease user left in the
/// admin group after its delete.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn an_administrator_only_with_a_valid_gate_grant_used_once() {
    let (_serial, mac) = mac();
    let plain = mac.create("11.1", None);
    assert!(
        !is_admin(&plain.name()),
        "{} is an administrator",
        plain.name()
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
        .to_string();
    let grant = |seed: &str, serial: &str, lease: &str| {
        let out = mac.client(&["grant", seed, serial, lease, &now]);
        assert!(out.ok(), "grant: {}", out.all());
        out.said().to_owned()
    };
    let token = grant("07", &mac.serial, "11.2");
    let admin = mac.create("11.2", Some(&token));
    assert!(
        is_admin(&admin.name()),
        "{} is not an administrator",
        admin.name()
    );
    let create = |lease: &str, token: &str| mac.client(&["create", SOCKET, lease, token]);
    refused(
        &create("11.3", &token),
        "names lease 11.2",
        "another lease's grant",
    );
    refused(
        &create("11.2", &token),
        "never reused",
        "the grant used twice",
    );
    let forged = grant("08", &mac.serial, "11.3");
    refused(
        &create("11.3", &forged),
        "does not verify",
        "a forged grant",
    );
    let elsewhere = grant("07", "OTHERSERIAL", "11.3");
    refused(&create("11.3", &elsewhere), "serial", "another Mac's grant");
    plain.end();
    admin.end();
    let members = root(
        "/usr/bin/dscl",
        &[".", "-read", "/Groups/admin", "GroupMembership"],
    );
    assert!(
        !members.all().contains("kbf-lease"),
        "a lease user is still in the admin group: {}",
        members.all()
    );
}

/// Catches: a ledger that does not survive a restart of the helper (a used lease id or a
/// uid handed out again; [`Mac::create`] checks the uid after the restart).
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_ledger_survives_a_restart_of_the_helper() {
    let (_serial, mac) = mac();
    let used = mac.create("12.1", None);
    used.end();
    assert_eq!(
        mac.restart(&[]),
        "kept none deleted ",
        "the boot reset after a restart"
    );
    says(
        &used.call("delete"),
        "absent",
        "the deleted lease, from the reread ledger",
    );
    refused(
        &mac.client(&["create", SOCKET, "12.1"]),
        "never reused",
        "create again",
    );
    mac.create("12.2", None).end();
}

/// Catches: user-delete that refuses at once instead of waiting for a process of the
/// uid that is about to exit by itself.
///
/// The process is `kbf-act`'s spawned child itself, sleeping 2 s and then exiting
/// (komira-build-farm#639: a `/bin/sleep` that the child started was looked for while
/// the child had not started it yet, 24 ms after the action ended, and was not found).
/// The action writes the child's pid after spawning it and before it exits, so the
/// child exists once `run-quiet` returns: there is nothing to wait for before looking.
/// The look checks it is that pid, of the lease's uid, still alive; the delete follows
/// at once. User-delete's first look waits up to `EXIT_LOOKS` (50) listings 100 ms
/// apart, at least 4.9 s, and the child has at most 2 s of sleep left plus its own
/// start, so the delete waits it out; a delete that refused at once would meet it.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn user_delete_waits_for_a_process_that_exits_by_itself() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-13", 0o777);
    let pid_file = format!("{dir}/child.pid");
    let lease = mac.create("13.1", None);
    lease.kill();
    let sleep: [&str; 5] = ["spawn-child", "--pid", &pid_file, "sleep", "2"];
    says(
        &lease.run_quiet(&dir, &mac.act(&sleep)),
        "exit 0",
        "start a sleeping child",
    );
    let pid = fs::read_to_string(&pid_file).unwrap_or_else(|e| panic!("{pid_file}: {e}"));
    let pid = pid.trim();
    let alive = pgrep(&["-U", &lease.uid(), "kbf-act"]);
    assert!(
        alive.iter().any(|p| p == pid),
        "the child {pid} is not a live process of the lease user: {alive:?}"
    );
    says(
        &lease.call("delete"),
        "existed",
        "delete did not wait for the sleeping child",
    );
}

/// The second of the minute, from the wall clock (time zones differ by whole minutes).
fn second() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
        % 60
}

/// Catches (issue #195): kill-uid that leaves the lease user's crontab, so cron starts
/// its job (and launchd per-user agents with it) after the kill, and the delete is
/// refused. Staged, not left to chance: the crontab runs a 3 s sleep every minute, and
/// kill-uid comes in the last seconds of a minute; then no process of the uid may start
/// as the next minute begins. Each of the two rounds takes up to a minute.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn no_cron_job_of_the_lease_user_starts_after_kill_uid() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-14", 0o777);
    let crontab = format!("{SCRATCH}/crontab-14");
    write_root(&crontab, "* * * * * /bin/sleep 3\n");
    for round in 1..=2 {
        let lease = mac.create(&format!("14.{round}"), None);
        let uid = lease.uid();
        lease.run(&dir, &["/usr/bin/crontab", &crontab]);
        let tabs = format!("/private/var/at/tabs/{}", lease.name());
        assert!(exists(&tabs), "round {round}: no crontab was planted");
        // Out of the last seconds of this minute, then into those of the next.
        while second() >= 55 {
            std::thread::sleep(Duration::from_millis(200));
        }
        while second() < 57 {
            std::thread::sleep(Duration::from_millis(200));
        }
        lease.kill();
        let done = second();
        println!("round {round}: kill-uid done at second {done}");
        // Ended past :04, the watch below would look at nothing: not staged.
        assert!(
            !(4..55).contains(&done),
            "round {round}: kill-uid ended at second {done}, too late to stage the race"
        );
        assert!(!exists(&tabs), "round {round}: kill-uid left the crontab");
        // Watch past the minute's start, when cron would have started the job.
        while second() >= 55 || second() < 4 {
            let started = pgrep(&["-lU", &uid]);
            assert!(
                started.is_empty(),
                "round {round}: a process of {uid} started after kill-uid: {started:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        says(&lease.call("delete"), "existed", "user-delete");
    }
}

/// Catches: a boot reset that deletes the user of the session the server still holds
/// (or drops its auto-login), keeps any other leftover lease user, lets the held
/// session's user be deleted, or keeps the session once the server no longer holds it.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_boot_reset_keeps_the_held_session_and_deletes_every_other_leftover() {
    let (_serial, mac) = mac();
    let held = mac.create("15.1", None);
    let leftover = mac.create("15.2", None);
    // Dropped before the leases: their delete is refused while the session is held.
    let _clear = OnPanic(clear_session);
    // What session-login leaves (it is not run here: it restarts userspace).
    write_root(&intent(), "15.1\n");
    succeeds(
        "/usr/bin/defaults",
        &[
            "write",
            &loginwindow(),
            "autoLoginUser",
            "-string",
            &held.name(),
        ],
    );
    write_root(&kcpassword(), "x\n");
    assert_eq!(
        mac.restart(&["15.1"]),
        "kept 15.1 deleted 15.2",
        "reset, 15.1 held"
    );
    assert!(
        user_exists(&held.name()),
        "the held session's user was deleted"
    );
    assert!(
        !user_exists(&leftover.name()),
        "the leftover user survived the reset"
    );
    says(
        &root(
            "/usr/bin/defaults",
            &["read", &loginwindow(), "autoLoginUser"],
        ),
        &held.name(),
        "auto-login",
    );
    assert!(exists(&kcpassword()), "kcpassword was not kept");
    refused(
        &held.call("delete"),
        "holds the GUI session",
        "delete the held session's user",
    );
    // Booted again, and the server no longer holds 15.1: everything goes.
    assert_eq!(
        mac.restart(&[]),
        "kept none deleted 15.1",
        "reset, nothing held"
    );
    assert!(
        !user_exists(&held.name()),
        "{} survived the reset",
        held.name()
    );
    let auto_login = root(
        "/usr/bin/defaults",
        &["read", &loginwindow(), "autoLoginUser"],
    );
    assert!(
        !auto_login.ok(),
        "auto-login survived the reset: {}",
        auto_login.all()
    );
    for gone in [kcpassword(), intent()] {
        assert!(!exists(&gone), "{gone} survived the reset");
    }
}

/// Catches: a scan that stays dirty after every lease user is gone (an item that
/// counts a deleted user), a baseline that does not survive a restart, a `reboot-dirty`
/// that reboots after a clean scan, and a baseline window reopened by a restart.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn the_scan_is_clean_again_once_every_lease_user_is_gone() {
    let (_serial, mac) = mac();
    mac.create("16.1", None).end();
    assert_eq!(mac.restart(&[]), "kept none deleted ", "the boot reset");
    // The order matters: reboot-dirty only after a scan that said clean.
    says(
        &mac.client(&["scan", SOCKET]),
        "clean",
        "the scan after every lease",
    );
    refused(
        &mac.client(&["reboot-dirty", SOCKET]),
        "did not come back dirty",
        "reboot-dirty after a clean scan",
    );
    refused(
        &mac.client(&["baseline", SOCKET]),
        "window is closed",
        "baseline",
    );
}

/// Catches: `session-ready` or `run-gui` that answer for a lease that does not hold the
/// GUI session, or that run in a GUI domain that does not exist.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn run_gui_is_refused_until_the_lease_user_s_gui_domain_exists() {
    let (_serial, mac) = mac();
    let dir = scratch_dir("lease-17", 0o777);
    let lease = mac.create("17.1", None);
    // Dropped before the lease: its delete is refused while the session is held.
    let _clear = OnPanic(clear_session);
    refused(
        &lease.call("session-ready"),
        "does not hold the GUI session",
        "session-ready, no session",
    );
    // What session-login leaves for the lease (it is not run here: it restarts
    // userspace).
    write_root(&intent(), "17.1\n");
    says(
        &lease.call("session-ready"),
        "false",
        "a user never logged in",
    );
    let gui = mac.client(&["run-gui", SOCKET, "17.1", &dir, "/usr/bin/true"]);
    refused(&gui, "does not exist", "run-gui with no GUI domain");
    fs::remove_file(intent()).expect("remove the intent record");
    lease.end();
}

/// Escapes `text` for a plist's XML.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The client's `spawn <session> ...` started by a root LaunchDaemon in the system
/// domain, as the helper runs on a node: what it printed.
fn spawn_in(mac: &Mac, session: &str, argv: &[&str]) -> String {
    let label = format!("kbf.ci.mac-session.spawn-{session}");
    let out = format!("{SCRATCH}/spawn-{session}.out");
    let _ = fs::remove_file(&out);
    let me = mac.runner.uid.to_string();
    let gid = mac.runner.gid.to_string();
    let dir = scratch_dir(&format!("gui-{session}"), 0o755);
    let mut words = vec![
        path_str(&mac.client),
        "spawn",
        session,
        me.as_str(),
        gid.as_str(),
    ];
    words.extend([mac.runner.name.as_str(), dir.as_str()]);
    words.extend(argv);
    let mut args = String::new();
    for word in &words {
        args.push_str("<string>");
        args.push_str(&xml(word));
        args.push_str("</string>");
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>{args}</array>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{out}.err</string>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
    );
    // launchd loads a system job only from a file root owns and no one else may write.
    let file = format!("{SCRATCH}/{label}.plist");
    write_root(&file, &plist);
    std::os::unix::fs::chown(&file, Some(0), Some(0)).expect("root:wheel");
    succeeds("/bin/launchctl", &["bootstrap", "system", &file]);
    let give_up = Instant::now() + Duration::from_secs(30);
    let finished =
        || fs::read_to_string(&out).is_ok_and(|text| text.lines().any(|l| l.starts_with("exit ")));
    while !finished() && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(200));
    }
    root("/bin/launchctl", &["bootout", &format!("system/{label}")]);
    let printed = fs::read_to_string(&out).unwrap_or_default();
    let errors = fs::read_to_string(format!("{out}.err")).unwrap_or_default();
    println!("{session}: {printed}{errors}");
    printed.trim_end_matches('\n').to_owned()
}

/// Catches: a GUI run that does not land in the user's Aqua session, or lands there with
/// another uid or group, or as a process that can still read a file only wheel may read.
/// The control, a background run, stays in the system's session.
///
/// No lease user has a GUI session here (that takes a session switch, which restarts
/// userspace), so `spawn::spawn` runs for the runner's own uid, from a root
/// LaunchDaemon, as the helper runs on a node. Not from this process: it is already in
/// the runner's Aqua session; and not under `launchctl bsexec 1`, where the nested
/// `launchctl asuser` refuses with "Reentrancy avoided".
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn a_gui_spawn_lands_in_aqua_and_a_background_one_in_the_system_session() {
    let (_serial, mac) = mac();
    let me = mac.runner.uid;
    let domain = quiet("/bin/launchctl", &["print", &format!("gui/{me}")]);
    assert!(
        domain.ok(),
        "the runner has no gui/{me} domain: {}",
        domain.stderr
    );
    let wheel_only = format!("{SCRATCH}/wheel-only");
    write_root(&wheel_only, "secret\n");
    std::os::unix::fs::chown(&wheel_only, Some(0), Some(0)).expect("root:wheel");
    set_mode(&wheel_only, 0o040);
    // One process prints its session's manager, its uid and group, its directory and
    // home, and then reads the file only wheel may read: the last step, so a refused
    // read ends the steps with exit 1, and a read prints the file and exits 0.
    let probe: [&[&str]; 4] = [
        &["run", "/bin/launchctl", "managername", ";"],
        &["run", "/usr/bin/id", "-u", ";"],
        &["run", "/usr/bin/id", "-g", ";"],
        &["pwd", ";", "env", "HOME", ";", "cat", &wheel_only],
    ];
    let argv = mac.act(&probe.concat());
    let control = spawn_in(mac, "background", &argv);
    assert_eq!(
        control.lines().next(),
        Some("System"),
        "the control is not in the system's session: {control:?}"
    );
    let gui = spawn_in(mac, "gui", &argv);
    let dir = fs::canonicalize(format!("{SCRATCH}/gui-gui")).expect("the directory");
    let want = [
        "Aqua".to_owned(),
        me.to_string(),
        mac.runner.gid.to_string(),
        dir.display().to_string(),
        home(&mac.runner.name),
        "exit 1".to_owned(),
    ]
    .join("\n");
    assert_eq!(gui, want, "the GUI spawn");
}

/// Catches: a GUI run that keeps root's supplementary groups. Under sudo, root holds
/// its whole group list (initgroups), which this process passes on; the GUI run's
/// process must hold the user's primary group alone (the kernel's list, `getgroups`,
/// not `id -G`, which asks the directory).
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`"]
fn a_gui_run_holds_no_group_but_its_own() {
    let (_serial, mac) = mac();
    let client = path_str(&mac.client);
    let root_groups = succeeds(client, &["groups"]);
    assert!(
        root_groups.split_whitespace().count() > 1,
        "root holds one group here, so the check below cannot fail: {root_groups}"
    );
    let me = mac.runner.uid.to_string();
    let gid = mac.runner.gid.to_string();
    let dir = scratch_dir("gui-groups", 0o755);
    let name = mac.runner.name.as_str();
    let held = succeeds(
        client,
        &["spawn", "gui", &me, &gid, name, &dir, client, "groups"],
    );
    let lines: Vec<&str> = held.lines().collect();
    // Its own primary group, or none at all, as the kernel keeps an empty list.
    assert!(
        matches!(lines.first(), Some(&first) if first == gid || first.is_empty()),
        "the GUI run kept groups: {held:?}"
    );
    assert_eq!(lines.last(), Some(&"exit 0"), "{held:?}");
}
