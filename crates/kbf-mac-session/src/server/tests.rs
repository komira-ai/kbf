use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::autologin::SessionPaths;
use crate::client::{Client, ClientError, Exit};
use crate::helper::Settings;
use crate::lease::UidRange;
use crate::ledger::Ledger;
use crate::proto::Request;
use crate::sweep::SweepPlan;
use crate::testing::{FakeHost, act_argv, scratch, trace};

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn my_gid() -> u32 {
    rustix::process::getgid().as_raw()
}

/// The process a [`Scripted`] check names when its script runs out.
fn caller(pid: i64) -> Caller {
    Caller {
        token: vec![u8::try_from(pid).unwrap(); 4],
        pid,
        version: 1,
    }
}

/// A caller check over a script: `identify` answers from `ids` in turn (then
/// `caller(1)`), `check` answers `verdict`; `asked` counts the identifications.
struct Scripted {
    ids: Mutex<Vec<Result<Caller, String>>>,
    verdict: Result<(), String>,
    asked: AtomicUsize,
}

impl Scripted {
    fn new(ids: Vec<Result<Caller, String>>, verdict: Result<(), String>) -> Arc<Self> {
        Arc::new(Self {
            ids: Mutex::new(ids.into_iter().rev().collect()),
            verdict,
            asked: AtomicUsize::new(0),
        })
    }

    fn fixed(verdict: Result<(), String>) -> Arc<Self> {
        Self::new(Vec::new(), verdict)
    }
}

impl CallerCheck for Scripted {
    fn identify(&self, _socket: &UnixStream) -> Result<Caller, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.ids
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Ok(caller(1)))
    }

    fn check(&self, _caller: &Caller) -> Result<(), String> {
        self.verdict.clone()
    }
}

struct Served {
    host: FakeHost,
    client: Client,
    dir: kbf_testkit::dir::TestDir,
}

/// A helper over a fake host, serving on a socket in a scratch directory.
fn served(name: &str, verdict: Result<(), String>) -> Served {
    served_with(name, Scripted::fixed(verdict))
}

fn served_with(name: &str, check: Arc<Scripted>) -> Served {
    served_from(name, check, true)
}

/// A helper served on a socket, its boot reset done if `reset`.
fn served_from(name: &str, check: Arc<Scripted>, reset: bool) -> Served {
    served_vm(name, check, reset, None)
}

/// [`served_from`] on a Mac whose VM user is `vm_user`, if any.
fn served_vm(name: &str, check: Arc<Scripted>, reset: bool, vm_user: Option<&str>) -> Served {
    served_in(
        name,
        check,
        reset,
        vm_user,
        UidRange::new(me(), me() + 1).unwrap(),
    )
}

/// [`served_vm`] with the lease users' uid `range`.
fn served_in(
    name: &str,
    check: Arc<Scripted>,
    reset: bool,
    vm_user: Option<&str>,
    range: UidRange,
) -> Served {
    trace();
    let dir = scratch(&format!("server-{name}"));
    std::fs::create_dir(dir.join("Users")).unwrap();
    let host = FakeHost::default();
    if vm_user.is_some() {
        // What provisioning wrote: 64 hex digits, mode 0400.
        let file = dir.join("vm-user-password");
        std::fs::write(&file, "ab".repeat(32)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let settings = Settings {
        range,
        gid: my_gid(),
        homes: dir.join("Users"),
        schedules: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        sweep: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        grant_keys: None,
        serial: "S".to_owned(),
        session: SessionPaths::under(&dir),
        scan: crate::scan::ScanSettings::under(&dir),
        image: crate::helper::ImageSettings::under(&dir),
        vm_user: vm_user.map(|name| crate::lease::VmUser::new(name).unwrap()),
    };
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let helper = Arc::new(Helper::new(Box::new(host.clone()), settings, ledger));
    if reset {
        helper.boot_reset(&[]).unwrap();
        host.state().log.clear();
    }
    let socket = dir.join("run/socket");
    let listener = bind(&socket, my_gid()).unwrap();
    let check: Arc<dyn CallerCheck> = check;
    std::thread::spawn(move || serve(&listener, &helper, &check));
    Served {
        host,
        client: Client::new(socket),
        dir,
    }
}

/// Catches: a socket any local user can reach (S10: "socket 0666"), or a directory
/// that lets other users in, or one whose group is not the daemon's.
#[test]
fn the_socket_and_its_directory_admit_only_their_group() {
    let dir = scratch("server-bind");
    let socket = dir.join("run/socket");
    let listener = bind(&socket, my_gid()).unwrap();
    let meta = std::fs::metadata(&socket).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o660);
    assert_eq!(meta.gid(), my_gid());
    let parent = std::fs::metadata(dir.join("run")).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o750);
    assert_eq!(parent.gid(), my_gid());
    drop(listener);

    // A stale socket is replaced, and a directory left too open is closed up.
    std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let _again = bind(&socket, my_gid()).unwrap();
    let parent = std::fs::metadata(dir.join("run")).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o750);
}

/// Catches: the socket's directory taken through a link someone planted.
#[test]
fn a_linked_or_missing_directory_is_refused() {
    let dir = scratch("server-bind-bad");
    std::fs::create_dir(dir.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("run")).unwrap();
    assert!(bind(&dir.join("run/socket"), my_gid()).is_err());
    assert!(bind(&dir.join("no/such/socket"), my_gid()).is_err());
    let error = bind(Path::new("socket"), my_gid()).unwrap_err();
    assert!(error.to_string().contains("needs a directory"), "{error}");
    let error = bind(&dir.join("elsewhere/.."), my_gid()).unwrap_err();
    assert!(error.to_string().contains("needs a name"), "{error}");
    std::fs::create_dir(dir.join("elsewhere/socket")).unwrap();
    assert!(bind(&dir.join("elsewhere/socket"), my_gid()).is_err());
}

/// Catches: the session switch or the boot reset answered with the wrong reply over
/// the socket, or a refusal of either taken by the client as success.
#[test]
fn the_switch_and_the_reset_work_over_the_socket() {
    let served = served_from("switch", Scripted::fixed(Ok(())), false);
    let client = &served.client;
    assert_eq!(client.boot_reset(&[]).unwrap(), (None, Vec::new()));
    match client.boot_reset(&[]) {
        Err(ClientError::Refused(why)) => assert!(why.contains("runs once"), "{why}"),
        other => panic!("a second reset: {other:?}"),
    }
    match client.session_login("1.1") {
        Err(ClientError::Refused(why)) => assert!(why.contains("has no user"), "{why}"),
        other => panic!("a login without a user: {other:?}"),
    }
    client.user_create("1.1", None).unwrap();
    client.session_login("1.1").unwrap();
    client.session_idle().unwrap();
    let log = served.host.state().log.clone();
    let restarts = log.iter().filter(|call| call.contains("reboot userspace"));
    assert_eq!(restarts.count(), 2, "{log:#?}");
}

/// Catches: the VM user's verbs answered with the wrong reply over the socket, a refusal
/// taken by the client as success, or a Mac with no VM user answering them.
#[test]
fn the_role_verbs_work_over_the_socket() {
    let refused = |result: Result<(), ClientError>, says: &str| match result {
        Err(ClientError::Refused(why)) => assert!(why.contains(says), "{why}"),
        other => panic!("{other:?}"),
    };
    let null = std::fs::File::open("/dev/null").unwrap();
    let fds = [null.as_fd(), null.as_fd(), null.as_fd(), null.as_fd()];
    let none = served("role-none", Ok(()));
    refused(none.client.role_session_ready().map(drop), "no VM user");
    refused(none.client.role_kill_uid(), "no VM user");
    refused(none.client.role_reap(4242, 1, "boot-2"), "no VM user");
    refused(
        none.client.role_run(&argv(&none.dir), &[], fds).map(drop),
        "no VM user",
    );

    let served = served_vm("role", Scripted::fixed(Ok(())), true, Some("kbf-vm"));
    let client = &served.client;
    refused(client.role_session_ready().map(drop), "has no account");
    served.host.state().users.insert(
        "kbf-vm".to_owned(),
        crate::helper::NewUser {
            name: "kbf-vm".to_owned(),
            uid: me() + 10,
            gid: my_gid(),
            home: served.dir.join("Users/kbf-vm"),
            admin: false,
        },
    );
    assert!(!client.role_session_ready().unwrap());
    refused(
        client.role_run(&argv(&served.dir), &[], fds).map(drop),
        "does not exist",
    );
    served
        .host
        .state()
        .domains
        .insert(format!("gui/{}", me() + 10));
    assert!(client.role_session_ready().unwrap());
    client.role_kill_uid().unwrap();
    // A group the table does not show: nothing to end, and the reply is `killed`.
    client.role_reap(4242, 1, "boot-2").unwrap();
    refused(client.role_reap(1, 1, "boot-2"), "will signal");
    // The session switch rests at the VM user, over the socket.
    client.session_idle().unwrap();
    assert_eq!(
        std::fs::read_to_string(served.dir.join("session-intent")).unwrap(),
        "vm\n"
    );
}

/// Catches: the image verbs answered with the wrong reply over the socket, a refusal
/// taken by the client as success, or a report lost on the way.
#[test]
fn the_image_verbs_work_over_the_socket() {
    // The VM user is this test's own account, the one a test can hand a file to.
    let range = UidRange::new(me() + 100, me() + 101).unwrap();
    let served = served_in(
        "image",
        Scripted::fixed(Ok(())),
        true,
        Some("kbf-vm"),
        range,
    );
    let client = &served.client;
    let image = crate::helper::ImageSettings::under(&served.dir);
    let refused = |result: Result<String, ClientError>, says: &str| match result {
        Err(ClientError::Refused(why)) => assert!(why.contains(says), "{why}"),
        other => panic!("{other:?}"),
    };
    refused(
        client.image_setup("/elsewhere/b"),
        "not inside the staging directory",
    );
    refused(
        client.image_harvest("/elsewhere/b", "/elsewhere/o"),
        "inside neither",
    );
    std::fs::create_dir(&image.dir).unwrap();
    std::fs::create_dir(&image.staging).unwrap();
    for program in [&image.vmm, &image.guest] {
        std::fs::write(program, "program").unwrap();
        std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::set_permissions(&image.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bundle = image.staging.join("clone");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("disk.img"), "d").unwrap();
    let dir = |name: &str| image.dir.join(name).display().to_string();
    let staged = |name: &str| image.staging.join(name).display().to_string();
    // Not the VM user's account yet: setup is refused before it takes the bundle.
    refused(client.image_setup(&staged("clone")), "has no account");
    served.host.state().users.insert(
        "kbf-vm".to_owned(),
        crate::helper::NewUser {
            name: "kbf-vm".to_owned(),
            uid: me(),
            gid: my_gid(),
            home: served.dir.join("Users/kbf-vm"),
            admin: false,
        },
    );
    let setup = format!(
        "{} image setup --base {} --guest-binary {}",
        image.vmm.display(),
        dir("clone"),
        image.guest.display()
    );
    served.host.state().answers.insert(
        setup,
        crate::autologin::CommandOutput {
            success: true,
            stdout: "{\"setup\":1}".to_owned(),
            stderr: String::new(),
        },
    );
    assert_eq!(
        client.image_setup(&staged("clone")).unwrap(),
        "{\"setup\":1}"
    );
    assert!(image.dir.join("clone").is_dir());
    assert!(bundle.exists(), "the original is left alone");
    served.host.state().answers.insert(
        format!(
            "{} image harvest --bundle {} --out {}/files",
            image.vmm.display(),
            dir("clone"),
            dir("out")
        ),
        crate::autologin::CommandOutput {
            success: true,
            stdout: "{\"harvest\":2}".to_owned(),
            stderr: String::new(),
        },
    );
    assert_eq!(
        client.image_harvest(&dir("clone"), &dir("out")).unwrap(),
        "{\"harvest\":2}"
    );
    assert!(image.dir.join("out").is_dir());
    refused(
        client.image_harvest(&dir("clone"), &dir("out")),
        "making out in the images directory",
    );
}

fn argv(dir: &std::path::Path) -> Vec<String> {
    crate::helper::ImageSettings::under(dir).vm_run_argv()
}

/// Catches: the leak scan's verbs answered with the wrong reply over the socket, a
/// refusal of one taken by the client as success, or the scan's lists lost on the way.
#[test]
fn the_scan_verbs_work_over_the_socket() {
    let served = served("scan", Ok(()));
    let client = &served.client;
    let refused = |result: Result<(), ClientError>, says: &str| match result {
        Err(ClientError::Refused(why)) => assert!(why.contains(says), "{why}"),
        other => panic!("{other:?}"),
    };
    refused(client.scan().map(drop), "no baseline");
    refused(client.baseline().map(drop), "window is closed");
    let window = served.dir.join("baseline-window");
    std::fs::write(&window, "boot-1\n").unwrap();
    std::fs::set_permissions(&window, std::fs::Permissions::from_mode(0o600)).unwrap();
    let classes = client.baseline().unwrap();
    assert!(
        classes.contains(&("launch-daemons".to_owned(), 0)),
        "{classes:?}"
    );
    assert!(client.scan().unwrap().clean);
    refused(client.reboot_dirty(), "did not come back dirty");
    std::fs::create_dir(served.dir.join("LaunchDaemons")).unwrap();
    std::fs::write(served.dir.join("LaunchDaemons/evil.plist"), "x").unwrap();
    let scanned = client.scan().unwrap();
    assert!(!scanned.clean);
    assert_eq!(scanned.added.len(), 1, "{scanned:?}");
    assert!(scanned.removed.is_empty() && scanned.errors.is_empty());
    client.reboot_dirty().unwrap();
}

/// The four verbs end to end over the socket, descriptors included. Catches: the
/// process not started in the lease directory it was sent, its exit status or the
/// signal that ended it lost on the way back, and a deleted user's kill accepted.
#[test]
fn the_verbs_work_over_the_socket() {
    let served = served("verbs", Ok(()));
    let client = &served.client;
    assert_eq!(client.user_create("1.1", None).unwrap(), me());
    let (mut out, write) = std::io::pipe().unwrap();
    let null = std::fs::File::open("/dev/null").unwrap();
    let lease_dir = std::fs::File::open(&served.dir).unwrap();
    let argv = act_argv("pwd ; exit 3");
    let running = client
        .run(
            "1.1",
            &argv,
            &[],
            [null.as_fd(), write.as_fd(), null.as_fd(), lease_dir.as_fd()],
        )
        .unwrap();
    drop(write);
    assert!(running.pid > 0);
    assert_eq!(
        running.wait().unwrap(),
        Exit {
            code: Some(3),
            signal: None
        }
    );
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    let real = std::fs::canonicalize(&served.dir).unwrap();
    assert_eq!(
        text,
        format!("{}\n", real.display()),
        "not run in the lease directory"
    );

    let argv = act_argv("kill-self 9");
    let running = client
        .run(
            "1.1",
            &argv,
            &[],
            [null.as_fd(), null.as_fd(), null.as_fd(), lease_dir.as_fd()],
        )
        .unwrap();
    assert_eq!(
        running.wait().unwrap(),
        Exit {
            code: None,
            signal: Some(9)
        }
    );

    client.kill_uid("1.1").unwrap();
    assert!(client.user_delete("1.1").unwrap());
    assert!(!client.user_delete("1.1").unwrap());
    match client.kill_uid("1.1") {
        Err(ClientError::Refused(why)) => assert!(why.contains("was deleted"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Catches: the caller check removed (S10: "caller check removed"), or done after
/// the request was carried out: a refused caller changes nothing.
#[test]
fn a_refused_caller_gets_nothing_done() {
    let served = served("refused", Err("not the daemon".to_owned()));
    match served.client.user_create("2.1", None) {
        Err(ClientError::Refused(why)) => {
            assert_eq!(why, "caller refused: not the daemon");
        }
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().log.is_empty());
    assert!(served.host.state().users.is_empty());
}

/// Catches: the caller checked only after its request was read (S4.3, the
/// connect-then-exec case: a process that writes its request and then executes the
/// genuine daemon is checked as the daemon). The refusal comes before the caller has
/// written a byte, and the caller is identified once, at accept.
#[test]
fn a_caller_is_checked_before_anything_is_read() {
    let check = Scripted::fixed(Err("not the daemon".to_owned()));
    let served = served_with("checked-first", Arc::clone(&check));
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "caller refused: not the daemon".to_owned()
        }
    );
    assert!(proto::recv::<Reply>(stream.as_fd(), 0).unwrap().is_none());
    assert_eq!(check.asked.load(Ordering::SeqCst), 1);
}

/// Catches: the second identification dropped, or compared loosely: a process that
/// passed the check hands the connection to another (a child it forked) to write the
/// request, or the kernel can no longer say who is there.
#[test]
fn a_request_from_another_process_than_the_checked_one_is_refused() {
    let check = Scripted::new(
        vec![
            Ok(caller(7)),
            Ok(caller(8)),
            Ok(caller(7)),
            Err("gone".to_owned()),
        ],
        Ok(()),
    );
    let served = served_with("changed", check);
    match served.client.user_create("2.2", None) {
        Err(ClientError::Refused(why)) => assert_eq!(
            why,
            "caller refused: the process at the other end changed since the check \
             (pid 7 version 1, now pid 8 version 1)"
        ),
        other => panic!("{other:?}"),
    }
    match served.client.user_create("2.2", None) {
        Err(ClientError::Refused(why)) => assert_eq!(why, "caller refused: gone"),
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().users.is_empty());
    // The same pid with another version is another process.
    let check = Scripted::new(
        vec![
            Ok(caller(7)),
            Ok(Caller {
                version: 2,
                ..caller(7)
            }),
        ],
        Ok(()),
    );
    let served = served_with("changed-version", check);
    match served.client.user_create("2.3", None) {
        Err(ClientError::Refused(why)) => assert!(why.contains("now pid 7 version 2"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().users.is_empty());
}

/// Catches: the nonce not checked, so a request written before the helper's check (by
/// a process that then executed the genuine daemon before the helper accepted) is
/// carried out.
#[test]
fn a_request_written_before_the_hello_is_refused() {
    let served = served("early", Ok(()));
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    let call = Call {
        nonce: "00000000000000000000000000000000".to_owned(),
        request: Request::UserCreate {
            lease: "2.4".to_owned(),
            grant: None,
        },
    };
    proto::send(stream.as_fd(), &call, &[]).unwrap();
    let hello = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap().0;
    assert!(
        matches!(&hello, Reply::Hello { nonce } if nonce.len() == 32 && *nonce != call.nonce),
        "{hello:?}"
    );
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "caller refused: the request does not carry this connection's nonce".to_owned()
        }
    );
    assert!(served.host.state().users.is_empty());
}

/// Catches: a nonce that repeats, which a request written early could then carry.
#[test]
fn every_connection_gets_a_fresh_nonce() {
    let nonces: std::collections::BTreeSet<String> = (0..64).map(|_| fresh_nonce()).collect();
    assert_eq!(nonces.len(), 64);
}

/// Catches: no read timeout, under which a member of the socket's group ties up a
/// thread and its descriptors for ever by never sending its request.
#[test]
fn a_caller_that_sends_nothing_is_dropped() {
    trace();
    let dir = scratch("server-silent");
    let helper = quiet_helper(&dir);
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    let check = Scripted::fixed(Ok(()));
    connection(
        &helper_end,
        &helper,
        check.as_ref(),
        Duration::from_millis(50),
    );
    let hello = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap()
        .0;
    assert!(matches!(hello, Reply::Hello { .. }), "{hello:?}");
    let (reply, _) = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.starts_with("bad request")),
        "{reply:?}"
    );
    // A zero timeout refuses at once; it is not a wait without end.
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    connection(&helper_end, &helper, check.as_ref(), Duration::ZERO);
    proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    let (reply, _) = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "bad request: the deadline passed".to_owned()
        }
    );
}

/// Catches: a deadline looked at only before a read that needs more bytes, so that a
/// frame whose last byte arrives after the deadline is parsed and answered. The whole
/// frame is queued in one write before the helper reads; its clock reads 0 ms for the
/// start and for each of the two looks before a read (header, body), then 1000 ms, the
/// request being due at 400 ms. The reads all find their bytes at once, so only a look
/// after the frame is complete can refuse it, and it must be refused at the deadline,
/// not parsed ("bad request: expected value").
#[test]
fn a_request_completed_after_its_deadline_is_refused_at_the_deadline() {
    assert_eq!(
        reply_to_a_frame_completed_at(1000),
        Reply::Refused {
            reason: "bad request: the deadline passed".to_owned()
        }
    );
}

/// Catches: a deadline that is not yet passed when the clock reads exactly the
/// deadline (`>` where `>=` is meant). The clock reads 400 ms, the whole of the
/// 400 ms allowed, after the frame is complete.
#[test]
fn a_request_completed_exactly_at_its_deadline_is_refused() {
    assert_eq!(
        reply_to_a_frame_completed_at(400),
        Reply::Refused {
            reason: "bad request: the deadline passed".to_owned()
        }
    );
}

/// The helper's answer to a whole frame queued in one write, with a 400 ms deadline
/// and a clock that reads 0 ms until the frame is read and `after_ms` once it is.
fn reply_to_a_frame_completed_at(after_ms: u64) -> Reply {
    use std::io::Write as _;
    trace();
    let dir = scratch(&format!("server-late-frame-{after_ms}"));
    let helper = quiet_helper(&dir);
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    let check = Scripted::fixed(Ok(()));
    // Not a request: a frame that is parsed fails to parse, so an answer other than
    // the deadline's shows the frame was read as if on time.
    let mut frame = vec![0, 0, 0, 20];
    frame.extend_from_slice(&[b'x'; 20]);
    (&caller_end).write_all(&frame).unwrap();
    let looks = AtomicUsize::new(0);
    connection_on(
        &helper_end,
        &helper,
        check.as_ref(),
        Duration::from_millis(400),
        &|| {
            let at = looks.fetch_add(1, Ordering::SeqCst);
            Duration::from_millis(if at < 3 { 0 } else { after_ms })
        },
    );
    let hello = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap()
        .0;
    assert!(matches!(hello, Reply::Hello { .. }), "{hello:?}");
    proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap()
        .0
}

/// Catches: a timeout per read only, under which a caller that sends one byte at a
/// time, each within the timeout, holds a thread for as long as it likes. The caller
/// sends the 24 bytes of a frame, and each byte moves the connection's clock on by
/// 100 ms, from the helper's first reading of the clock; the request is due 400 ms
/// after that. The reads may split the bytes any way (one byte each, or all 24 at
/// once after the clock has run far past the deadline), and in every split the
/// request is refused at the deadline: either a look before a read finds it passed,
/// or the look after the frame is complete does. A per-read timeout would instead
/// read the whole frame and answer "bad request: expected value".
#[test]
fn a_caller_that_drips_its_request_is_dropped_at_the_deadline() {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    trace();
    let dir = scratch("server-drip");
    let helper = quiet_helper(&dir);
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    let check = Scripted::fixed(Ok(()));
    let clock = Arc::new(AtomicU64::new(0));
    let helper_clock = clock.clone();
    // Told after the helper's first reading of the clock, which starts the 400 ms: the
    // caller moves the clock and sends nothing before that, so the deadline cannot
    // start late.
    let (started, started_rx) = std::sync::mpsc::channel();
    let served = std::thread::spawn(move || {
        connection_on(
            &helper_end,
            &helper,
            check.as_ref(),
            Duration::from_millis(400),
            &|| {
                // Read, then tell: the caller moves the clock only after this
                // reading, so the deadline starts at 0 whatever the interleaving.
                let reading = Duration::from_millis(helper_clock.load(Ordering::SeqCst));
                let _ = started.send(());
                reading
            },
        );
    });
    let hello = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap()
        .0;
    assert!(matches!(hello, Reply::Hello { .. }), "{hello:?}");
    let mut dripper = caller_end.try_clone().unwrap();
    let drip = std::thread::spawn(move || {
        started_rx.recv().expect("the helper read the clock");
        // A frame of 20 bytes, then those bytes; the helper hangs up partway.
        for byte in [0, 0, 0, 20].into_iter().chain([b'x'; 20]) {
            clock.fetch_add(100, Ordering::SeqCst);
            if dripper.write_all(&[byte]).is_err() {
                return;
            }
        }
    });
    let (reply, _) = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.contains("deadline")),
        "{reply:?}"
    );
    served.join().unwrap();
    drop(caller_end);
    drip.join().unwrap();
}

#[test]
fn a_malformed_request_is_refused_and_an_empty_one_ignored() {
    let served = served("malformed", Ok(()));
    let socket = served.client.clone();
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    proto::send(stream.as_fd(), &Reply::Killed, &[]).unwrap();
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.starts_with("bad request")),
        "{reply:?}"
    );
    drop(UnixStream::connect(socket_path(&served)).unwrap());
    // The server is still up.
    assert_eq!(socket.user_create("3.1", None).unwrap(), me());
}

fn socket_path(served: &Served) -> PathBuf {
    served.dir.join("run/socket")
}

/// The client's side of replies it does not expect, and of a helper that hangs up.
#[test]
fn the_client_reports_unexpected_replies_and_hangups() {
    let dir = scratch("server-client");
    let path = dir.join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let answers = [
        Some(Reply::Killed),
        Some(Reply::Created { uid: 1 }),
        Some(Reply::Killed),
        Some(Reply::Killed),
        Some(Reply::Started { pid: 7 }),
        None,
        Some(Reply::Started { pid: 7 }),
        Some(Reply::Killed),
        Some(Reply::SessionReady { ready: true }),
        Some(Reply::Killed),
        // The role verbs: a reply of the wrong kind each.
        Some(Reply::Killed),
        Some(Reply::SessionReady { ready: true }),
        Some(Reply::Created { uid: 1 }),
        Some(Reply::Killed),
        Some(Reply::Started { pid: 9 }),
        Some(Reply::Exited {
            code: Some(3),
            signal: None,
        }),
    ];
    std::thread::spawn(move || {
        // First a helper that answers the connection with something other than Hello.
        let (stream, _) = listener.accept().unwrap();
        proto::send(stream.as_fd(), &Reply::Killed, &[]).unwrap();
        drop(stream);
        let mut answers = answers.into_iter();
        while let Some(answer) = answers.next() {
            let (stream, _) = listener.accept().unwrap();
            let hello = Reply::Hello {
                nonce: "n".to_owned(),
            };
            proto::send(stream.as_fd(), &hello, &[]).unwrap();
            let _ = proto::recv::<Call>(stream.as_fd(), RUN_FDS);
            if let Some(answer) = answer {
                proto::send(stream.as_fd(), &answer, &[]).unwrap();
                if matches!(answer, Reply::Started { .. })
                    && let Some(Some(then)) = answers.next()
                {
                    proto::send(stream.as_fd(), &then, &[]).unwrap();
                }
            }
        }
    });
    let client = Client::new(&path);
    let unexpected = |result: Result<(), ClientError>| match result {
        Err(error @ ClientError::Unexpected(_)) => error.to_string(),
        other => panic!("{other:?}"),
    };
    assert!(unexpected(client.kill_uid("1.1")).contains("Killed"));
    assert!(unexpected(client.user_create("1.1", None).map(drop)).contains("Killed"));
    assert!(unexpected(client.kill_uid("1.1")).contains("Created"));
    assert!(unexpected(client.user_delete("1.1").map(drop)).contains("Killed"));
    let null = std::fs::File::open("/dev/null").unwrap();
    let fds = [null.as_fd(), null.as_fd(), null.as_fd(), null.as_fd()];
    assert!(unexpected(client.run("1.1", &[], &[], fds).map(drop)).contains("Killed"));
    // Started, then the helper hangs up before the exit.
    let running = client.run("1.1", &[], &[], fds).unwrap();
    match running.wait() {
        Err(error @ ClientError::Io(_)) => assert!(error.to_string().contains("kbf-mac-session")),
        other => panic!("{other:?}"),
    }
    let running = client.run("1.1", &[], &[], fds).unwrap();
    assert!(unexpected(running.wait().map(drop)).contains("Killed"));
    assert!(client.session_ready("1.1").unwrap());
    assert!(unexpected(client.session_ready("1.1").map(drop)).contains("Killed"));
    assert!(unexpected(client.role_session_ready().map(drop)).contains("Killed"));
    assert!(unexpected(client.role_kill_uid()).contains("SessionReady"));
    assert!(unexpected(client.role_reap(7, 8, "boot")).contains("Created"));
    assert!(unexpected(client.role_run(&[], &[], fds).map(drop)).contains("Killed"));
    let running = client.role_run(&[], &[], fds).unwrap();
    assert_eq!(running.pid, 9);
    assert_eq!(running.wait().unwrap().code, Some(3));
    let refused = ClientError::Refused("why".to_owned());
    assert_eq!(refused.to_string(), "kbf-mac-session refused: why");
    let gone = Client::new(dir.join("no-socket"));
    assert!(matches!(gone.kill_uid("1.1"), Err(ClientError::Io(_))));
}

/// A failed `accept` ends serving with its error (the service manager restarts the
/// helper) rather than spinning.
#[test]
fn serve_returns_when_accept_fails() {
    let dir = scratch("server-accept");
    let listener = UnixListener::bind(dir.join("socket")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let helper = {
        let ledger = Ledger::open(&dir.join("ledger")).unwrap();
        let settings = Settings {
            range: UidRange::new(600, 601).unwrap(),
            gid: 20,
            homes: dir.to_path_buf(),
            schedules: SweepPlan {
                named: Vec::new(),
                owned: Vec::new(),
            },
            sweep: SweepPlan {
                named: Vec::new(),
                owned: Vec::new(),
            },
            grant_keys: None,
            serial: "S".to_owned(),
            session: SessionPaths::under(&dir),
            scan: crate::scan::ScanSettings::under(&dir),
            image: crate::helper::ImageSettings::under(&dir),
            vm_user: None,
        };
        Arc::new(Helper::new(Box::new(FakeHost::default()), settings, ledger))
    };
    let check: Arc<dyn CallerCheck> = Scripted::fixed(Ok(()));
    let error = serve(&listener, &helper, &check);
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

/// A caller that hangs up before the hello or the reply costs the helper nothing but a
/// log line.
#[test]
fn a_caller_that_left_is_not_an_error() {
    trace();
    let dir = scratch("server-left");
    let helper = quiet_helper(&dir);
    let check = Scripted::fixed(Ok(()));
    let (caller, helper_end) = UnixStream::pair().unwrap();
    drop(caller);
    connection(&helper_end, &helper, check.as_ref(), REQUEST_TIMEOUT);
    // Gone after its request: the reply finds no one.
    let (caller, helper_end) = UnixStream::pair().unwrap();
    let thread = std::thread::spawn(move || {
        let hello = proto::recv::<Reply>(caller.as_fd(), 0).unwrap().unwrap().0;
        let Reply::Hello { nonce } = hello else {
            panic!("{hello:?}")
        };
        let request = Request::KillUid {
            lease: "1.1".to_owned(),
        };
        proto::send(caller.as_fd(), &Call { nonce, request }, &[]).unwrap();
    });
    connection(&helper_end, &helper, check.as_ref(), REQUEST_TIMEOUT);
    thread.join().unwrap();
}

/// A helper over a fake host whose leases nothing in the test creates.
fn quiet_helper(dir: &Path) -> Helper {
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let settings = Settings {
        range: UidRange::new(600, 601).unwrap(),
        gid: 20,
        homes: dir.to_path_buf(),
        schedules: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        sweep: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        grant_keys: None,
        serial: "S".to_owned(),
        session: SessionPaths::under(dir),
        scan: crate::scan::ScanSettings::under(dir),
        image: crate::helper::ImageSettings::under(dir),
        vm_user: None,
    };
    Helper::new(Box::new(FakeHost::default()), settings, ledger)
}

#[test]
fn a_wait_that_fails_is_reported() {
    let reply = exited(Err(io::Error::other("gone")));
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "waiting for the process: gone".to_owned()
        }
    );
}

/// Catches: the refusal naming a binary the pinned requirement does not pin (it once
/// said `kbf-daemon` while the installed binary is `kbf`, run as `kbf daemon`), which
/// sends an operator hunting for a file that is not installed. Portable, so Linux
/// runs it; the macOS caller check builds its refusal through the same function.
#[test]
fn a_refused_caller_is_told_it_is_not_the_installed_kbf_daemon() {
    let text = not_the_daemon(&caller(7), "code failed the requirement");
    assert_eq!(
        text,
        "pid 7 version 1 is not the installed `kbf` daemon: code failed the requirement"
    );
    assert!(!text.contains("kbf-daemon"), "{text}");
}
