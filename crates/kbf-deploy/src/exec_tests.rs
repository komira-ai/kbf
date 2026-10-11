use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::*;
use crate::plan::{Mode, Planner, RunAs, execute};
use crate::testkit::scratch;

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn me() -> Owner {
    Owner {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

fn host(name: &str) -> (HostExec, std::path::PathBuf) {
    let dir = scratch(name);
    (HostExec::new(Root::new(&dir).expect("absolute")), dir)
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Catches the real host writing outside its root, ignoring the permissions or owner
/// a step names, or a second plan that is not empty after an apply on a real file
/// system (the fake could hide a comparison the real `inspect` gets wrong).
#[test]
fn steps_land_under_the_root_and_a_rerun_plans_nothing() {
    let (mut exec, dir) = host("apply");
    fs::write(dir.join("old"), "x").expect("seed");
    fs::create_dir(dir.join("olddir")).expect("seed");
    let plan_once = |exec: &HostExec| {
        let mut planner = Planner::new(exec);
        planner
            .dir(p("/etc"), Perms::new(0o750), Some(me()))
            .expect("plan");
        planner
            .file(
                p("/etc/a.env"),
                Perms::new(0o640),
                Some(me()),
                b"A=1\n".to_vec(),
            )
            .expect("plan");
        planner
            .file(p("/etc/b"), Perms::new(0o600), None, Vec::new())
            .expect("plan");
        planner.absent(p("/old")).expect("plan");
        planner.absent(p("/olddir")).expect("plan");
        planner.finish()
    };
    let plan = plan_once(&exec);
    assert_eq!(plan.steps().len(), 5);
    execute(&plan, RunAs::User, Mode::Apply, &mut exec, &mut Vec::new()).expect("apply");

    assert_eq!(fs::read(dir.join("etc/a.env")).expect("written"), b"A=1\n");
    assert_eq!(mode_of(&dir.join("etc")), 0o750);
    assert_eq!(mode_of(&dir.join("etc/a.env")), 0o640);
    assert_eq!(mode_of(&dir.join("etc/b")), 0o600);
    assert!(!dir.join("old").exists() && !dir.join("olddir").exists());
    let names: Vec<_> = fs::read_dir(dir.join("etc"))
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names.len(), 2, "a temporary copy was left: {names:?}");

    assert!(plan_once(&exec).is_empty(), "the re-run planned steps");
}

/// Catches a write that edits the file in place (a reader could see half of it) or
/// trips over a copy an interrupted run left behind.
#[test]
fn a_write_replaces_the_file_through_a_fresh_copy() {
    let (mut exec, dir) = host("replace");
    fs::write(dir.join("f"), "old").expect("seed");
    let held = fs::File::open(dir.join("f")).expect("open the old file");
    fs::write(dir.join(".f.kbf-deploy-tmp"), "stale").expect("stale copy");
    let step = Step::WriteFile {
        path: p("/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: b"new".to_vec(),
    };
    exec.apply(&step).expect("write");
    assert_eq!(fs::read(dir.join("f")).expect("read"), b"new");
    let mut old = String::new();
    (&held)
        .read_to_string(&mut old)
        .expect("read the held file");
    assert_eq!(old, "old", "the file was rewritten in place");
    assert!(!dir.join(".f.kbf-deploy-tmp").exists());
}

/// Catches `Mkdir` treating a file at the path as the directory (it would chmod the
/// file, and every later run would plan the directory again), and an existing
/// directory refused instead of having its permissions set.
#[test]
fn mkdir_sets_an_existing_directory_and_refuses_a_file() {
    let (mut exec, dir) = host("mkdir");
    fs::create_dir(dir.join("d")).expect("seed");
    fs::write(dir.join("f"), "").expect("seed");
    let mkdir = |path| Step::Mkdir {
        path: p(path),
        perms: Perms::new(0o700),
        owner: None,
    };
    fs::set_permissions(dir.join("f"), fs::Permissions::from_mode(0o644)).expect("chmod");
    exec.apply(&mkdir("/d")).expect("an existing directory");
    assert_eq!(mode_of(&dir.join("d")), 0o700);
    let err = exec
        .apply(&mkdir("/f"))
        .expect_err("a file is not a directory");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(mode_of(&dir.join("f")), 0o644, "the file was chmodded");
    let err = exec.apply(&mkdir("/no/parent")).expect_err("no parent");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// Catches `inspect` following a link (a link to a file would read as that file, and a
/// plan could never replace the link), and errors other than "not found" read as
/// absent.
#[test]
fn inspect_sees_links_as_other_and_reports_errors() {
    let (mut exec, dir) = host("inspect");
    fs::write(dir.join("target"), "x").expect("seed");
    std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).expect("link");
    assert_eq!(
        exec.inspect(&p("/link")).expect("inspect"),
        Some(Entry::Other)
    );
    assert_eq!(exec.inspect(&p("/none")).expect("inspect"), None);
    let err = exec
        .inspect(&p("/target/below"))
        .expect_err("a file has no children");
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    exec.apply(&Step::Remove { path: p("/link") })
        .expect("remove the link");
    assert!(
        dir.join("target").exists(),
        "removing a link removed its target"
    );
    let err = exec
        .apply(&Step::Remove { path: p("/link") })
        .expect_err("gone");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// A write whose copy cannot be made fails, and leaves nothing behind.
#[test]
fn a_write_into_a_missing_directory_fails() {
    let (mut exec, dir) = host("nodir");
    let step = Step::WriteFile {
        path: p("/no/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: Vec::new(),
    };
    let err = exec.apply(&step).expect_err("no parent directory");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(fs::read_dir(&dir).expect("list").count(), 0);
}

/// Catches a stale copy the run cannot remove being ignored (a directory there would
/// make every write fail later, with a less clear error).
#[test]
fn a_stale_copy_that_cannot_be_removed_fails_the_write() {
    let (mut exec, dir) = host("stuck");
    fs::create_dir(dir.join(".f.kbf-deploy-tmp")).expect("a directory where the copy goes");
    let step = Step::WriteFile {
        path: p("/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: Vec::new(),
    };
    exec.apply(&step)
        .expect_err("the copy's path is a directory");
    assert!(!dir.join("f").exists());
}

/// Catches a command that inherits the caller's environment (a secret or a `PATH` the
/// kit never chose would reach it) or loses the variables the step names, and a
/// failing command read as a success. `printenv NAME` exits 1 when NAME is unset.
#[test]
fn a_command_gets_only_the_environment_it_names() {
    let (mut exec, _dir) = host("run");
    let printenv = || Cmd::new(p("/usr/bin/printenv"));
    let inherited = Step::Run(printenv().arg("HOME"));
    let err = exec
        .apply(&inherited)
        .expect_err("HOME must not be inherited");
    let msg = err.to_string();
    assert!(msg.starts_with("/usr/bin/printenv exited with "), "{msg}");
    exec.apply(&Step::Run(printenv().arg("KBF_X").env("KBF_X", "1")))
        .expect("the named variable is set");
    let missing = Step::Run(Cmd::new(p("/nonexistent/tool")));
    let err = exec.apply(&missing).expect_err("no such program");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// Answers each connection with the next status in `statuses`, then closes.
/// The number of answers sent so far is counted before each is written, so a client
/// that has read an answer sees it counted.
fn server(statuses: Vec<&'static str>) -> (u16, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let answered = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&answered);
    let handle = thread::spawn(move || {
        for status in statuses {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                reader.read_line(&mut request).expect("read the request");
            }
            let want = format!(
                "GET /readyz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            assert_eq!(request, want);
            count.fetch_add(1, Ordering::SeqCst);
            conn.write_all(status.as_bytes()).expect("answer");
        }
    });
    (port, answered, handle)
}

fn wait(exec: &mut HostExec, url: &str, deadline: Duration) -> io::Result<()> {
    let url = HttpUrl::new(url).expect("url");
    exec.apply(&Step::WaitHttp { url, deadline })
}

/// Catches a wait that gives up at the first non-2xx answer (a service still starting
/// answers 503) or takes a non-2xx answer as ready.
#[test]
fn a_wait_retries_until_the_url_answers_2xx() {
    let (mut exec, _dir) = host("wait_ok");
    let (port, answered, server) = server(vec![
        "HTTP/1.1 503 Service Unavailable\r\n\r\n",
        "garbage\r\n",
        "HTTP/1.1 204 No Content\r\n\r\n",
    ]);
    wait(
        &mut exec,
        &format!("http://127.0.0.1:{port}/readyz"),
        Duration::from_secs(10),
    )
    .expect("ready on the third answer");
    // Checked before joining: a wait that stopped early leaves the server in `accept`.
    assert_eq!(
        answered.load(Ordering::SeqCst),
        3,
        "ready before the 2xx answer"
    );
    server.join().expect("the server saw three requests");
}

/// Catches a wait that never ends, or ends without an error, when the URL never
/// answers 2xx; the error names the last answer.
#[test]
fn a_wait_fails_at_its_deadline() {
    let (mut exec, _dir) = host("wait_late");
    let (port, answered, server) = server(vec!["HTTP/1.1 500 Oops\r\n\r\n"]);
    let url = format!("http://127.0.0.1:{port}/readyz");
    let err = wait(&mut exec, &url, Duration::ZERO).expect_err("never ready");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(err.to_string().ends_with("last: status 500"), "{err}");
    assert_eq!(answered.load(Ordering::SeqCst), 1);
    server.join().expect("one request");

    // Nothing listens on the port now: each attempt fails to connect.
    let start = Instant::now();
    let err = wait(&mut exec, &url, Duration::from_millis(500)).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() >= Duration::from_millis(500));
    let err = wait(&mut exec, "http://[::1]:1/readyz", Duration::ZERO).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    let err = wait(&mut exec, "http://host.invalid/", Duration::ZERO).expect_err("no name");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

/// The real binary check refuses the user-owned test binary, through the trait.
#[test]
fn the_host_checks_the_running_binary() {
    let (exec, _dir) = host("binary");
    assert!(exec.check_binary().is_err());
}
