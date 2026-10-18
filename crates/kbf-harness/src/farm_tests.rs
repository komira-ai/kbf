//! The supervisor, over plain processes (`sleep`, `tail -f`, `true`) in place of `kbf`:
//! what it does with a kill, an exit nobody asked for, a hang and a restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

/// A fresh directory for one test.
fn dir(name: &str) -> PathBuf {
    crate::scratch(&format!("farm-{name}"))
}

fn spec(role: Role, program: &str, argv: &[&str], cwd: &Path) -> (Role, Spec) {
    let spec = Spec {
        name: role.to_string(),
        program: PathBuf::from(program),
        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        env: Vec::new(),
        cwd: cwd.to_owned(),
    };
    (role, spec)
}

/// A process that runs until it is ended.
fn sleeper(role: Role, cwd: &Path) -> (Role, Spec) {
    spec(role, "sleep", &["300"], cwd)
}

/// A process that prints `marker`'s text and runs until it is ended.
fn chatty(role: Role, marker: &Path, cwd: &Path) -> (Role, Spec) {
    spec(role, "tail", &["-f", &marker.display().to_string()], cwd)
}

/// The command line of process `pid`, as the kernel has it once the process is `program` (a
/// process that has just been forked has its parent's, and one that is exec'ing has none, for
/// a moment); a bounded wait.
fn cmdline(pid: u32, program: &str) -> Vec<String> {
    let until = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).expect("a running process");
        let words: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect();
        if words.first().map(String::as_str) == Some(program) || std::time::Instant::now() >= until
        {
            return words;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until process `pid` has become `program` (an `env` that execs it has set what it
/// sets by then); a bounded wait.
fn wait_until_running(pid: u32, program: &str) {
    let until = std::time::Instant::now() + Duration::from_secs(30);
    while std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .is_none_or(|c| c.trim() != program)
    {
        assert!(std::time::Instant::now() < until, "{program} never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn restart_replays_the_same_unit_line() {
    // Catches: a restart that runs another command (a rebuilt argv with a flag dropped, or
    // the original lost), so a role the farm restarts is not the role it killed; and a
    // process table that forgets the first start (the stand-in for a boot counter).
    let d = dir("restart");
    let role = Role::Node(1);
    let mut farm = Farm::supervise(&d, vec![sleeper(role, &d)]);
    let first = farm.process_table();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].ended, None);
    let wanted = farm.spec(role).clone();
    farm.kill(role);
    farm.restart(role);
    let table = farm.process_table();
    assert_eq!(table.len(), 2, "{table:?}");
    assert_eq!((table[0].generation, table[1].generation), (0, 1));
    assert_eq!(table[0].pid, first[0].pid);
    assert_ne!(table[1].pid, table[0].pid);
    assert_eq!(table[0].ended.as_deref(), Some("signal 9"));
    assert_eq!(table[1].ended, None);
    assert_eq!(farm.spec(role).argv, wanted.argv);
    let mut line = vec![wanted.program.display().to_string()];
    line.extend(wanted.argv.clone());
    assert_eq!(cmdline(table[1].pid, "sleep"), line);
}

#[test]
fn a_restart_keeps_the_output_of_the_process_before() {
    // Catches: the new process truncating the log of the one that died, which is the
    // evidence of why it died.
    let d = dir("restart-logs");
    let marker = d.join("marker");
    std::fs::write(&marker, "first-start\n").expect("marker");
    let role = Role::Daemon(1);
    let mut farm = Farm::supervise(&d, vec![chatty(role, &marker, &d)]);
    farm.wait("the first start's output", |f| {
        f.logs().contains("first-start").then_some(())
    });
    farm.kill(role);
    std::fs::write(&marker, "second-start\n").expect("marker");
    farm.restart(role);
    farm.wait("the second start's output", |f| {
        f.logs().contains("second-start").then_some(())
    });
    assert!(d.join("daemon-1.out").exists());
    assert!(d.join("daemon-1.1.out").exists());
    assert!(
        std::fs::read_to_string(d.join("daemon-1.out"))
            .expect("log")
            .contains("first-start")
    );
}

#[test]
#[should_panic(expected = "still runs")]
fn a_role_that_runs_cannot_be_restarted() {
    // Catches: two processes of one role (two daemons with one node id, one port twice).
    let d = dir("restart-running");
    let mut farm = Farm::supervise(&d, vec![sleeper(Role::Node(1), &d)]);
    farm.restart(Role::Node(1));
}

#[test]
fn a_kill_is_not_taken_for_a_crash() {
    // Catches: a killed role failing `wait` (the kill tests could never wait again), and
    // the other half: an exit nobody asked for being taken for a kill.
    let d = dir("kill");
    let mut farm = Farm::supervise(
        &d,
        vec![sleeper(Role::Node(1), &d), sleeper(Role::Node(2), &d)],
    )
    .with_hang(Duration::from_secs(30));
    farm.kill(Role::Node(2));
    assert_eq!(
        farm.try_wait("anything", |_| Some(7)).expect("not a crash"),
        7
    );
    assert_eq!(farm.unexpected_exits(), Vec::new());
    let table = farm.process_table();
    assert_eq!(table[1].ended.as_deref(), Some("signal 9"));
}

#[test]
fn a_process_exit_while_waiting_fails_at_once_with_the_logs() {
    // Catches: waiting out the hang on a farm that is already dead (the error would say
    // "timed out" after the hang), and a failure without the output of the processes.
    let d = dir("exit");
    let marker = d.join("marker");
    std::fs::write(&marker, "still-chatting\n").expect("marker");
    let mut farm = Farm::supervise(
        &d,
        vec![
            chatty(Role::Node(1), &marker, &d),
            spec(Role::Daemon(1), "true", &[], &d),
        ],
    )
    // Only a guard against a hang of this test: the failure must come before it.
    .with_hang(Duration::from_secs(120));
    let err = farm
        .try_wait("a state that never comes", |_| None::<()>)
        .expect_err("an exit");
    assert!(err.what.starts_with("daemon-1 exited"), "{}", err.what);
    assert!(
        err.what.contains("a state that never comes"),
        "{}",
        err.what
    );
    assert!(err.report.contains("--- processes ---"), "{}", err.report);
    assert!(err.report.contains("node-1 pid"), "{}", err.report);
    assert_eq!(farm.unexpected_exits().len(), 1);
}

#[test]
fn a_hang_reports_the_process_list_and_the_logs() {
    // Catches: a timeout that says nothing of what the processes were doing, which is all a
    // hang on a hosted runner leaves to read.
    let d = dir("hang");
    let marker = d.join("marker");
    std::fs::write(&marker, "marker-of-node-1\n").expect("marker");
    let mut farm = Farm::supervise(&d, vec![chatty(Role::Node(1), &marker, &d)]);
    farm.wait("the log", |f| {
        f.logs().contains("marker-of-node-1").then_some(())
    });
    let mut farm = farm.with_hang(Duration::from_millis(500));
    let err = farm
        .try_wait("a state that never comes", |_| None::<()>)
        .expect_err("a hang");
    assert_eq!(err.what, "timed out waiting for a state that never comes");
    assert!(
        err.report.contains("--- processes ---\nnode-1 pid "),
        "{}",
        err.report
    );
    assert!(err.report.contains("marker-of-node-1"), "{}", err.report);
}

#[test]
fn stop_all_ends_the_last_started_first_and_says_how() {
    // Catches: a stop that skips a role, stops them in start order (the frontend waits
    // for open streams, so it goes last), or reports a role's end as something it was not.
    let d = dir("stop");
    let mut farm = Farm::supervise(
        &d,
        vec![
            sleeper(Role::Frontend, &d),
            sleeper(Role::Node(1), &d),
            sleeper(Role::Daemon(1), &d),
        ],
    );
    farm.kill(Role::Node(1));
    let ended = farm.stop_all();
    // `sleep` dies of the SIGTERM; the killed role is not stopped twice.
    assert_eq!(
        ended,
        vec![
            (Role::Daemon(1), "signal 15".to_owned()),
            (Role::Frontend, "signal 15".to_owned())
        ]
    );
}

#[test]
fn a_role_that_ended_before_its_turn_is_reported_with_how() {
    // Catches: a stop that skips a role which had already exited on its own and so drops
    // its status: a node that crashed (exit 1) or left (exit 0) before the stop reached it
    // would then be in no `clean_stop` evidence, and a crash would pass the gate.
    let d = dir("ended-before");
    let mut farm = Farm::supervise(
        &d,
        vec![
            spec(Role::Node(1), "false", &[], &d),
            spec(Role::Node(2), "true", &[], &d),
            sleeper(Role::Daemon(1), &d),
        ],
    );
    wait_all_ended_but(&mut farm, Role::Daemon(1));
    let ended = farm.stop_all();
    assert_eq!(
        ended,
        vec![
            (Role::Daemon(1), "signal 15".to_owned()),
            (Role::Node(2), "exited before the stop: exit 0".to_owned()),
            (Role::Node(1), "exited before the stop: exit 1".to_owned()),
        ]
    );
}

/// Waits until every process but `except`'s has ended.
fn wait_all_ended_but(farm: &mut Farm, except: Role) {
    let until = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let table = farm.process_table();
        if table
            .iter()
            .filter(|p| p.role != except.to_string())
            .all(|p| p.ended.is_some())
        {
            return;
        }
        assert!(std::time::Instant::now() < until, "a process never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_process_that_ignores_sigterm_is_killed_and_reported() {
    // Catches: a stop that waits for ever on a process that will not drain, or kills it and
    // says it exited cleanly (the `clean_stop` gate reads this text).
    let d = dir("ignores-term");
    let stubborn = spec(
        Role::Daemon(1),
        "env",
        &["--ignore-signal=TERM", "sleep", "300"],
        &d,
    );
    let mut farm = Farm::supervise(&d, vec![stubborn]).with_hang(Duration::from_millis(300));
    // `env` sets the signal to be ignored and then becomes `sleep`.
    wait_until_running(farm.process_table()[0].pid, "sleep");
    let ended = farm.stop_all();
    assert_eq!(
        ended,
        vec![(
            Role::Daemon(1),
            "still running after SIGTERM: killed".to_owned()
        )]
    );
    assert_eq!(farm.process_table()[0].ended.as_deref(), Some("signal 9"));
}

#[test]
fn a_process_that_ends_with_a_code_is_reported_with_it() {
    // Catches: an exit code read as a signal or not at all (`exit 1` is what the
    // `clean_stop` gate calls unclean).
    let d = dir("code");
    let mut farm = Farm::supervise(&d, vec![spec(Role::Node(1), "false", &[], &d)]);
    let table = loop {
        let table = farm.process_table();
        if table[0].ended.is_some() {
            break table;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(table[0].ended.as_deref(), Some("exit 1"));
}

#[test]
fn the_process_list_names_every_role() {
    let d = dir("roles");
    let farm = Farm::supervise(
        &d,
        vec![
            sleeper(Role::Frontend, &d),
            sleeper(Role::Envoy, &d),
            sleeper(Role::Daemon(2), &d),
        ],
    );
    assert_eq!(farm.roles(), [Role::Frontend, Role::Envoy, Role::Daemon(2)]);
    let names: Vec<String> = farm.roles().iter().map(ToString::to_string).collect();
    assert_eq!(names, ["frontend", "envoy", "daemon-2"]);
    assert_eq!(node_id(3), "node-3");
}

#[test]
fn log_files_are_numbered_by_generation() {
    // Catches: the files of generation 1 named as generation 0's (the restart would
    // overwrite the evidence).
    let (out, err) = crate::procs::log_files(Path::new("/l"), "node-1", 0);
    assert_eq!(
        (out, err),
        (
            PathBuf::from("/l/node-1.out"),
            PathBuf::from("/l/node-1.err")
        )
    );
    let (out, _) = crate::procs::log_files(Path::new("/l"), "node-1", 2);
    assert_eq!(out, PathBuf::from("/l/node-1.2.out"));
}

#[test]
fn a_start_line_field_is_read_by_key() {
    let line = "kbf-frontend 0.1.0 reapi=127.0.0.1:80 worker=127.0.0.1:81 api=127.0.0.1:82";
    assert_eq!(field(line, "reapi="), "127.0.0.1:80");
    assert_eq!(field(line, "api="), "127.0.0.1:82");
}

#[test]
#[should_panic(expected = "no storage= in")]
fn a_missing_start_line_field_panics() {
    let _ = field("kbf-frontend 0.1.0 reapi=127.0.0.1:80", "storage=");
}

#[test]
#[should_panic(expected = "spawn /nonexistent/kbf")]
fn a_program_that_cannot_be_started_panics_naming_it() {
    // Catches: a farm that goes on without a role whose program is not there (the rest of the
    // run would then fail with no word of the cause).
    let d = dir("no-program");
    let _ = Farm::supervise(&d, vec![spec(Role::Frontend, "/nonexistent/kbf", &[], &d)]);
}

#[test]
#[should_panic(expected = "the farm has no frontend")]
fn a_role_the_farm_does_not_have_panics_naming_it() {
    let d = dir("no-role");
    let farm = Farm::supervise(&d, vec![sleeper(Role::Node(1), &d)]);
    let _ = farm.spec(Role::Frontend);
}

#[test]
#[should_panic(expected = "timed out waiting for the impossible")]
fn wait_panics_with_the_error_of_try_wait() {
    // Catches: a `wait` that returns without what it waited for.
    let d = dir("wait-panics");
    let mut farm =
        Farm::supervise(&d, vec![sleeper(Role::Node(1), &d)]).with_hang(Duration::from_millis(100));
    farm.wait("the impossible", |_| None::<()>);
}

#[test]
fn term_returns_how_the_role_ended_and_does_not_count_as_a_crash() {
    // Catches: a role stopped on purpose being reported as a crash, and the exit status lost.
    let d = dir("term");
    let mut farm = Farm::supervise(
        &d,
        vec![sleeper(Role::Node(1), &d), sleeper(Role::Node(2), &d)],
    );
    let status = farm.term(Role::Node(1)).expect("it drained");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(15)
    );
    assert_eq!(farm.unexpected_exits(), Vec::new());
    assert_eq!(farm.process_table()[0].ended.as_deref(), Some("signal 15"));
}

#[test]
fn term_kills_what_ignores_sigterm_and_says_nothing_of_its_status() {
    // Catches: a `term` that waits for ever on a process that will not drain, or leaves it running.
    let d = dir("term-stubborn");
    let stubborn = spec(
        Role::Daemon(1),
        "env",
        &["--ignore-signal=TERM", "sleep", "300"],
        &d,
    );
    let mut farm = Farm::supervise(&d, vec![stubborn]).with_hang(Duration::from_millis(300));
    wait_until_running(farm.process_table()[0].pid, "sleep");
    assert_eq!(farm.term(Role::Daemon(1)), None);
    assert_eq!(farm.process_table()[0].ended.as_deref(), Some("signal 9"));
}

/// A stand-in operator API on a free port: it answers each request with the body listed
/// for its path (and 404 for another), until the test ends.
fn serve(routes: Vec<(&'static str, &'static str)>) -> u16 {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let (code, body) = routes
                .iter()
                .find(|(p, _)| *p == path)
                .map_or((404, ""), |(_, b)| (200, *b));
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

const NODES_BOTH: &str =
    r#"{"nodes":[{"node_id":"node-1","connected":true},{"node_id":"node-2","connected":true}]}"#;
const NODES_ONE: &str =
    r#"{"nodes":[{"node_id":"node-1","connected":true},{"node_id":"node-2","connected":false}]}"#;
const MEMBERS_ACTIVE: &str = r#"{"members":[{"state":"active"},{"state":"active"}]}"#;
const MEMBERS_CATCHING_UP: &str = r#"{"members":[{"state":"active"},{"state":"catching_up"}]}"#;

/// A farm of one sleeper whose frontend API is `routes`, with two storage nodes expected.
fn farm_with_api(name: &str, routes: Vec<(&'static str, &'static str)>) -> Farm {
    let d = dir(name);
    let mut farm = Farm::supervise(&d, vec![sleeper(Role::Frontend, &d)]);
    farm.api = serve(routes);
    farm.storage_nodes = 2;
    farm.node_ids = vec!["node-1".to_owned(), "node-2".to_owned()];
    farm
}

#[test]
fn a_farm_is_whole_when_every_node_is_connected_and_every_member_active() {
    // Catches: a check that fails a healthy farm (the end-of-load gate would be red on
    // every run).
    let farm = farm_with_api(
        "whole",
        vec![("/v1/nodes", NODES_BOTH), ("/v1/members", MEMBERS_ACTIVE)],
    );
    assert_eq!(farm.whole(), Ok(()));
}

#[test]
fn a_farm_that_lost_a_node_a_member_or_its_frontend_is_not_whole() {
    // Catches: a node that is still running but no longer connected, a member that has
    // fallen back to catching up, and a frontend that does not answer, each passing as a
    // whole farm.
    let lost_node = farm_with_api(
        "not-whole-node",
        vec![("/v1/nodes", NODES_ONE), ("/v1/members", MEMBERS_ACTIVE)],
    );
    let why = lost_node.whole().expect_err("a node is not connected");
    assert!(why.starts_with("nodes connected: [\"node-1\"]"), "{why}");
    let catching_up = farm_with_api(
        "not-whole-member",
        vec![
            ("/v1/nodes", NODES_BOTH),
            ("/v1/members", MEMBERS_CATCHING_UP),
        ],
    );
    let why = catching_up.whole().expect_err("a member is not active");
    assert!(why.starts_with("storage members"), "{why}");
    let no_members = farm_with_api("not-whole-nomembers", vec![("/v1/nodes", NODES_BOTH)]);
    let why = no_members.whole().expect_err("no /v1/members");
    assert!(why.contains("/v1/members does not answer"), "{why}");
    let silent = farm_with_api("not-whole-silent", Vec::new());
    let why = silent.whole().expect_err("no /v1/nodes");
    assert!(why.contains("/v1/nodes does not answer"), "{why}");
}

#[test]
fn the_control_socket_is_removed_when_the_farm_stops_and_when_it_is_dropped() {
    // Catches: the frontend's control socket left in the temporary directory by every run
    // (a killed process cannot remove its own), one more per run on a long-lived host.
    // Mutant: `stop_all_within` or `Drop for Farm` does not remove it.
    let d = dir("socket");
    let socket = d.join("kbf-farm.sock");
    std::fs::write(&socket, b"").expect("socket");
    let mut farm = Farm::supervise(&d, vec![sleeper(Role::Frontend, &d)]);
    farm.socket = Some(socket.clone());
    let _ = farm.stop_all();
    assert!(!socket.exists(), "removed by the stop");
    std::fs::write(&socket, b"").expect("socket");
    let mut farm = Farm::supervise(&d, vec![sleeper(Role::Frontend, &d)]);
    farm.socket = Some(socket.clone());
    drop(farm);
    assert!(!socket.exists(), "removed by the drop");
}

#[test]
fn a_farm_that_is_dropped_leaves_no_process_running() {
    // Catches: a run that panics or returns early (the farm goes out of scope with no stop)
    // leaving its processes running. Mutant: `Proc` has no `Drop`.
    let d = dir("dropped");
    let farm = Farm::supervise(
        &d,
        vec![sleeper(Role::Node(1), &d), sleeper(Role::Node(2), &d)],
    );
    let pids: Vec<u32> = farm
        .process_list()
        .lines()
        .filter_map(|l| l.split(" pid ").nth(1)?.split(' ').next()?.parse().ok())
        .collect();
    assert_eq!(pids.len(), 2);
    assert!(pids.iter().all(|p| crate::alive(*p)));
    drop(farm);
    for pid in pids {
        assert!(!crate::alive(pid), "{pid} still runs");
    }
}

#[test]
fn a_stop_is_bounded_as_a_whole_not_per_role() {
    // Catches: a stop that gives every role the farm's whole wait in turn (sixty seconds
    // each, so minutes for a farm of seven that will not drain), and with it a run that
    // outlasts its job's timeout while "stopping". Three processes that ignore SIGTERM are
    // stopped within a budget of nothing: each is killed at once. The farm's own wait is an
    // hour, so a stop that ignored the budget would not return in the minute the thread is
    // given (a liveness guard, not a measure of time). Mutant: `stop_all_within` waits
    // `hang` for each role.
    let d = dir("stop-bound");
    let stubborn = |role| spec(role, "env", &["--ignore-signal=TERM", "sleep", "300"], &d);
    let roles = [Role::Node(1), Role::Node(2), Role::Node(3)];
    let mut farm = Farm::supervise(&d, roles.iter().map(|r| stubborn(*r)).collect())
        .with_hang(Duration::from_secs(3600));
    for pid in farm.process_table().iter().map(|p| p.pid) {
        wait_until_running(pid, "sleep");
    }
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ended = farm.stop_all_within(Duration::ZERO);
        let _ = done.send(ended);
    });
    let ended = result
        .recv_timeout(Duration::from_secs(60))
        .expect("the stop returned: its budget bounds it");
    assert_eq!(ended.len(), 3);
    assert!(
        ended
            .iter()
            .all(|(_, how)| how == "still running after SIGTERM: killed"),
        "{ended:?}"
    );
}

#[test]
fn a_wait_ends_at_once_when_the_run_is_asked_to_stop() {
    // Catches: a run that has been sent SIGTERM going on waiting for a farm to become ready
    // (up to the farm's whole wait) before it notices. Mutant: `try_wait` does not look at
    // the flag.
    let d = dir("interrupted");
    let mut farm =
        Farm::supervise(&d, vec![sleeper(Role::Node(1), &d)]).with_hang(Duration::from_secs(3600));
    farm.interrupt.set();
    let error = farm
        .try_wait("never", |_| None::<()>)
        .expect_err("a wait that is interrupted fails");
    assert!(
        error
            .what
            .starts_with("interrupted by a signal while waiting for never"),
        "{}",
        error.what
    );
    assert!(error.report.contains("--- processes ---"));
}

#[test]
#[should_panic(expected = "podman ran on")]
fn a_stop_refuses_a_farm_on_which_podman_ran() {
    // Catches: kbf starting `podman` (the container driver is crun only: the `podman` first on
    // a node's `PATH` fails and records that it ran) without anyone noticing. Mutant:
    // `Farm::stop` does not look.
    let d = dir("podman-ran");
    std::fs::create_dir_all(d.join("bin")).expect("bin");
    std::fs::write(d.join("bin/podman-ran"), "podman pull\n").expect("record");
    let farm = Farm::supervise(&d, vec![sleeper(Role::Node(1), &d)]);
    farm.stop();
}
