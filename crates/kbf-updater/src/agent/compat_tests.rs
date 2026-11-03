//! This agent against the previous helper's answers (rule B8 of AGENTS.md: a release runs on
//! what the other writes). The previous helper's answers are this release's answer lines for
//! the codes it had, which `kbf-host-install`'s `tests/previous_answer.rs` holds byte for byte
//! equal to the lines its shipped `answer.rs` makes. The agent talks over a real socket,
//! through `SocketLocal`. (The previous agent against this helper is `tests/previous_release.rs`.)

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_host_install::contract::local::{Answer, Code, QueryView};
use kbf_types::ReleasePlatform;

use super::flags::Timing;
use super::hold::Features;
use super::local::SocketLocal;
use super::local_tests::{dir, limits, line, serve};
use super::status::{Shared, Status};
use super::testing::{
    COMMIT_A, COMMIT_B, Events, FakeDrain, FakeHealth, ManualClock, VA, VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::wire::{Outcome, UpdateResult};

/// What the helper answers each request with: by its verb.
fn helper(
    answers: impl Fn(&str) -> Answer + Send + 'static,
) -> impl Fn(&str) -> Option<String> + Send + 'static {
    move |request: &str| line(&answers(request.split(' ').next().unwrap_or_default()))
}

/// The answer this release's helper gives a `query` of a Mac that runs A, nothing on trial.
fn runs_a() -> Answer {
    Answer::Queried(QueryView {
        commit: Some(COMMIT_A.into()),
        version: Some(VA.into()),
        node_id: Some("n1".into()),
        pending: None,
        last: None,
        busy: false,
        releases: Vec::new(),
        releases_omitted: 0,
        releases_unavailable: false,
    })
}

fn staged() -> Answer {
    Answer::Staged {
        version: VB.into(),
        commit: COMMIT_B.into(),
        current: false,
    }
}

/// The drain, the health and the clock of a Mac whose node the frontend counts idle and whose
/// release A runs healthy.
struct World {
    /// What the fakes saw, in order (kept for them; the tests read the socket's requests).
    _events: Events,
    drain: FakeDrain,
    health: FakeHealth,
    clock: ManualClock,
    status: Shared,
    logs: Arc<Mutex<Vec<String>>>,
}

impl World {
    fn new() -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        Self {
            drain: FakeDrain::new(&events),
            health,
            clock: ManualClock::default(),
            status: Shared::new(Status::unknown(ReleasePlatform::DarwinArm64, version(VA))),
            logs: Arc::default(),
            _events: events,
        }
    }

    fn log(&self) -> impl Fn(&str) + '_ {
        move |line: &str| self.logs.lock().unwrap().push(line.to_owned())
    }
}

/// Catches this agent reading a previous helper's refusal as a deferral: against a helper that
/// answers a switch with each code the previous helper had (none is `leases_live`), this
/// release's update, told the coordinator takes deferrals, goes back as before (`revert`, then
/// `query`) and ends in a `Failed` of the target with no hold.
#[test]
fn this_agent_goes_back_as_before_on_each_refusal_of_the_previous_helper() {
    for code in Code::ALL.into_iter().filter(|c| *c != Code::LeasesLive) {
        let d = dir(&format!("compat-prev-helper-{}", code.code()));
        let (socket, server) = serve(
            &d,
            5,
            helper(move |verb| match verb {
                "install" => staged(),
                "switch" => Answer::Refused(code, "no".to_owned()),
                "revert" => Answer::Refused(Code::NothingPending, "nothing is on trial".to_owned()),
                _ => runs_a(),
            }),
        );
        let world = World::new();
        let local = SocketLocal::new(&socket, limits());
        let host = host(Role::MacDaemon);
        let log = world.log();
        let unix_now = || 10_000;
        let update = super::update::Update {
            ports: super::update::Ports {
                local: &local,
                drain: &world.drain,
                health: &world.health,
                clock: &world.clock,
                unix_now: &unix_now,
                log: &log,
            },
            host: &host,
            channel: "stable",
            timing: Timing {
                retry: Duration::from_secs(900),
                ..Timing::DEFAULT
            },
            status: &world.status,
        };
        let done = update.run_for(
            &version(VB),
            Features {
                deferred: true,
                halt: true,
            },
        );
        assert_eq!(
            done.result,
            UpdateResult::new(version(VB), Outcome::Failed),
            "{}",
            code.code()
        );
        assert_eq!(done.hold, None, "{}", code.code());
        let asked = server.join().unwrap();
        assert_eq!(&asked[3..], ["revert", "query"], "{}", code.code());
    }
}
