//! An agent started before its host is enrolled, as the packaged unit starts it at boot: it says
//! once that it is not enrolled and what it waits for, makes no port while it waits (so it calls
//! nothing), finishes an enrollment a crash cut short in its commit, and once the enrollment is
//! usable it is made from the enrolled files and heartbeats. Every wait is driven by the test's
//! own nap and every stop by a handshake, never by a clock; no test opens a socket: the agent is
//! made over fake ports, with a coordinator that says when it got a heartbeat.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use kbf_ca::{PublicKey, SystemEntropy, UpdaterCert};
use kbf_enroll::answer::Cert;
use kbf_enroll::store::{Node, Store, StoredConfig};
use kbf_testkit::dir::TestDir;
use kbf_types::{CaPin, HostRole, ReleasePlatform, Role as PkiRole, RoleSet};

use super::build::{identity_of_files, this_host};
use super::build_tests::enrolled_in;
use super::coordinator::{Coordinator, CoordinatorError};
use super::enrolled::{Waited, Waiting, not_enrolled, wait};
use super::flags::{Config, parse_flags};
use super::pki::{Pki, now, pem_of};
use super::run::{Agent, Stop, World};
use super::run_config_waiting;
use super::status::Status;
use super::testing::{Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, version};
use crate::identity::Identity;
use crate::ports::SystemClock;
use crate::wire::{Answer, Heartbeat, Reply, ServiceReply};

/// The text an agent says when its state directory holds no enrollment, spelled out so that a
/// change to it shows here.
fn said(state: &Path) -> String {
    format!(
        "not enrolled: {0} holds no enrollment yet; waiting for `kbf-updater enroll --state-dir \
         {0}`, and starting once it has run",
        state.display()
    )
}

/// The flags of an agent with an enrollment directory, as the packaged unit gives them: the
/// coordinator, the frontend's enroll listener (where the certificate is renewed), the directory,
/// the channel, and the helper's socket. No test here dials any of them: the agent is made over
/// fakes.
fn config(state: &Path) -> Config {
    let words: Vec<String> = [
        "--coordinator",
        "coordinator.farm.example:8443",
        "--enroll-frontend",
        "frontend.farm.example:8444",
        "--frontend",
        "frontend.farm.example:8981",
        "--state-dir",
        &state.display().to_string(),
        "--channel",
        "stable",
        "--helper-socket",
        "/run/kbf-host-install/local.sock",
    ]
    .iter()
    .map(|w| (*w).to_owned())
    .collect();
    parse_flags(&words).expect("the agent's flags")
}

/// A coordinator that records the heartbeats and says on `beat` when the first arrives.
struct Signalling {
    inner: FakeCoordinator,
    beat: Mutex<Option<Sender<()>>>,
}

impl Signalling {
    fn tell(&self) {
        if let Some(beat) = self.beat.lock().unwrap().take() {
            let _ = beat.send(());
        }
    }
}

impl Coordinator for Signalling {
    fn heartbeat(&self, beat: &Heartbeat) -> Result<Reply, CoordinatorError> {
        let reply = self.inner.heartbeat(beat);
        self.tell();
        reply
    }

    fn heartbeat_services(&self, beat: &Heartbeat) -> Result<ServiceReply, CoordinatorError> {
        let reply = self.inner.heartbeat_services(beat);
        self.tell();
        reply
    }

    fn heartbeat_answer(&self, beat: &Heartbeat) -> Result<Answer, CoordinatorError> {
        let reply = self.inner.heartbeat_answer(beat);
        self.tell();
        reply
    }
}

/// What a run saw: the lines it said, its naps, the identities it made agents for, and the
/// heartbeats its coordinator got.
#[derive(Clone, Default)]
struct Seen {
    lines: Arc<Mutex<Vec<String>>>,
    naps: Arc<AtomicUsize>,
    made: Arc<Mutex<Vec<Identity>>>,
    coordinator: FakeCoordinator,
}

impl Seen {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn naps(&self) -> usize {
        self.naps.load(Ordering::SeqCst)
    }

    fn made(&self) -> Vec<Identity> {
        self.made.lock().unwrap().clone()
    }

    fn log(&self) -> impl Fn(&str) + '_ {
        |line: &str| self.lines.lock().unwrap().push(line.to_owned())
    }

    /// The agent's maker: the identity of the certificate `cfg` names (the enrollment's), then
    /// an agent of that host over fakes, whose coordinator says on `beat` when it got the first
    /// heartbeat.
    fn maker(&self, beat: Sender<()>) -> impl FnOnce(&Config) -> Result<Agent, String> + use<'_> {
        move |cfg: &Config| {
            let identity = identity_of_files(&cfg.files)?;
            let host = this_host(&identity, None)?;
            self.made.lock().unwrap().push(identity);
            let events = Events::default();
            Ok(Agent::new(
                host,
                cfg.channel.clone(),
                cfg.timing,
                World {
                    coordinator: Arc::new(Signalling {
                        inner: self.coordinator.clone(),
                        beat: Mutex::new(Some(beat)),
                    }),
                    local: Arc::new(FakeLocal::new(&events)),
                    drain: Arc::new(FakeDrain::new(&events)),
                    health: Arc::new(FakeHealth::new(&events)),
                    clock: Arc::new(SystemClock::new()),
                    unix_now: Arc::new(|| 10_000),
                    renewer: None,
                    log: Arc::new(|_| {}),
                },
                Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
            ))
        }
    }
}

/// Runs the agent of `state` until its coordinator has its first heartbeat, with `nap` between
/// two looks. The exit code and text.
fn run_until_a_heartbeat(seen: &Seen, state: &Path, nap: &dyn Fn(&Stop)) -> (u8, String) {
    let (beat, beaten) = channel();
    let log = seen.log();
    run_config_waiting(
        &config(state),
        move |stop| {
            std::thread::spawn(move || {
                if beaten.recv().is_ok() {
                    stop.stop();
                }
            });
        },
        &Waiting { nap, log: &log },
        seen.maker(beat),
    )
}

/// An enrollment as `kbf-updater enroll` makes one, step by step, in a state directory.
struct Enrolling {
    store: Store,
    pki: Pki,
    certs: Vec<Cert>,
    node: Option<Node>,
}

impl Enrolling {
    /// The directory, opened as `enroll` opens it.
    fn new(state: &Path) -> Self {
        Self {
            store: Store::open(state).unwrap(),
            pki: Pki::new(),
            certs: Vec::new(),
            node: None,
        }
    }

    /// The key and the request, persisted before they are sent (what a failed attempt leaves).
    fn pending(&mut self) {
        let pending = self
            .store
            .pending(RoleSet::new([PkiRole::Updater]).unwrap())
            .unwrap();
        let public = PublicKey::from_spki(&pending[0].spki).unwrap();
        let node_id = "n1".parse().unwrap();
        let issued = self
            .pki
            .ca
            .issue_updater(
                &UpdaterCert {
                    node_id: &node_id,
                    host_role: &HostRole::DAEMON,
                    key: &public,
                },
                now(),
                &mut SystemEntropy,
            )
            .unwrap();
        self.certs = vec![Cert {
            role: PkiRole::Updater,
            pem: pem_of("CERTIFICATE", &issued.der),
            serial: "a1".to_owned(),
            not_before: 1,
            not_after: now() + 200_000,
        }];
        self.node = Some(Node::new(
            &node_id,
            &CaPin::from_bytes([3; 32]),
            &self.certs,
            now() + 100_000,
            StoredConfig::default(),
        ));
    }

    /// The accepted answer, staged: `node.json` in `staging/`, nothing installed yet.
    fn stage(&self) {
        let node = self.node.as_ref().unwrap();
        self.store
            .stage(&self.certs, self.pki.root_pem().as_bytes(), node)
            .unwrap();
    }

    /// The commit cut short after `moved` of the files it moves before `node.json` (the CA
    /// bundle, the key, the certificate), as a crash leaves it.
    fn commit_cut_after(&self, state: &Path, moved: usize) {
        for name in ["ca.pem", "updater.key", "updater.pem"].iter().take(moved) {
            std::fs::rename(
                state.join("pki/staging").join(name),
                state.join("pki").join(name),
            )
            .unwrap();
        }
    }
}

/// An agent given a state directory that holds no enrollment says so once, in so many words,
/// makes no agent (no coordinator, frontend or helper port exists, so nothing is called) while it
/// waits, and ends with exit 0 when it is stopped during the wait, however many naps it took.
///
/// Catches: an agent that exits 1 on a missing certificate (and is restarted in a loop by its
/// unit), that says the line on every look, or that makes its ports before it is enrolled.
#[test]
fn an_agent_with_no_enrollment_says_not_enrolled_once_and_calls_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-wait");
    let state = dir.join("state");
    let seen = Seen::default();
    let stop_at = Arc::new(Mutex::new(None::<Stop>));
    let handle = Arc::clone(&stop_at);
    // The third nap asks the agent to stop: it has looked three times by then.
    let nap = |_: &Stop| {
        if seen.naps.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            handle.lock().unwrap().as_ref().unwrap().stop();
        }
    };
    let (beat, _) = channel();
    let log = seen.log();
    let keep = Arc::clone(&stop_at);
    let (code, text) = run_config_waiting(
        &config(&state),
        move |stop| *keep.lock().unwrap() = Some(stop),
        &Waiting {
            nap: &nap,
            log: &log,
        },
        seen.maker(beat),
    );
    assert_eq!((code, text.as_str()), (0, ""));
    assert_eq!(seen.lines(), [said(&state)]);
    assert_eq!(seen.lines(), [not_enrolled(&state)]);
    assert_eq!(seen.naps(), 3);
    assert!(seen.made().is_empty(), "an agent was made");
    assert!(seen.coordinator.beats().is_empty());
}

/// An agent that waited and then finds the enrollment `kbf-updater enroll` leaves (written at its
/// first nap) is made from the enrolled certificate, node `n1`, and heartbeats, after one line and
/// one nap. An agent whose host was enrolled before it started says nothing and naps never.
///
/// Catches: an agent that waits on after the enrollment is there, that must be restarted to pick
/// it up, that is made from other files, or that waits (and says it is not enrolled) on a host that
/// is.
#[test]
fn once_the_host_is_enrolled_the_waiting_agent_heartbeats() {
    for enrolled_before in [false, true] {
        let dir = TestDir::under(std::env::temp_dir(), "enrolled-start");
        let pki = Pki::new();
        if enrolled_before {
            enrolled_in(&dir, &pki, HostRole::DAEMON);
        }
        let state = dir.join("state");
        let seen = Seen::default();
        let nap = |_: &Stop| {
            seen.naps.fetch_add(1, Ordering::SeqCst);
            enrolled_in(&dir, &pki, HostRole::DAEMON);
        };
        let (code, text) = run_until_a_heartbeat(&seen, &state, &nap);
        assert_eq!((code, text.as_str()), (0, ""), "{enrolled_before}");
        let made = seen.made();
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].node_id.to_string(), "n1");
        assert!(!seen.coordinator.beats().is_empty());
        if enrolled_before {
            assert_eq!((seen.lines(), seen.naps()), (Vec::<String>::new(), 0));
        } else {
            assert_eq!((seen.lines(), seen.naps()), (vec![said(&state)], 1));
        }
    }
}

/// A first enrollment that a crash cut short in its commit, at every point of it (nothing moved,
/// the CA bundle, then the key, then the certificate, `node.json` still staged), is finished by
/// the agent at its first look, under the store's lock: the agent says nothing, naps never, is made
/// from the enrolled certificate and heartbeats, and the directory holds the whole enrollment.
///
/// Catches: an agent that takes a staged enrollment for none and waits forever, so that the host
/// needs an operator to run `enroll` again.
#[test]
fn an_enrollment_a_crash_cut_short_in_its_commit_is_finished_and_the_agent_starts() {
    for moved in 0..=3 {
        let dir = TestDir::under(std::env::temp_dir(), "enrolled-cut");
        let state = dir.join("state");
        let mut enrolling = Enrolling::new(&state);
        enrolling.pending();
        enrolling.stage();
        enrolling.commit_cut_after(&state, moved);
        let seen = Seen::default();
        let nap = |_: &Stop| panic!("a staged enrollment is finished at the first look");
        let (code, text) = run_until_a_heartbeat(&seen, &state, &nap);
        assert_eq!((code, text.as_str()), (0, ""), "{moved}");
        assert_eq!((seen.lines(), seen.naps()), (Vec::<String>::new(), 0));
        assert_eq!(seen.made()[0].node_id.to_string(), "n1");
        assert!(!seen.coordinator.beats().is_empty());
        assert!(state.join("pki/node.json").exists(), "{moved}");
        assert!(!state.join("pki/staging").exists(), "{moved}");
    }
}

/// The agent looks between the steps of an enrollment: after the key and request alone (what a
/// refused attempt leaves, and what a retry starts from), after that attempt failed, and once the
/// retry's answer is staged. It says its line once, makes no agent while only the key is there,
/// and is made from the enrolled certificate and heartbeats once the answer is staged.
///
/// Catches: an agent that takes a key without a certificate for an enrollment, that says its line
/// again after a failed attempt, or that is made before the enrollment is usable.
#[test]
fn the_agent_looks_between_the_steps_of_an_enrollment_and_starts_once_it_is_usable() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-steps");
    let state = dir.join("state");
    let enrolling = Mutex::new(None::<Enrolling>);
    let seen = Seen::default();
    let nap = |_: &Stop| {
        let n = seen.naps.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(
            seen.made().is_empty(),
            "made before the enrollment is usable"
        );
        let mut enrolling = enrolling.lock().unwrap();
        match n {
            // The first attempt sends its request: the key is kept; its answer is a refusal.
            1 => {
                let mut first = Enrolling::new(&state);
                first.pending();
                *enrolling = Some(first);
            }
            // Nothing new: the attempt failed, the operator has not run `enroll` again.
            2 => {}
            // The retry sends the same request and its answer is staged, then committed.
            3 => enrolling.as_ref().unwrap().stage(),
            _ => panic!("a usable enrollment is started from"),
        }
    };
    let (code, text) = run_until_a_heartbeat(&seen, &state, &nap);
    assert_eq!((code, text.as_str()), (0, ""));
    assert_eq!((seen.lines(), seen.naps()), (vec![said(&state)], 3));
    assert_eq!(seen.made().len(), 1);
    assert_eq!(seen.made()[0].node_id.to_string(), "n1");
    assert!(!seen.coordinator.beats().is_empty());
}

/// The text of the error the agent ends with when its enrollment cannot be read: the directory,
/// then the store's own words.
fn unreadable(state: &Path, store: &str) -> String {
    format!(
        "kbf-updater: the enrollment in {} cannot be read: {store}\n",
        state.display()
    )
}

/// A state directory whose enrollment cannot be read is not waited for, before the wait or after it
/// began: a `node.json` that does not parse, and one that cannot be read at all (a directory in its
/// place), are exit 1 with the whole error, no agent is made, and the line was said once at most.
/// An agent given files an operator placed has no enrollment to wait for: it is made at once.
///
/// Catches: an agent that takes an unreadable enrollment for a missing one and waits forever, one
/// that hides the store's error, and one that waits for an enrollment it was not told to use.
#[test]
fn an_enrollment_that_cannot_be_read_ends_the_wait_and_placed_files_are_never_waited_for() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-unreadable");
    let none = |_: &Stop| panic!("no nap");

    // Damaged, before the first look.
    let state = dir.join("damaged");
    std::fs::create_dir_all(state.join("pki")).unwrap();
    std::fs::write(state.join("pki/node.json"), b"{not json").unwrap();
    let seen = Seen::default();
    let (code, text) = run_until_a_heartbeat(&seen, &state, &none);
    let store = format!("{} is damaged", state.join("pki/node.json").display());
    assert_eq!((code, text), (1, unreadable(&state, &store)));
    assert_eq!((seen.lines(), seen.made().len()), (Vec::<String>::new(), 0));

    // Not readable, before the first look.
    let state = dir.join("unreadable");
    let node_json = state.join("pki/node.json");
    std::fs::create_dir_all(&node_json).unwrap();
    let why = std::fs::read(&node_json).unwrap_err();
    let store = format!("{}: {why}", node_json.display());
    let seen = Seen::default();
    let (code, text) = run_until_a_heartbeat(&seen, &state, &none);
    assert_eq!((code, text), (1, unreadable(&state, &store)));
    assert_eq!((seen.lines(), seen.made().len()), (Vec::<String>::new(), 0));

    // Not readable once the wait began: the first nap puts a directory where `node.json` goes.
    let state = dir.join("unreadable-later");
    let node_json = state.join("pki/node.json");
    let seen = Seen::default();
    let nap = |_: &Stop| {
        seen.naps.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(&node_json).unwrap();
    };
    let (code, text) = run_until_a_heartbeat(&seen, &state, &nap);
    assert_eq!((code, text), (1, unreadable(&state, &store_of(&node_json))));
    assert_eq!((seen.lines(), seen.naps()), (vec![said(&state)], 1));
    assert!(seen.made().is_empty());

    // Placed files: made at once, from the files named, and never waited for.
    let placed: PathBuf = dir.join("placed");
    let words: Vec<String> = [
        "--coordinator",
        "coordinator.farm.example:8443",
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
    let seen = Seen::default();
    let (beat, _) = channel();
    let log = seen.log();
    let (code, text) = run_config_waiting(
        &parse_flags(&words).unwrap(),
        |_| {},
        &Waiting {
            nap: &none,
            log: &log,
        },
        seen.maker(beat),
    );
    assert_eq!(code, 1);
    assert!(
        text.contains(&placed.join("cert.pem").display().to_string()),
        "{text}"
    );
    assert_eq!(seen.lines(), Vec::<String>::new());
}

/// The store's words for a path that cannot be read as a file.
fn store_of(path: &Path) -> String {
    format!("{}: {}", path.display(), std::fs::read(path).unwrap_err())
}

/// The wait alone: an agent asked to stop before its first look still looks once (an enrolled
/// host is never kept waiting by a stop), says the line once, and ends after one nap, which a stop
/// cuts short; an enrolled host never naps.
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
    assert!(matches!(waited, Ok(Waited::Stopped)), "{waited:?}");
    assert_eq!(*lines.lock().unwrap(), [said(&state)]);
    assert_eq!(naps.load(Ordering::SeqCst), 1);

    let pki = Pki::new();
    let state = enrolled_in(&dir, &pki, HostRole::DAEMON);
    let nap = |_: &Stop| panic!("an enrolled host never naps");
    let quiet = |_: &str| panic!("an enrolled host is not said to be unenrolled");
    let waited = wait(
        &state,
        &stop,
        &Waiting {
            nap: &nap,
            log: &quiet,
        },
    );
    assert!(matches!(waited, Ok(Waited::Enrolled)), "{waited:?}");
}

/// The process's own nap (the agent's `sleep` of [`super::enrolled::LOOK_EVERY`], which a stop cuts
/// short), stopped once the wait has begun: the line the agent says is the sign it has, and the stop
/// is asked only then. The run ends with exit 0 and no agent made.
///
/// Catches: a production nap that a stop does not end, so that stopping a waiting agent hangs.
#[test]
fn the_process_stops_once_its_wait_has_begun() {
    let dir = TestDir::under(std::env::temp_dir(), "enrolled-process");
    let state = dir.join("state");
    let seen = Seen::default();
    let (began, waiting) = channel::<()>();
    let began = Mutex::new(began);
    let log = |line: &str| {
        seen.lines.lock().unwrap().push(line.to_owned());
        began.lock().unwrap().send(()).unwrap();
    };
    let nap = |stop: &Stop| super::run::sleep(super::enrolled::LOOK_EVERY, stop);
    let (beat, _) = channel();
    let (code, text) = run_config_waiting(
        &config(&state),
        move |stop| {
            std::thread::spawn(move || {
                if waiting.recv().is_ok() {
                    stop.stop();
                }
            });
        },
        &Waiting {
            nap: &nap,
            log: &log,
        },
        seen.maker(beat),
    );
    assert_eq!((code, text.as_str()), (0, ""));
    assert_eq!(seen.lines(), [said(&state)]);
    assert!(seen.made().is_empty());
}
