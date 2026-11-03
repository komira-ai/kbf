//! An agent of the previous release against this release's helper (rule B8 of AGENTS.md: a
//! release runs on what the other writes).
//!
//! The previous agent is its code as it shipped. `previous_release/v0_0_63_update.rs` is
//! v0.0.63's `src/agent/update.rs`, compiled here beside this crate's modules under the paths it
//! names (`agent::{flags, hold, local, status}`, `inventory`, `ports`, `version`, `wire`, which
//! it reads and which did not change since; `hold` only gained a constant). Its socket reader,
//! `src/agent/local.rs`, is the one v0.0.63 shipped (`previous_release/v0_0_63_local.rs`), which
//! a test here holds equal. Both talk to a socket answering with this release's helper lines
//! (`kbf_host_install::contract::local`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_host_install::contract::local::{Answer, Code, QueryView};
use kbf_testkit::dir::TestDir;
use kbf_types::ReleasePlatform;
use kbf_updater::{inventory, ports, version, wire};

/// The crate's modules the shipped file reads, at the paths it reads them by, and the shipped
/// file itself as `agent::update`.
#[path = "previous_release/agent.rs"]
mod agent;

use agent::local::{Limits, SocketLocal};
use agent::status::{Shared, Status};
use inventory::{Host, Role};
use ports::{Clock, Drain, Health, HealthCheck, Placed, PortError};
use version::Version;
use wire::{Outcome, UpdateResult};

const VA: &str = "0.1.5+gaaaaaaaaaaaa-2";
const VB: &str = "0.1.7+gbbbbbbbbbbbb-3";
const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn version(text: &str) -> Version {
    Version::parse(text).unwrap()
}

/// Catches this release changing the agent's socket reader while the test below says it runs
/// the previous agent's: the reader is the file v0.0.63 shipped. (A change to `src/agent/local.rs`
/// keeps the shipped copy and compiles it here beside the shipped `update.rs`.)
#[test]
fn the_agents_socket_reader_is_the_previous_releases() {
    assert_eq!(
        include_str!("../src/agent/local.rs"),
        include_str!("previous_release/v0_0_63_local.rs")
    );
}

/// A helper on a socket in `dir`: each of `connections` requests is answered by `answer` of its
/// verb; the requests, in order, when it is done.
fn helper(
    dir: &TestDir,
    connections: usize,
    answer: impl Fn(&str) -> Answer + Send + 'static,
) -> (PathBuf, JoinHandle<Vec<String>>) {
    let socket = dir.join("h.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        let mut asked = Vec::new();
        for _ in 0..connections {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let line = line.trim_end_matches('\n').to_owned();
            let verb = line.split(' ').next().unwrap_or_default().to_owned();
            let text = format!("{}\n", answer(&verb).json_line());
            (&stream).write_all(text.as_bytes()).unwrap();
            asked.push(line);
        }
        asked
    });
    (socket, handle)
}

/// A node the frontend counts idle: in placement, drained on request, given back on request.
#[derive(Default)]
struct Node {
    drained: Mutex<bool>,
}

impl Drain for Node {
    fn placed(&self, _host: &Host) -> Result<Placed, PortError> {
        Ok(Placed::In)
    }
    fn drain(&self, _host: &Host, _deadline: Option<Duration>) -> Result<(), PortError> {
        *self.drained.lock().unwrap() = true;
        Ok(())
    }
    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Ok(0)
    }
    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        *self.drained.lock().unwrap() = false;
        Ok(())
    }
}

/// Every release runs healthy.
struct Healthy;

impl HealthCheck for Healthy {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }
    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Ok(Health::Healthy)
    }
    fn check_held(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Ok(Health::Healthy)
    }
}

/// A clock that moves only when slept on.
#[derive(Default)]
struct Manual(Mutex<Duration>);

impl Clock for Manual {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn sleep(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

/// Catches a `leases_live` refusal that strands an agent of the previous release, or that it
/// takes as anything but a refused switch: v0.0.63's update, over the previous reader, against
/// this helper refusing the switch `leases_live`, goes back as it does for any refused switch:
/// it asks `revert` (answered `nothing_pending`: nothing is on trial) and `query`, finds A
/// running healthy, gives the node back, and ends in a `Failed` of the target; the log names
/// the code.
#[test]
fn the_previous_agent_goes_back_on_a_leases_live_refusal_and_switches_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "prev-agent-leases-live");
    let (socket, server) = helper(&dir, 5, |verb| match verb {
        "install" => Answer::Staged {
            version: VB.into(),
            commit: COMMIT_B.into(),
            current: false,
        },
        "switch" => Answer::Refused(Code::LeasesLive, "1 leases are live".to_owned()),
        "revert" => Answer::Refused(Code::NothingPending, "nothing is on trial".to_owned()),
        _ => Answer::Queried(QueryView {
            commit: Some(COMMIT_A.into()),
            version: Some(VA.into()),
            node_id: Some("n1".into()),
            pending: None,
            last: None,
            busy: false,
            releases: Vec::new(),
            releases_omitted: 0,
            releases_unavailable: false,
        }),
    });
    let local = SocketLocal::new(
        &socket,
        Limits {
            install: Duration::from_secs(5),
            switch: None,
            revert: Duration::from_secs(5),
            other: Duration::from_secs(5),
        },
    );
    let node = Node::default();
    let clock = Manual::default();
    let logs = Mutex::new(Vec::new());
    let log = |line: &str| logs.lock().unwrap().push(line.to_owned());
    let unix_now = || 10_000;
    let host = Host {
        alias: "n1".to_owned(),
        address: "localhost".to_owned(),
        role: Role::MacDaemon,
        host_key: String::new(),
        node_id: Some("n1".to_owned()),
        member: None,
    };
    let status = Shared::new(Status::unknown(ReleasePlatform::DarwinArm64, version(VA)));
    let update = agent::update::Update {
        ports: agent::update::Ports {
            local: &local,
            drain: &node,
            health: &Healthy,
            clock: &clock,
            unix_now: &unix_now,
            log: &log,
        },
        host: &host,
        channel: "stable",
        timing: agent::flags::Timing::DEFAULT,
        status: &status,
    };
    assert_eq!(
        update.run(&version(VB)),
        UpdateResult::new(version(VB), Outcome::Failed)
    );
    assert_eq!(
        server.join().unwrap(),
        [
            "query".to_owned(),
            format!("install kbf={VB} from stable"),
            format!("switch kbf={VB}"),
            "revert".to_owned(),
            "query".to_owned(),
        ]
    );
    assert!(
        !*node.drained.lock().unwrap(),
        "A runs healthy: the node is given back"
    );
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|l| l.contains("the host helper refused: leases_live")),
        "{logs:?}"
    );
}
