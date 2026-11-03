//! An agent started before its host is enrolled, as the packaged unit starts it at boot: it says
//! once that it is not enrolled and what it waits for, calls nothing while it waits, and once the
//! enrollment is in its state directory it starts and heartbeats. Every wait is driven by the
//! test's own nap, never by a clock.

use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;

use super::build_tests::enrolled_in;
use super::coordinator_tests::{ok, serve, server_config};
use super::enrolled::{Waited, Waiting, not_enrolled, wait};
use super::flags::{Config, parse_flags};
use super::pki::Pki;
use super::run::Stop;
use super::run_config_waiting;
use crate::identity::identity_of;

/// The text an agent says when its state directory holds no enrollment, spelled out so that a
/// change to it shows here.
fn said(state: &Path) -> String {
    format!(
        "not enrolled: {0} holds no enrollment yet; waiting for `kbf-updater enroll --state-dir \
         {0}`, and starting once it has run",
        state.display()
    )
}

fn config(words: &[String]) -> Config {
    parse_flags(words).expect("the agent's flags")
}

/// The flags of an agent with an enrollment directory, as the packaged unit gives them: the
/// coordinator, the frontend's enroll listener (where the certificate is renewed), the directory,
/// the channel, and the helper's socket.
fn flags(
    coordinator: u16,
    enroll: u16,
    state: &Path,
    helper: &Path,
    more: &[String],
) -> Vec<String> {
    let mut words: Vec<String> = [
        "--coordinator".to_owned(),
        format!("localhost:{coordinator}"),
        "--enroll-frontend".to_owned(),
        format!("localhost:{enroll}"),
        "--state-dir".to_owned(),
        state.display().to_string(),
        "--channel".to_owned(),
        "stable".to_owned(),
        "--helper-socket".to_owned(),
        helper.display().to_string(),
    ]
    .to_vec();
    words.extend_from_slice(more);
    words
}

/// What a run saw: the lines it said and how many naps it took.
#[derive(Clone, Default)]
struct Seen {
    lines: Arc<Mutex<Vec<String>>>,
    naps: Arc<AtomicUsize>,
}

impl Seen {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn naps(&self) -> usize {
        self.naps.load(Ordering::SeqCst)
    }
}

/// A TCP listener whose port an agent is given, to tell afterwards whether anything dialled it: a
/// connection made to it waits in its backlog until it is taken.
struct Watched(TcpListener);

impl Watched {
    fn new() -> Self {
        Self(TcpListener::bind("127.0.0.1:0").unwrap())
    }

    fn port(&self) -> u16 {
        self.0.local_addr().unwrap().port()
    }

    fn dialled(&self) -> bool {
        self.0.set_nonblocking(true).unwrap();
        self.0.accept().is_ok()
    }
}

/// An agent given a state directory that holds no enrollment says so once, in so many words,
/// calls nothing while it waits (not the coordinator, not the frontend's worker or enroll listener,
/// not the helper's socket), and ends with exit 0 when it is stopped during the wait, however many
/// naps it took.
///
/// Catches: an agent that exits 1 on a missing certificate (and is restarted in a loop by its
/// unit), that says the line on every look, or that dials anything before it is enrolled.
#[test]
fn an_agent_with_no_enrollment_says_not_enrolled_once_and_calls_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-wait");
    let state = dir.join("state");
    let (coordinator, frontend, enroll) = (Watched::new(), Watched::new(), Watched::new());
    let helper = dir.join("local.sock");
    let helper_listener = UnixListener::bind(&helper).unwrap();
    let cfg = config(&flags(
        coordinator.port(),
        enroll.port(),
        &state,
        &helper,
        &[
            "--frontend".to_owned(),
            format!("localhost:{}", frontend.port()),
        ],
    ));
    let seen = Seen::default();
    let stop_at = Arc::new(Mutex::new(None::<Stop>));
    let (log_seen, nap_seen, handle) = (seen.clone(), seen.clone(), Arc::clone(&stop_at));
    let log = move |line: &str| log_seen.lines.lock().unwrap().push(line.to_owned());
    // The third nap asks the agent to stop: it has looked three times by then.
    let nap = move |_: &Stop| {
        if nap_seen.naps.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            handle.lock().unwrap().as_ref().unwrap().stop();
        }
    };
    let keep = Arc::clone(&stop_at);
    let (code, text) = run_config_waiting(
        &cfg,
        move |stop| *keep.lock().unwrap() = Some(stop),
        &Waiting {
            nap: &nap,
            log: &log,
        },
    );
    assert_eq!((code, text.as_str()), (0, ""));
    assert_eq!(seen.lines(), [said(&state)]);
    assert_eq!(seen.lines(), [not_enrolled(&state)]);
    assert_eq!(seen.naps(), 3);
    assert!(!coordinator.dialled(), "the coordinator was dialled");
    assert!(!frontend.dialled(), "the frontend was dialled");
    assert!(!enroll.dialled(), "the enroll listener was dialled");
    helper_listener.set_nonblocking(true).unwrap();
    assert!(
        helper_listener.accept().is_err(),
        "the helper's socket was dialled"
    );
}

/// An agent that waited and then finds the enrollment `kbf-updater enroll` leaves (here written
/// at its first nap) starts: its first heartbeat reaches the coordinator over mutual TLS with the
/// enrolled certificate, after one line and one nap. An agent whose host was enrolled before it
/// started says nothing and naps never.
///
/// Catches: an agent that waits on after the enrollment is there, that must be restarted to pick
/// it up, or that waits (and says it is not enrolled) on a host that is.
#[test]
fn once_the_host_is_enrolled_the_waiting_agent_heartbeats() {
    for enrolled_before in [false, true] {
        let dir = TestDir::under(std::env::temp_dir(), "enrolled-start");
        let pki = Pki::new();
        if enrolled_before {
            enrolled_in(&dir, &pki, HostRole::DAEMON);
        }
        let state = dir.join("state");
        let (addr, coordinator) = serve(
            server_config(&pki, &pki.coordinator()),
            vec![ok(r#"{"reply":"nothing"}"#)],
        );
        let cfg = config(&flags(
            addr.port(),
            1,
            &state,
            &dir.join("no-helper.sock"),
            &[],
        ));
        let seen = Seen::default();
        let (log_seen, nap_seen) = (seen.clone(), seen.clone());
        let log = move |line: &str| log_seen.lines.lock().unwrap().push(line.to_owned());
        let nap = |_: &Stop| {
            nap_seen.naps.fetch_add(1, Ordering::SeqCst);
            enrolled_in(&dir, &pki, HostRole::DAEMON);
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let got = Arc::clone(&calls);
        // Stopped once the coordinator has answered its one heartbeat.
        let (code, text) = run_config_waiting(
            &cfg,
            move |stop| {
                std::thread::spawn(move || {
                    *got.lock().unwrap() = coordinator.join().unwrap();
                    stop.stop();
                });
            },
            &Waiting {
                nap: &nap,
                log: &log,
            },
        );
        assert_eq!((code, text.as_str()), (0, ""), "{enrolled_before}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let request = String::from_utf8_lossy(&calls[0].request);
        assert!(request.starts_with("POST "), "{request}");
        let client = identity_of(calls[0].client.as_deref().expect("a client certificate"))
            .expect("an updater's certificate");
        assert_eq!(client.node_id.to_string(), "n1");
        if enrolled_before {
            assert_eq!((seen.lines(), seen.naps()), (Vec::<String>::new(), 0));
        } else {
            assert_eq!((seen.lines(), seen.naps()), (vec![said(&state)], 1));
        }
    }
}

/// A state directory whose enrollment cannot be read is not waited for: the agent is not started,
/// exit 1 names the damaged file, and it neither naps nor says it is not enrolled. An agent given
/// files an operator placed has no enrollment to wait for: it is built at once, and a certificate
/// that is not there is exit 1 naming it, with no nap and no line.
///
/// Catches: an agent that takes a damaged enrollment for a missing one and waits forever, or one
/// that waits for an enrollment it was not told to use.
#[test]
fn a_damaged_enrollment_or_placed_files_are_never_waited_for() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-damaged");
    let state = dir.join("state");
    std::fs::create_dir_all(state.join("pki")).unwrap();
    std::fs::write(state.join("pki/node.json"), b"{not json").unwrap();
    let seen = Seen::default();
    let (log_seen, nap_seen) = (seen.clone(), seen.clone());
    let log = move |line: &str| log_seen.lines.lock().unwrap().push(line.to_owned());
    let nap = move |_: &Stop| {
        nap_seen.naps.fetch_add(1, Ordering::SeqCst);
    };
    let waiting = Waiting {
        nap: &nap,
        log: &log,
    };
    let cfg = config(&flags(1, 1, &state, &dir.join("no-helper.sock"), &[]));
    let (code, text) = run_config_waiting(&cfg, |_| {}, &waiting);
    assert_eq!(
        (code, text),
        (
            1,
            format!(
                "kbf-updater: {} is damaged\n",
                state.join("pki/node.json").display()
            )
        )
    );

    let placed: PathBuf = dir.join("placed");
    let words: Vec<String> = [
        "--coordinator",
        "localhost:1",
        "--client-cert",
        &placed.join("cert.pem").display().to_string(),
        "--client-key",
        &placed.join("key.pem").display().to_string(),
        "--ca",
        &placed.join("ca.pem").display().to_string(),
        "--channel",
        "stable",
    ]
    .iter()
    .map(|w| (*w).to_owned())
    .collect();
    let (code, text) = run_config_waiting(&config(&words), |_| {}, &waiting);
    assert_eq!(code, 1);
    assert!(
        text.contains(&placed.join("cert.pem").display().to_string()),
        "{text}"
    );
    assert_eq!((seen.lines(), seen.naps()), (Vec::<String>::new(), 0));
}

/// The wait alone: an agent asked to stop before its first look still looks once (an enrolled
/// host is never kept waiting by a stop), says the line once, and ends after one nap, which a stop
/// cuts short.
#[test]
fn a_stop_before_the_first_look_ends_the_wait_after_one_look() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-stopped");
    let state = dir.join("state");
    let stop = Stop::default();
    stop.stop();
    let lines = Mutex::new(Vec::new());
    let log = |line: &str| lines.lock().unwrap().push(line.to_owned());
    let naps = AtomicUsize::new(0);
    let nap = |_: &Stop| {
        naps.fetch_add(1, Ordering::SeqCst);
    };
    let waited = wait(
        &state,
        &stop,
        &Waiting {
            nap: &nap,
            log: &log,
        },
    );
    assert_eq!(waited, Ok(Waited::Stopped));
    assert_eq!(*lines.lock().unwrap(), [said(&state)]);
    assert_eq!(naps.load(Ordering::SeqCst), 1);
    let nap = |_: &Stop| panic!("an enrolled host never naps");

    let pki = Pki::new();
    let state = enrolled_in(&dir, &pki, HostRole::DAEMON);
    let quiet = |_: &str| panic!("an enrolled host is not said to be unenrolled");
    let waited = wait(
        &state,
        &stop,
        &Waiting {
            nap: &nap,
            log: &quiet,
        },
    );
    assert_eq!(waited, Ok(Waited::Enrolled));
}

/// The process's own wait (`run_config`, as `kbf-updater agent` runs it): an agent that is not
/// enrolled and is stopped as it starts says its line on the error stream, naps until the stop
/// (which cuts the nap short), and ends with exit 0, having called nothing.
///
/// Catches: a production nap that ignores the stop, so that stopping a waiting agent hangs.
#[test]
fn the_process_stops_while_it_waits_for_its_enrollment() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-process");
    let coordinator = Watched::new();
    let cfg = config(&flags(
        coordinator.port(),
        1,
        &dir.join("state"),
        &dir.join("no-helper.sock"),
        &[],
    ));
    let (code, text) = super::run_config(&cfg, |stop| stop.stop());
    assert_eq!((code, text.as_str()), (0, ""));
    assert!(!coordinator.dialled(), "the coordinator was dialled");
}
