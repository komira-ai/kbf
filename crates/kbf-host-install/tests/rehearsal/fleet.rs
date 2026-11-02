//! Stand-in storage hosts for the rehearsal of tierstore as a service: each is a node of the
//! storage role that heartbeats the real coordinator over mutual TLS with a certificate of the
//! farm CA and announces `tierstore` among its services, as the updater agent does with
//! `--service tierstore,...`. A host obeys the coordinator's order the way the agent's A/B
//! ends: the beat after the order says it is updating, the beat after that says it runs the
//! target and how the update ended. Nothing is installed; the host only says so.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::farm::HEARTBEAT;
use super::run;

/// The service the hosts announce.
pub const SERVICE: &str = "tierstore";

/// The `kbf` the hosts say they run: they are kbf-less stand-ins, and `kbf` has no desired
/// version in this rehearsal.
const KBF_VERSION: &str = "0.0.1";

/// One stand-in host.
pub struct Host {
    pub node: String,
    cert: PathBuf,
    key: PathBuf,
    /// The version of the service it says it runs.
    pub running: String,
    updating: Option<String>,
    /// How the last update ended, as the host reports it.
    pub last_update: Option<Value>,
    /// The version of every order for the service the host has been given, in order.
    pub orders: Vec<String>,
}

impl Host {
    /// A host `node` that runs `running`, with its certificate and key.
    pub fn new(node: &str, cert: (PathBuf, PathBuf), running: &str) -> Self {
        Self {
            node: node.to_owned(),
            cert: cert.0,
            key: cert.1,
            running: running.to_owned(),
            updating: None,
            last_update: None,
            orders: Vec::new(),
        }
    }

    /// The heartbeat the host sends now.
    fn heartbeat(&self) -> Value {
        json!({
            "platform": "linux-x86_64",
            "version": KBF_VERSION,
            "health": "healthy",
            "agent_features": ["heartbeat-v2"],
            "services": [{
                "name": SERVICE,
                "version": self.running,
                "health": "healthy",
                "updating": self.updating,
                "last_update": self.last_update,
            }],
        })
    }

    /// One beat: sends the heartbeat and acts on the answer. An update that began at the last
    /// beat ends now, as a failure when `refuses` is the version it targets. An order for
    /// the service begins one.
    fn beat(&mut self, ca: &Path, refuses: Option<&str>) {
        let port = HEARTBEAT.rsplit(':').next().unwrap();
        let said = run(
            &format!("beat_{}", self.node),
            Command::new("curl")
                .args(["--silent", "--show-error", "--fail-with-body", "-X", "POST"])
                // A bound on the one request, so a wedged coordinator fails the call with
                // curl's diagnostic.
                .args(["--max-time", "30"])
                .args(["-H", "Content-Type: application/json"])
                .arg("--cacert")
                .arg(ca)
                .arg("--cert")
                .arg(&self.cert)
                .arg("--key")
                .arg(&self.key)
                .arg("--data")
                .arg(self.heartbeat().to_string())
                .arg(format!("https://localhost:{port}/v1/heartbeat")),
        );
        let answer: Value = serde_json::from_str(&said).unwrap();
        if let Some(target) = self.updating.take() {
            let succeeded = refuses != Some(target.as_str());
            if succeeded {
                self.running.clone_from(&target);
            }
            let outcome = if succeeded { "succeeded" } else { "failed" };
            self.last_update = Some(json!({"target": target, "outcome": outcome}));
        } else if answer["reply"] == "update" && answer["service"] == SERVICE {
            self.updating = answer["version"].as_str().map(str::to_owned);
            self.orders.extend(self.updating.clone());
        }
    }
}

/// The hosts of the rehearsal's farm.
pub struct Fleet {
    pub hosts: Vec<Host>,
    /// The most hosts that were updating at the end of any round so far.
    pub peak_updating: usize,
}

impl Fleet {
    /// A fleet of `hosts`.
    pub fn new(hosts: Vec<Host>) -> Self {
        Self {
            hosts,
            peak_updating: 0,
        }
    }

    /// Every host beats once, in order, and the number updating after the round is noted.
    pub fn round(&mut self, ca: &Path, refuses: Option<&str>) {
        for host in &mut self.hosts {
            host.beat(ca, refuses);
        }
        let updating = self.hosts.iter().filter(|h| h.updating.is_some()).count();
        self.peak_updating = self.peak_updating.max(updating);
    }

    /// The versions the hosts run, in order.
    pub fn running(&self) -> Vec<&str> {
        self.hosts.iter().map(|h| h.running.as_str()).collect()
    }
}
