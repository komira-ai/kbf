//! The pure parts of the `deploy-farm` fakes: no sockets, no TLS. `deploy_fake.rs` re-exports them.
#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use serde_json::{Value, json};
use xtask::deploy_farm::{Clock, Plan};

pub const RUNNER_KEY: &str = "runner-key";

/// The plan of a run against the coordinator at `base`.
pub fn plan_for(base: &str) -> Plan {
    Plan {
        url: base.to_owned(),
        service: "kbf".to_owned(),
        audience: base.to_owned(),
        version: "2.0.0+g000000000002".to_owned(),
        min_up_storage: 1,
        min_up_frontend: 1,
        require_coordinator_current: false,
        deadline: Duration::from_secs(30),
        stall: Duration::from_secs(30),
        every: Duration::from_secs(1),
    }
}

pub fn farm(state: &str, updating: u64, failed: u64) -> Value {
    json!({"desired": "2.0.0+g000000000002", "quiet": false, "state": state, "roles": [
        {"role": "storage", "at_desired": 2, "updating": updating, "failed": failed, "down": 0},
        {"role": "daemon", "at_desired": 1, "updating": 0, "failed": 0, "down": 1},
    ]})
}

/// A virtual clock: `sleep` moves `now` and is recorded, so no test waits for a deadline.
#[derive(Default)]
pub struct Time {
    pub now: Cell<Duration>,
    pub slept: RefCell<Vec<Duration>>,
}

impl Time {
    pub fn clock(&self) -> Clock<'_> {
        Clock {
            now: Box::new(|| self.now.get()),
            sleep: Box::new(|d| {
                self.now.set(self.now.get() + d);
                self.slept.borrow_mut().push(d);
            }),
        }
    }
}
