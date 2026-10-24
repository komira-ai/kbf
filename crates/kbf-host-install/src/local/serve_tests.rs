//! The socket: who may connect, how a line is read, what is audited, and the loop that
//! answers and ticks.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::answer::{Answer, Code};
use super::backend::Fault;
use super::core_tests::{A, B, Fx, VA, VB};
use super::request::MAX_LINE_LEN;
use super::serve::{ServeConfig, Serving, bind, passwd_ids, read_line};
use crate::trust::Trust;

fn trust(dir: &Path) -> Trust {
    Trust::owned_by(fs::metadata(dir).unwrap().uid(), dir)
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The accounts of a passwd file are found by name; a name that is not there, a line that is
/// not a passwd line and ids that are not numbers give none.
#[test]
fn the_ids_of_an_account_come_from_the_passwd_text() {
    let passwd = "root:x:0:0:root:/root:/bin/sh\nkbf-updater:x:991:990::/var/empty:/usr/sbin/nologin\n\
                  broken:x:abc:1::/:/bin/false\nshort:x\n";
    assert_eq!(passwd_ids(passwd, "kbf-updater"), Some((991, 990)));
    assert_eq!(passwd_ids(passwd, "root"), Some((0, 0)));
    assert_eq!(passwd_ids(passwd, "broken"), None);
    assert_eq!(passwd_ids(passwd, "short"), None);
    assert_eq!(passwd_ids(passwd, "nobody"), None);
    assert_eq!(passwd_ids("", "root"), None);
}

/// The flags: the socket and its account are required, the window defaults to ten minutes and
/// is held to 120..1800, units are kbf units and may be repeated, `--flag=value` is the same
/// as `--flag value`, and nothing is read from anywhere else.
#[test]
fn the_flags_make_one_config_and_refuse_the_rest() {
    let config = ServeConfig::parse(&args(&[
        "--socket",
        "/run/kbf/host.sock",
        "--socket-user=kbf-updater",
        "--window-secs",
        "300",
        "--restart-unit",
        "kbf-daemon.service",
        "--restart-unit=kbf-frontend@a.service",
    ]))
    .unwrap();
    assert_eq!(config.socket, PathBuf::from("/run/kbf/host.sock"));
    assert_eq!(config.user, "kbf-updater");
    assert_eq!(config.window_secs, 300);
    let units: Vec<_> = config.units.iter().map(|u| u.as_str()).collect();
    assert_eq!(units, ["kbf-daemon.service", "kbf-frontend@a.service"]);

    let minimal = ServeConfig::parse(&args(&["--socket", "/s", "--socket-user", "u"])).unwrap();
    assert_eq!((minimal.window_secs, minimal.units.len()), (600, 0));
    assert!(!minimal.frontend_container);

    let bad: [(&[&str], &str); 10] = [
        (&["--socket-user", "u"], "--socket is required"),
        (&["--socket", "/s"], "--socket-user is required"),
        (&["--socket", "s", "--socket-user", "u"], "absolute"),
        (
            &["--socket", "/s", "--socket", "/t", "--socket-user", "u"],
            "given twice",
        ),
        (
            &[
                "--socket",
                "/s",
                "--socket-user",
                "u",
                "--window-secs",
                "119",
            ],
            "120 to 1800",
        ),
        (
            &[
                "--socket",
                "/s",
                "--socket-user",
                "u",
                "--window-secs",
                "1801",
            ],
            "120 to 1800",
        ),
        (
            &[
                "--socket",
                "/s",
                "--socket-user",
                "u",
                "--window-secs",
                "soon",
            ],
            "120 to 1800",
        ),
        (
            &[
                "--socket",
                "/s",
                "--socket-user",
                "u",
                "--restart-unit",
                "sshd.service",
            ],
            "not a kbf unit",
        ),
        (
            &["--socket", "/s", "--socket-user", "u", "--bind", "x"],
            "unknown argument",
        ),
        (&["--socket"], "needs a value"),
    ];
    for (words, why) in bad {
        let err = ServeConfig::parse(&args(words)).unwrap_err();
        assert!(err.contains(why), "{words:?}: {err}");
    }
}

/// The socket is the access control: mode 0600, owned by the account asked for, in a
/// directory that is the trusted user's and closed to others. A stale socket is replaced; a
/// file that is not a socket, or a directory others can write, is not.
#[test]
fn the_socket_is_0600_and_binds_only_where_it_is_safe() {
    let dir = TestDir::under(std::env::temp_dir(), "sv-bind");
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let meta = fs::metadata(&*dir).unwrap();
    let ids = (meta.uid(), meta.gid());
    let run = dir.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = run.join("s");

    let first = bind(&trust(&dir), &socket, ids).unwrap();
    let made = fs::metadata(&socket).unwrap();
    assert!(made.file_type().is_socket());
    assert_eq!(made.mode() & 0o7777, 0o600);
    assert_eq!((made.uid(), made.gid()), ids);
    drop(first);
    // The file stays after the listener is gone; the next start replaces it.
    assert!(socket.exists());
    let second = bind(&trust(&dir), &socket, ids).unwrap();
    assert!(UnixStream::connect(&socket).is_ok());
    drop(second);

    fs::remove_file(&socket).unwrap();
    fs::write(&socket, "not a socket").unwrap();
    let err = bind(&trust(&dir), &socket, ids).unwrap_err();
    assert!(err.to_string().contains("not a socket"), "{err}");
    assert_eq!(fs::read_to_string(&socket).unwrap(), "not a socket");

    fs::remove_file(&socket).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(bind(&trust(&dir), &socket, ids).is_err());
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();

    // A directory that cannot be looked into: the socket's own file cannot be looked at.
    if meta.uid() != 0 {
        fs::set_permissions(&run, fs::Permissions::from_mode(0o600)).unwrap();
        let err = bind(&trust(&dir), &run.join("blind"), ids).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // A path with no directory, a directory that is not there, and a socket path whose
    // account cannot be given it.
    assert!(bind(&trust(&dir), Path::new("/"), ids).is_err());
    assert!(bind(&trust(&dir), &run.join("gone/s"), ids).is_err());
    // An account the file cannot be given to (a process that is not root cannot give files
    // away).
    if meta.uid() != 0 {
        let stranger = (ids.0.wrapping_add(1), ids.1);
        assert!(bind(&trust(&dir), &run.join("t"), stranger).is_err());
    }
}

/// Serve one request through a pair of sockets and read what came back.
struct Wire<'a> {
    serving: &'a Serving<'a, super::core_tests::Fake>,
}

impl Wire<'_> {
    fn send(&self, bytes: &[u8]) -> String {
        let (client, server) = UnixStream::pair().unwrap();
        (&client).write_all(bytes).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        self.serving.serve_one(server);
        let mut text = String::new();
        // A client that sent more than a line is reset after the answer; the answer is read.
        let _ = (&client).read_to_string(&mut text);
        text
    }

    fn ask(&self, line: &str) -> Value {
        let text = self.send(format!("{line}\n").as_bytes());
        assert!(
            text.ends_with('\n') && text.matches('\n').count() == 1,
            "{text:?}"
        );
        serde_json::from_str(&text).unwrap()
    }
}

fn audit_lines(fx: &Fx) -> Vec<Value> {
    fs::read_to_string(fx.dir.join("audit.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Run `body` with a [`Serving`] over a fresh fake host; the log lines it wrote come back.
fn with_serving(name: &str, body: impl FnOnce(&Fx, &Wire<'_>)) -> Vec<String> {
    let fx = Fx::new(name);
    let lines = RefCell::new(Vec::new());
    let log = |line: &str| lines.borrow_mut().push(line.to_owned());
    let audit = fx.dir.join("audit.log");
    let trust = trust(&fx.dir);
    let serving = Serving {
        ab: &fx.ab,
        trust: &trust,
        audit: &audit,
        log: &log,
    };
    body(&fx, &Wire { serving: &serving });
    lines.into_inner()
}

/// A whole update over the socket: each request is one line in and one line out, and every
/// request but the query leaves an audit line with the verb, the version, the channel, the
/// outcome and the reason; the query leaves none.
#[test]
fn a_request_is_one_line_in_and_one_line_out_and_is_audited() {
    with_serving("sv-audit", |fx, wire| {
        assert_eq!(wire.ask("query")["result"], "query");
        assert_eq!(
            wire.ask(&format!("install kbf={VB} from stable"))["result"],
            "staged"
        );
        assert_eq!(wire.ask(&format!("switch kbf={VB}"))["result"], "switched");
        assert_eq!(
            wire.ask(&format!("confirm kbf={VB}"))["result"],
            "confirmed"
        );
        assert_eq!(
            wire.ask("revert"),
            json!({"result":"refused","reason":"nothing_pending"})
        );
        let lines = audit_lines(fx);
        let fields: Vec<(String, String, String)> = lines
            .iter()
            .map(|l| {
                (
                    l["verb"].as_str().unwrap().to_owned(),
                    l["outcome"].as_str().unwrap().to_owned(),
                    l["reason"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            fields,
            [
                ("stage".into(), "staged".into(), "".into()),
                ("switch".into(), "switched".into(), "".into()),
                ("confirm".into(), "confirmed".into(), "".into()),
                ("revert".into(), "refused".into(), "nothing_pending".into()),
            ]
        );
        assert_eq!(lines[0]["version"], VB);
        assert_eq!(lines[0]["channel"], "stable");
        assert_eq!(lines[1]["version"], VB);
        assert_eq!(lines[3]["version"], Value::Null);
    });
}

/// A line that is not a request is refused and recorded, escaped and cut, as the ssh path
/// records one: too long, with no end, not text, empty, or simply another verb.
#[test]
fn a_line_that_is_not_a_request_is_refused_and_recorded_cut() {
    let logged = with_serving("sv-bad", |fx, wire| {
        let refused = json!({"result":"refused","reason":"bad_command"});
        assert_eq!(wire.ask("rm -rf /"), refused);
        assert_eq!(wire.ask(&"y".repeat(300)), refused);
        assert_eq!(wire.ask(&"x".repeat(MAX_LINE_LEN * 2)), refused);
        for raw in [&b"query"[..], b"\xff\xfe\n", b"\n", b""] {
            let text = wire.send(raw);
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                refused,
                "{raw:?}"
            );
        }
        let lines = audit_lines(fx);
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0]["command"], "rm -rf /");
        assert_eq!(lines[1]["command"].as_str().unwrap().len(), 200);
        assert_eq!(lines[0]["verb"], Value::Null);
        // Too long, unterminated, not text, an empty line, nothing at all: only the empty
        // line is a line we read.
        let commands: Vec<&Value> = lines[2..].iter().map(|l| &l["command"]).collect();
        assert_eq!(
            commands,
            [
                &Value::Null,
                &Value::Null,
                &Value::Null,
                &json!(""),
                &Value::Null
            ]
        );
        assert!(lines.iter().all(|l| l["reason"] == "bad_command"));
    });
    assert!(
        logged.iter().any(|l| l.contains("?: bad_command")),
        "{logged:?}"
    );
}

/// Without an audit log nothing that changes the host runs: the install, the switch and the
/// confirm are failures that name it; a query still answers and a revert still goes back,
/// because the way back is never held up by a log.
#[test]
fn without_the_audit_log_only_the_query_and_the_revert_run() {
    let logged = with_serving("sv-noaudit", |fx, wire| {
        wire.ask(&format!("install kbf={VB} from stable"));
        wire.ask(&format!("switch kbf={VB}"));
        let calls = fx.calls().len();
        fs::remove_file(fx.dir.join("audit.log")).unwrap();
        fs::create_dir(fx.dir.join("audit.log")).unwrap();
        let unavailable = json!({"result":"failed","reason":"audit_unavailable"});
        assert_eq!(
            wire.ask(&format!("install kbf={VA} from stable")),
            unavailable
        );
        assert_eq!(wire.ask(&format!("switch kbf={VB}")), unavailable);
        assert_eq!(wire.ask(&format!("confirm kbf={VB}")), unavailable);
        assert_eq!(fx.calls().len(), calls, "nothing was run");
        assert_eq!(wire.ask("query")["result"], "query");
        assert_eq!(wire.ask("revert")["result"], "reverted");
        assert_eq!(fx.world.borrow().current.as_deref(), Some(A));
    });
    assert!(
        logged.iter().any(|l| l.contains("audit log unavailable")),
        "{logged:?}"
    );
    assert!(
        logged
            .iter()
            .any(|l| l.contains("stage: audit_unavailable")),
        "{logged:?}"
    );
}

/// A client that has gone before the answer is written costs a line in the log, not the
/// server.
#[test]
fn a_client_that_is_gone_is_logged_and_forgotten() {
    let logged = with_serving("sv-gone", |_, wire| {
        let (client, server) = UnixStream::pair().unwrap();
        (&client).write_all(b"query\n").unwrap();
        drop(client);
        wire.serving.serve_one(server);
    });
    assert!(
        logged.iter().any(|l| l.contains("the client is gone")),
        "{logged:?}"
    );
}

/// The loop answers connections and ticks between them: with a release on trial whose
/// deadline already passed, the first tick reverts before any client asks, and the client's
/// query then sees the timeout; the loop ends when told to.
#[test]
fn the_loop_answers_clients_and_reverts_a_release_past_its_deadline() {
    let logged = with_serving("sv-loop", |fx, wire| {
        fx.ask(&format!("install kbf={VB} from stable"));
        fx.ask(&format!("switch kbf={VB}"));
        fx.clock.set(100_000);
        let run = fx.dir.join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        let socket = run.join("l.sock");
        let meta = fs::metadata(&*fx.dir).unwrap();
        let listener = bind(&trust(&fx.dir), &socket, (meta.uid(), meta.gid())).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let client = {
            let (stop, socket) = (Arc::clone(&stop), socket.clone());
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream.write_all(b"query\n").unwrap();
                let mut text = String::new();
                stream.read_to_string(&mut text).unwrap();
                stop.store(true, Ordering::Relaxed);
                text
            })
        };
        wire.serving.run(&listener, &stop).unwrap();
        let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
        assert_eq!(answer["pending"], Value::Null);
        assert_eq!(answer["commit"], A);
        assert_eq!(answer["last"]["reason"], "timeout");
        let lines = audit_lines(fx);
        assert_eq!(lines.last().unwrap()["verb"], "revert");
    });
    assert!(
        logged.iter().any(|l| l.contains("the deadline passed")),
        "{logged:?}"
    );
}

/// A listener that fails ends the loop with the error, so that the service manager starts a
/// new one.
#[test]
fn a_failing_listener_ends_the_loop() {
    with_serving("sv-fail", |_, wire| {
        // A socket that was never listened on: accept fails for good, with something other
        // than "nothing yet".
        let fd = rustix::net::socket(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            None,
        )
        .unwrap();
        let listener = UnixListener::from(fd);
        let err = wire
            .serving
            .run(&listener, &AtomicBool::new(false))
            .unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::WouldBlock);
    });
}

/// `Answer` is what the loop tells the log when it reverts on its own: the line of the
/// answer, so a person reading the journal sees what the host did.
#[test]
fn the_tick_logs_what_it_did() {
    let logged = with_serving("sv-tick", |fx, wire| {
        fx.ask(&format!("install kbf={VB} from stable"));
        fx.ask(&format!("switch kbf={VB}"));
        wire.serving.tick();
        assert!(fx.query().pending.is_some(), "the deadline has not passed");
        fx.clock.set(100_000);
        wire.serving.tick();
        assert_eq!(fx.query().pending, None);
    });
    assert_eq!(logged.len(), 1);
    assert!(logged[0].contains("reverted"), "{logged:?}");
    let _ = (
        B,
        Answer::Confirmed {
            version: String::new(),
        },
        Code::Busy,
    );
}

/// A client that sends a byte now and then is cut off at the window for the whole line, not
/// given a new window with every byte; a line sent at once, in pieces within the window, or
/// too long, or not text, is read as it should be.
#[test]
fn a_client_that_trickles_is_cut_off_at_the_window_for_the_whole_line() {
    let window = Duration::from_millis(300);
    let (client, server) = UnixStream::pair().unwrap();
    let slow = std::thread::spawn(move || {
        for _ in 0..40 {
            if (&client).write_all(b"q").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    assert_eq!(read_line(&server, window), None);
    assert!(started.elapsed() < Duration::from_millis(1500));
    drop(server);
    slow.join().unwrap();

    let read = |bytes: &[u8]| {
        let (client, server) = UnixStream::pair().unwrap();
        (&client).write_all(bytes).unwrap();
        drop(client);
        read_line(&server, window)
    };
    assert_eq!(read(b"query\n").as_deref(), Some("query"));
    assert_eq!(read(b"query"), None, "no end of line");
    assert_eq!(read(b""), None, "nothing sent");
    assert_eq!(read(&[0xff, b'\n']), None, "not text");
    let mut long = vec![b'a'; MAX_LINE_LEN];
    long.push(b'\n');
    assert_eq!(read(&long).map(|l| l.len()), Some(MAX_LINE_LEN));
    long.insert(0, b'a');
    assert_eq!(read(&long), None, "one byte too many");
    // In pieces, within the window.
    let (client, server) = UnixStream::pair().unwrap();
    let pieces = std::thread::spawn(move || {
        (&client).write_all(b"que").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        (&client).write_all(b"ry\n").unwrap();
    });
    assert_eq!(read_line(&server, window).as_deref(), Some("query"));
    pieces.join().unwrap();
}

/// A revert that fails with the release still on trial is tried again after a pause, not on
/// every turn of the loop (and its unit restarted five times a second).
#[test]
fn a_revert_that_fails_is_not_retried_on_every_turn() {
    let logged = with_serving("sv-retry", |fx, wire| {
        fx.ask(&format!("install kbf={VB} from stable"));
        fx.ask(&format!("switch kbf={VB}"));
        fx.clock.set(100_000);
        fx.world.borrow_mut().revert_fault = Some(Fault::failed(Code::RevertFailed, "links"));
        let run = fx.dir.join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        let meta = fs::metadata(&*fx.dir).unwrap();
        let listener = bind(
            &trust(&fx.dir),
            &run.join("r.sock"),
            (meta.uid(), meta.gid()),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(900));
                stop.store(true, Ordering::Relaxed);
            })
        };
        wire.serving.run(&listener, &stop).unwrap();
        stopper.join().unwrap();
        let reverts = fx
            .world
            .borrow()
            .calls
            .iter()
            .filter(|c| *c == "revert")
            .count();
        assert_eq!(reverts, 1);
        assert!(fx.query().pending.is_some(), "still on trial");
    });
    // One attempt, said twice: the loop's line and the failure's.
    assert_eq!(logged.len(), 2, "{logged:?}");
}

/// `--frontend-container` is a flag with no value, given once, and it replaces the units: a
/// host serves a container or restarts units, not both.
#[test]
fn the_frontend_container_flag_stands_for_the_units() {
    let base = ["--socket", "/s", "--socket-user", "u"];
    let with = |extra: &[&str]| {
        let mut words: Vec<&str> = base.to_vec();
        words.extend(extra);
        ServeConfig::parse(&args(&words))
    };
    let config = with(&["--frontend-container"]).unwrap();
    assert!(config.frontend_container && config.units.is_empty());
    // It may come before the flags that take a value.
    let first = ServeConfig::parse(&args(&[
        "--frontend-container",
        "--socket",
        "/s",
        "--socket-user",
        "u",
    ]))
    .unwrap();
    assert!(first.frontend_container);
    for bad in [
        &["--frontend-container", "--frontend-container"][..],
        &["--frontend-container=yes"],
        &[
            "--frontend-container",
            "--restart-unit",
            "kbf-daemon.service",
        ],
        &[
            "--restart-unit",
            "kbf-daemon.service",
            "--frontend-container",
        ],
    ] {
        assert!(with(bad).is_err(), "{bad:?}");
    }
    assert!(
        with(&["--frontend-container", "--frontend-container"])
            .unwrap_err()
            .contains("takes no value and is given once")
    );
}
