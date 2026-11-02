//! The steps of a `gamma` job against the stateful fake coordinator, shared by the restore's
//! test files: the deploy, the restore, and what they are expected to set.
//!
//! A test file includes it after `coordinator_fake` and `deploy_fake`.
#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::time::Duration;

use serde_json::{Value, json};
use xtask::deploy_farm::restore::{self, Error, restore_on};
use xtask::deploy_farm::{Error as Deploy, Plan, Roots, TokenService, deploy_on};

use super::coordinator_fake::{BEFORE, Fake, RELEASE, State};
use super::deploy_fake::{RUNNER_KEY, Time};

pub const RUN: &str = "4711-2";
pub const WRITE_ID: &str = "restore-4711-2";

pub fn token_service(base: &str) -> TokenService {
    TokenService {
        url: format!("{base}/token?api-version=2.0"),
        key: RUNNER_KEY.to_owned(),
    }
}

pub fn plan(fake: &Fake) -> Plan {
    Plan {
        url: fake.base.clone(),
        service: "kbf".to_owned(),
        audience: fake.base.clone(),
        version: RELEASE.to_owned(),
        min_up_storage: 1,
        min_up_frontend: 1,
        require_coordinator_current: false,
        deadline: Duration::from_secs(30),
        stall: Duration::from_secs(30),
        every: Duration::from_secs(1),
    }
}

/// The deploy step of the job, by `plan`.
pub fn deploy_with(fake: &Fake, plan: &Plan) -> Result<(), Deploy> {
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let mut out = Vec::new();
    deploy_on(
        plan,
        &token_service(&fake.base),
        &roots,
        &Time::default().clock(),
        &mut out,
    )
}

pub fn deploy(fake: &Fake) -> Result<(), Deploy> {
    deploy_with(fake, &plan(fake))
}

pub fn args(base: &str) -> restore::Args {
    restore::Args {
        url: base.to_owned(),
        version: RELEASE.to_owned(),
        service: "kbf".to_owned(),
        ca_pem: String::new(),
        audience: None,
        run: RUN.to_owned(),
        every_secs: 1,
    }
}

/// The restore step of the job: its result, its log, and what it slept.
pub fn restore_timed(fake: &Fake, args: &restore::Args) -> (Result<String, Error>, String, Time) {
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let time = Time::default();
    let mut out = Vec::new();
    let result = restore_on(
        args,
        &token_service(&fake.base),
        &roots,
        &time.clock(),
        &mut out,
    );
    (result, String::from_utf8(out).expect("utf8"), time)
}

pub fn restore(fake: &Fake) -> (Result<String, Error>, String) {
    let (result, out, _) = restore_timed(fake, &args(&fake.base));
    (result, out)
}

pub fn restored() -> Result<String, Error> {
    Err(Error::Restored {
        version: BEFORE.to_owned(),
        release: RELEASE.to_owned(),
    })
}

/// The body of the restore's POST of `BEFORE` and its minimums, on `generation`.
pub fn restore_body(generation: u64) -> Value {
    json!({
        "version": BEFORE,
        "min_up": {"storage": 2, "frontend": 1},
        "if_generation": generation,
        "write_id": WRITE_ID,
    })
}

/// The farm set back to `BEFORE` by the restore, on top of `generation`.
pub fn set_back(generation: u64) -> State {
    State {
        version: BEFORE.to_owned(),
        min_up: json!({"storage": 2, "frontend": 1}),
        generation: generation + 1,
        replaced: Some((RELEASE.to_owned(), json!({"storage": 1, "frontend": 1}))),
        write_id: WRITE_ID.to_owned(),
    }
}

pub fn versions(fake: &Fake) -> Vec<String> {
    fake.posts()
        .iter()
        .map(|p| p.body["version"].as_str().expect("version").to_owned())
        .collect()
}
