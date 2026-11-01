//! What the client's tests share: a client over a scratch directory, with helpers to drive
//! and read it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::*;
use crate::fixtures::{PLATFORM, offer_of, scratch};
use crate::helper::SCHEMA;

pub(super) struct Rig {
    pub(super) handle: Handle,
    pub(super) rx: watch::Receiver<Status>,
    pub(super) task: JoinHandle<()>,
    pub(super) dir: PathBuf,
}

impl Rig {
    /// A client over a host whose installer runs and has nothing to report.
    pub(super) fn new(name: &str) -> Self {
        let rig = Self::bare(name);
        rig.write_status(&status_json("idle", "", "", 0));
        rig
    }

    /// A client over a host with no status file.
    pub(super) fn bare(name: &str) -> Self {
        Self::over(&scratch(name))
    }

    pub(super) fn config(dir: &Path) -> Config {
        let mut config = Config::new(dir.to_owned(), PLATFORM.to_owned(), "f".repeat(12));
        config.poll_every = Duration::from_secs(3600);
        config
    }

    pub(super) fn over(dir: &Path) -> Self {
        let (handle, worker) = start(Self::config(dir)).expect("start");
        Self {
            rx: handle.status(),
            handle,
            task: tokio::spawn(worker.run()),
            dir: dir.to_owned(),
        }
    }

    /// A new client over the same directory, as after a restart.
    pub(super) fn restart(self) -> Self {
        self.task.abort();
        Self::over(&self.dir)
    }

    pub(super) fn spool(&self) -> PathBuf {
        self.dir.join("orders")
    }

    /// The names in the order spool other than the client's own state file, sorted.
    pub(super) fn orders(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.spool())
            .expect("spool")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with("client.json"))
            .collect();
        names.sort();
        names
    }

    pub(super) fn write_status(&self, json: &str) {
        fs::write(self.dir.join("status.json"), json).expect("write status.json");
        self.handle.refresh();
    }

    /// Offers generation `generation` (commit `commit(generation)`).
    pub(super) fn offer(&self, generation: u64) -> Offer {
        let offer = offer_of(generation);
        self.handle.offer(offer.clone());
        offer
    }
}

pub(super) fn status_json(state: &str, installed: &str, staged: &str, acked: u64) -> String {
    let quote = |c: &str| {
        if c.is_empty() {
            "null".to_owned()
        } else {
            format!("\"{c}\"")
        }
    };
    format!(
        "{{\"schema\":{SCHEMA},\"installed_commit\":{},\"previous_commit\":null,\
         \"staged_commit\":{},\"state\":\"{state}\",\"refusal\":\"why not\",\
         \"acked_order_id\":{acked},\"staged\":{{\"protocol\":2,\"min_protocol\":1,\
         \"state_schema\":3}}}}",
        quote(installed),
        quote(staged)
    )
}

pub(super) fn order(id: u64, verb: Verb, commit: &str) -> Order {
    Order {
        order_id: id,
        epoch: 7,
        generation: 1,
        verb: Some(verb),
        commit: commit.to_owned(),
        window_s: 600,
        source: Some(Source::Fetch),
    }
}

/// An activation of `commit` from the release the host's package carries.
pub(super) fn staged_order(id: u64, commit: &str) -> Order {
    Order {
        source: Some(Source::Staged),
        ..order(id, Verb::Activate, commit)
    }
}

/// `status` with one more key, as the installer writes them.
pub(super) fn with_key(status: &str, key_and_value: &str) -> String {
    let mut text = status.to_owned();
    text.pop();
    format!("{text},{key_and_value}}}")
}

pub(super) const NO_LEASES: &dyn Fn() -> usize = &|| 0;

impl Rig {
    /// Asks for order `id` of `verb` for `commit` with no lease live.
    pub(super) fn ask(&self, id: u64, verb: Verb, commit: &str) -> Decision {
        self.handle.order(&order(id, verb, commit), 7, NO_LEASES)
    }
}

/// The reason a decision refused, or "" when it did not.
pub(super) fn why(decision: &Decision) -> &str {
    decision.refusal().unwrap_or_default()
}
