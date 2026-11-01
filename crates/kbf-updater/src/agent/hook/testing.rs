//! What the hook's tests share: the agent's fake ports behind a [`HookWorld`], a record
//! directory, a params file as the updater writes it, and a driver that runs the steps in the
//! order the updater's table says (`update_hook::next`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kbf_types::ReleasePlatform;
use update_hook::{Context, Next, Outcome, Params, Report, Scalar, Step, Verdict, next};

use super::flags::split;
use super::record::{Gate, Record, Store};
use super::{HookWorld, Setup, run};
use crate::agent::flags::Timing;
use crate::agent::status::{Shared, Status};
use crate::agent::testing::{
    Events, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA, VB, host, version,
};
use crate::agent::update::{Ports, Update};
use crate::inventory::Role;
use crate::ports::{Drain, Health};
use crate::testutil::scratch;
use crate::wire::UpdateResult;

/// The digest the params carry.
pub const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Unix seconds "now" of every run; the fake helper's deadline is 600 s on.
pub const NOW: u64 = 10_000;

/// How one step ended.
pub struct Ran {
    pub outcome: Outcome,
    pub report: Report,
    /// Standard error.
    pub log: String,
}

impl Ran {
    /// The `reason` of the report.
    pub fn reason(&self) -> &str {
        self.report.reason.as_deref().unwrap_or_default()
    }

    pub fn logged(&self, needle: &str) -> bool {
        self.log.contains(needle)
    }
}

/// How the steps of one update went when the updater's table drove them.
pub struct Driven {
    pub steps: Vec<(Step, Outcome)>,
    pub verdict: Verdict,
}

/// A host that runs `VA`, offered `VB`, with its record directory and params file.
pub struct Rig {
    pub dir: PathBuf,
    pub events: Events,
    pub local: FakeLocal,
    pub drain: FakeDrain,
    pub health: FakeHealth,
    pub clock: Arc<ManualClock>,
    /// The drain port the world is given: [`Rig::drain`] unless a test replaces it.
    pub drain_port: Arc<dyn Drain>,
    /// The flags before the step.
    pub flags: Vec<String>,
    /// The params file's table, as `updater.*` keys.
    pub params: Params,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        let dir = scratch(name);
        std::fs::create_dir(dir.join("record")).expect("a record directory");
        std::fs::create_dir(dir.join("artifact")).expect("an artifact directory");
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        let drain = FakeDrain::new(&events);
        let mut params = Params::default();
        for (key, value) in [
            (
                "updater.staged_release",
                dir.join("artifact").display().to_string(),
            ),
            ("updater.target_version", VB.to_owned()),
            ("updater.artifact_digest", DIGEST.to_owned()),
        ] {
            params.insert(key, Scalar::Text(value));
        }
        // The certificate flags name files that are not there, in the rig's own directory: the
        // fake world never reads them, and the real wiring finds them missing on every host.
        let absent = |name: &str| dir.join("absent").join(name).display().to_string();
        let flags = vec![
            "--record-dir".to_owned(),
            dir.join("record").display().to_string(),
            "--channel".to_owned(),
            "stable".to_owned(),
            "--client-cert".to_owned(),
            absent("cert.pem"),
            "--client-key".to_owned(),
            absent("key.pem"),
            "--ca".to_owned(),
            absent("ca.pem"),
        ];
        Rig {
            drain_port: Arc::new(drain.clone()),
            local: FakeLocal::new(&events),
            health,
            drain,
            clock: Arc::new(ManualClock::default()),
            flags,
            params,
            events,
            dir,
        }
    }

    pub fn record_dir(&self) -> PathBuf {
        self.dir.join("record")
    }

    /// The record on disk, if there is one.
    pub fn record(&self) -> Option<Record> {
        Store::new(self.record_dir(), None)
            .load()
            .expect("a readable record")
    }

    /// The world the fakes make.
    pub fn world(&self) -> HookWorld {
        HookWorld {
            host: host(Role::Daemon),
            local: Arc::new(self.local.clone()),
            drain: self.drain_port.clone(),
            health: Arc::new(self.health.clone()),
            clock: self.clock.clone(),
            unix_now: Arc::new(|| NOW),
        }
    }

    /// The command line after `update-hook`.
    pub fn args(&self, step: Step) -> Vec<String> {
        let params = self.dir.join("params.toml");
        std::fs::write(&params, self.params.to_toml()).expect("a params file");
        let mut args = self.flags.clone();
        args.push(step.name().to_owned());
        if step == Step::Prepare {
            args.push(self.dir.join("artifact").display().to_string());
        }
        args.extend(["--params".to_owned(), params.display().to_string()]);
        args
    }

    /// Runs `step` as a process would.
    pub fn run(&self, step: Step) -> Ran {
        self.run_args(&self.args(step), None)
    }

    /// Runs `step` with `before_write` called before each write of the record.
    pub fn run_gated(&self, step: Step, before_write: Gate) -> Ran {
        self.run_args(&self.args(step), Some(before_write))
    }

    /// Runs this command line.
    pub fn run_args(&self, args: &[String], before_write: Option<Gate>) -> Ran {
        let world = |_: &super::flags::Flags| Ok(self.world());
        let setup = Setup {
            world: &world,
            before_write,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let outcome = run(args, &setup, &mut out, &mut err);
        let out = String::from_utf8(out).expect("text");
        let report = Report::from_stdout(&out).expect("a last line the strict reader accepts");
        Ran {
            outcome,
            report,
            log: String::from_utf8(err).expect("text"),
        }
    }

    /// The updater's table, driven to its verdict: `prepare` first, `retries` more tries of a
    /// failing `cleanup`.
    pub fn drive(&self) -> Driven {
        let mut ctx = Context::default();
        let mut step = Step::Prepare;
        let mut steps = Vec::new();
        loop {
            let ran = self.run(step);
            steps.push((step, ran.outcome));
            ctx.observe(step, &ran.report);
            match next(step, ran.outcome, &ran.report, &ctx) {
                Next::Run(to) | Next::Again { step: to, .. } => step = to,
                Next::Rollback { then } => {
                    ctx.then = Some(then);
                    step = Step::Rollback;
                }
                Next::Report(verdict) => return Driven { steps, verdict },
            }
        }
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.events.all()
    }

    /// The timing the flags give.
    pub fn timing(&self) -> Timing {
        let args = self.args(Step::Cutover);
        split(&args).expect("flags").0.timing
    }

    /// Today's update (`agent::update`) of this rig's host to `VB`, over the same fakes: the
    /// order of calls the hook's steps must make between them.
    pub fn today(&self) -> UpdateResult {
        let host = host(Role::Daemon);
        let status = Shared::new(Status::unknown(ReleasePlatform::LinuxX86_64, version(VA)));
        let log = |_: &str| {};
        let unix_now = || NOW;
        let update = Update {
            ports: Ports {
                local: &self.local,
                drain: &self.drain,
                health: &self.health,
                clock: &*self.clock,
                unix_now: &unix_now,
                log: &log,
            },
            host: &host,
            channel: "stable",
            timing: self.timing(),
            status: &status,
        };
        update.run(&version(VB))
    }
}

/// A kill: `before_write` that panics at its `n`th call (1 is the first), the way a process
/// killed at that write would stop.
pub fn kill_at(n: usize) -> Gate {
    let count = Mutex::new(0_usize);
    Arc::new(move || {
        let mut count = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count += 1;
        assert!(*count != n, "killed at write {n}");
    })
}
