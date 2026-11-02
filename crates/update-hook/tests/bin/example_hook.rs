//! `update-hook-example`: a small update hook, the program the conformance harness is proven
//! against (tests/conformance.rs), and with a switch for each way a hook can break the contract.
//!
//! The service is a blue/green pair in a directory (`EXAMPLE_WORLD`): a link `current` to the
//! live version, and while an update runs a staged `next` and a `cordon`. `cleanup` is the
//! commit: it retires the old version (`retired`) and says `reversible: false`. With
//! `--irreversible-cutover` the commit is `cutover` instead: it stops the old version
//! (`stopped`) before it moves the link, as the js-agent's hook stops its agent, and a rollback
//! can repair the link but not give back what was lost.
//!
//! The hook's record is a file per key in `EXAMPLE_RECORD`. Every change to the world or the
//! record passes a kill point first. Faults come from `EXAMPLE_FAULTS/<step>` (`fail`, `defer`,
//! `halt`) and take effect when a step has done its work. `EXAMPLE_OUTSIDE` is the service's own
//! data directory, which the hook must never write.
//!
//! A `--behaviour=<name>` (repeatable) is a behaviour a hook may show for safety and that the
//! harness accepts only when the test declares it (`conformance::Allowances`), with the
//! variants that stay refused even then:
//!
//! - `forgets-commit`: `rollback` also removes the `stop_begun` tombstone, so a rerun that finds
//!   no record says `reversible: true` where the first said `false`. With it,
//!   `rollback-rerun-fails`: such a rerun fails instead; `rollback-rerun-touches-world` and
//!   `rollback-rerun-touches-record`: such a rerun also writes `rerun` in the world or the
//!   record.
//! - `cordon-unowned`: `prepare` records the cordon as its own (`cordon_mine`) right after
//!   placing it, and a run that finds a cordon it has not recorded takes it for an operator's:
//!   it neither rewrites nor lifts it. `cordon-recorded-late`: it records the cordon two
//!   changes later. `cordon-unowned-noted`: as `cordon-unowned`, and it also notes the foreign
//!   cordon in the world. `cordon-recorded-first`: it records the cordon before placing it, but
//!   a run that finds a cordon and no `prepared` takes it for an operator's anyway and drops its
//!   record. `cordon-rooted`, with one of these: the cordon holds the world's path, which
//!   differs from sandbox to sandbox.
//! - `first-install-no-rollback`: with no previous release in the params, `rollback` after
//!   `cutover` fails with `no_previous_release`, says `reversible: false` and changes nothing.
//!   `no-rollback-after-cutover`: it does so with a previous release too. Variants of the first
//!   (every run): `first-install-no-rollback-busy` says `agent_busy`;
//!   `first-install-defers-rollback` and `first-install-halts-rollback` exit `75` and `76`
//!   instead; `first-install-no-rollback-undoes` and `first-install-no-rollback-undoes-record`
//!   take `next` from the world or `target` from the record first. Variants of every run after
//!   the first in one harness: `first-install-no-rollback-then-busy` says `agent_busy`;
//!   `first-install-no-rollback-once` rolls back; `first-install-no-rollback-rerun-undoes` and
//!   `first-install-no-rollback-rerun-undoes-record` take `next` or `target` first;
//!   `first-install-no-rollback-rerun-flips` says `reversible: true`.
//!
//! Command line: `update-hook [--irreversible-cutover] [--defect=<name>] [--behaviour=<name>]...
//! <step> [args]`, or the step first when the first argument is not `update-hook`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use update_hook::conformance::gate;
use update_hook::{Class, Invocation, Outcome, Params, Report, Step};

/// How a `rollback` declines: its end, its reason, its `reversible`, and what it takes away
/// first (a directory and a name).
struct Decline<'a> {
    outcome: Outcome,
    reason: &'static str,
    reversible: bool,
    takes: Option<(&'a PathBuf, &'static str)>,
}

struct Hook {
    world: PathBuf,
    record: PathBuf,
    outside: PathBuf,
    faults: PathBuf,
    irreversible: bool,
    defect: String,
    behaviours: Vec<String>,
    /// Skip kill points (the whole hook, or just `cutover`).
    ungated: bool,
    /// The params file, to overwrite it for a defect.
    params_file: PathBuf,
    /// This is a late run of the `flaky-gates` hook: it skips a kill point.
    late: bool,
}

fn var(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_default())
}

impl Hook {
    fn defect(&self, name: &str) -> bool {
        self.defect == name
    }

    fn behaves(&self, name: &str) -> bool {
        self.behaviours.iter().any(|b| b == name)
    }

    /// Whether the hook keeps a record of its own cordon, and lifts only that one.
    fn owns_cordons(&self) -> bool {
        [
            "cordon-unowned",
            "cordon-recorded-late",
            "cordon-unowned-noted",
            "cordon-recorded-first",
        ]
        .iter()
        .any(|name| self.behaves(name))
    }

    /// Lifts the cordon if it is the hook's own (always, for a hook that keeps no record of it).
    fn lift_cordon(&self) -> io::Result<()> {
        if self.owns_cordons() {
            if self.rec_get("cordon_mine").is_none() {
                return Ok(());
            }
            self.take(&self.world, "cordon")?;
            return self.take(&self.record, "cordon_mine");
        }
        self.take(&self.world, "cordon")
    }

    /// How `rollback` declines to stop what `cutover` started, if it does.
    fn declines_rollback(&self, params: &Params) -> Option<Decline<'_>> {
        // Nothing to decline before `cutover`.
        self.rec_get("cutover")?;
        let first_install = params.previous_release().is_none();
        let declines = self.behaves("no-rollback-after-cutover")
            || (first_install
                && self
                    .behaviours
                    .iter()
                    .any(|b| b.starts_with("first-install-")));
        if !declines {
            return None;
        }
        let mut decline = Decline {
            outcome: Outcome::Failure,
            reason: "no_previous_release",
            reversible: false,
            takes: None,
        };
        if self.behaves("first-install-no-rollback-busy") {
            decline.reason = "agent_busy";
        }
        if self.behaves("first-install-defers-rollback") {
            decline.outcome = Outcome::Deferred;
        }
        if self.behaves("first-install-halts-rollback") {
            decline.outcome = Outcome::Halt;
        }
        if self.behaves("first-install-no-rollback-undoes") {
            decline.takes = Some((&self.world, "next"));
        }
        if self.behaves("first-install-no-rollback-undoes-record") {
            decline.takes = Some((&self.record, "target"));
        }
        if self.later_run() {
            if self.behaves("first-install-no-rollback-once") {
                return None;
            }
            if self.behaves("first-install-no-rollback-then-busy") {
                decline.reason = "agent_busy";
            }
            if self.behaves("first-install-no-rollback-rerun-undoes") {
                decline.takes = Some((&self.world, "next"));
            }
            if self.behaves("first-install-no-rollback-rerun-undoes-record") {
                decline.takes = Some((&self.record, "target"));
            }
            if self.behaves("first-install-no-rollback-rerun-flips") {
                decline.reversible = true;
            }
        }
        Some(decline)
    }

    /// Whether a declining `rollback` has declined before in this harness: a mark beside the
    /// sandboxes, so that it counts across them and is no change of theirs.
    fn later_run(&self) -> bool {
        let seen = self.faults.join("../../declined");
        let later = seen.exists();
        let _ = fs::write(&seen, "1");
        later
    }

    /// A kill point, unless the hook is the one that never has any.
    fn gate(&self) {
        if !self.ungated {
            gate::point();
        }
    }

    fn put(&self, dir: &Path, name: &str, value: &str) -> io::Result<()> {
        self.gate();
        fs::write(dir.join(name), value)
    }

    fn take(&self, dir: &Path, name: &str) -> io::Result<()> {
        self.gate();
        match fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn world_set(&self, name: &str, value: &str) -> io::Result<()> {
        self.put(&self.world, name, value)
    }

    fn world_has(&self, name: &str) -> bool {
        self.world.join(name).exists()
    }

    fn rec_set(&self, key: &str, value: &str) -> io::Result<()> {
        if self.late && key == "prepared" {
            return fs::write(self.record.join(key), value);
        }
        self.put(&self.record, key, value)
    }

    fn rec_get(&self, key: &str) -> Option<String> {
        fs::read_to_string(self.record.join(key)).ok()
    }

    /// How many times the harness has started this program, this time included.
    fn runs(&self) -> usize {
        // Beside the sandboxes, so that it counts across them.
        let file = self.faults.join("../../flaky-runs");
        let n = fs::read_to_string(&file).map_or(0, |t| t.len()) + 1;
        let _ = fs::write(&file, ".".repeat(n));
        n
    }

    fn current(&self) -> String {
        fs::read_link(self.world.join("current"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Points `current` at `version`: a new link, renamed over the old.
    fn set_current(&self, version: &str) -> io::Result<()> {
        self.gate();
        let tmp = self.world.join("current.new");
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp)?;
        fs::rename(&tmp, self.world.join("current"))
    }

    fn fault(&self, step: Step) -> Option<String> {
        if self.defect("ignores-faults") {
            return None;
        }
        fs::read_to_string(self.faults.join(step.name())).ok()
    }

    /// The step's end: the injected fault if there is one, else success.
    fn finish(&self, step: Step, mut report: Report) -> (Outcome, Report) {
        match self.fault(step).as_deref() {
            Some("fail") => (Outcome::Failure, report),
            Some("defer") => {
                if !self.defect("defer-no-retry") {
                    report.retry_after_secs = Some(30);
                }
                (Outcome::Deferred, report)
            }
            Some("halt") => (Outcome::Halt, report),
            _ => (Outcome::Success, report),
        }
    }

    fn lost(&self) -> bool {
        self.rec_get("stop_begun").is_some()
    }

    fn run(&self, inv: &Invocation) -> io::Result<(Outcome, Report)> {
        let step = inv.step;
        let mut report = Report::default();
        if !self.defect("ignores-lock") {
            let record = File::open(&self.record)?;
            if record.try_lock().is_err() {
                if self.defect("writes-when-locked") {
                    fs::write(self.world.join("intruder"), "1")?;
                }
                report.detail = Some("another copy of this step is running".to_owned());
                return Ok((Outcome::Failure, report));
            }
            // Held to the end of the process.
            std::mem::forget(record);
        }
        // Every step gets the same params file: the staged release and the target version.
        let params = Params::read(&inv.params.clone().unwrap_or_default())
            .map_err(|e| io::Error::other(e.to_string()))?;
        if params.staged_release().is_none() || params.target_version().is_none() {
            report.detail = Some("the params lack the staged release or the target".to_owned());
            return Ok((Outcome::Failure, report));
        }
        if !matches!(params.artifact_digest(), Ok(Some(_))) {
            report.detail = Some("the params lack a well-formed artifact digest".to_owned());
            return Ok((Outcome::Failure, report));
        }
        if self.defect("needs-previous") && params.previous_release().is_none() {
            report.detail = Some("no previous release: a first install".to_owned());
            return Ok((Outcome::Failure, report));
        }
        if self.defect("refuses-unknown-updater-key")
            && params.get("updater.conformance_probe").is_some()
        {
            report.detail = Some("unknown updater.* key".to_owned());
            return Ok((Outcome::Failure, report));
        }
        if step == Step::Prepare && inv.artifact != params.staged_release() {
            report.detail =
                Some("the artifact is not the staged release, byte for byte".to_owned());
            return Ok((Outcome::Failure, report));
        }
        match step {
            Step::Prepare => {
                let artifact = inv.artifact.clone().unwrap_or_default();
                let target = fs::read_to_string(artifact.join("VERSION"))?;
                // A cordon there and not recorded as the hook's own is an operator's (for
                // `cordon-recorded-first`, wrongly: one there before `prepared`).
                let first = self.behaves("cordon-recorded-first");
                let unrecorded = if first {
                    self.rec_get("prepared").is_none()
                } else {
                    self.rec_get("cordon_mine").is_none()
                };
                let foreign = self.owns_cordons() && self.world_has("cordon") && unrecorded;
                self.world_set("next", target.trim())?;
                if first && !foreign {
                    self.rec_set("cordon_mine", "1")?;
                }
                if foreign {
                    if first {
                        self.take(&self.record, "cordon_mine")?;
                    }
                } else {
                    let world = self.world.to_string_lossy().into_owned();
                    let cordon = if self.behaves("cordon-rooted") {
                        world.as_str()
                    } else {
                        "1"
                    };
                    self.world_set("cordon", cordon)?;
                }
                if foreign && self.behaves("cordon-unowned-noted") {
                    self.world_set("foreign-cordon", "1")?;
                }
                let early = !foreign && !first && !self.behaves("cordon-recorded-late");
                if self.owns_cordons() && early {
                    self.rec_set("cordon_mine", "1")?;
                }
                self.rec_set("target", target.trim())?;
                self.rec_set("previous", &self.current())?;
                if self.behaves("cordon-recorded-late") && !foreign {
                    self.rec_set("cordon_mine", "1")?;
                }
                self.rec_set("prepared", "1")?;
                if self.defect("writes-outside") {
                    fs::write(self.outside.join("state.json"), "{}")?;
                }
            }
            Step::Cutover => {
                if self.rec_get("prepared").is_none() {
                    report.detail = Some("not prepared".to_owned());
                    return Ok((Outcome::Failure, report));
                }
                let target = self.rec_get("target").unwrap_or_default();
                let previous = self.rec_get("previous").unwrap_or_default();
                if self.defect("record-grows") {
                    let mut log = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.record.join("log"))?;
                    writeln!(log, "cutover")?;
                }
                if self.defect("resume-diverges-record") {
                    let resumed =
                        self.rec_get("marker").is_some() && self.rec_get("cutover").is_none();
                    self.rec_set("marker", if resumed { "resumed" } else { "fresh" })?;
                }
                if self.defect("writes-control") {
                    fs::write(&self.params_file, "")?;
                }
                if self.defect("writes-root") {
                    fs::write(self.record.join("../stray"), "1")?;
                }
                if self.defect("non-idempotent") {
                    let mut log = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.world.join("log"))?;
                    writeln!(log, "cutover")?;
                }
                if self.defect("resume-diverges") {
                    let resumed = self.world_has("marker") && self.rec_get("cutover").is_none();
                    self.world_set("marker", if resumed { "resumed" } else { "fresh" })?;
                }
                if self.irreversible {
                    self.rec_set("stop_begun", "1")?;
                    self.world_set("stopped", &previous)?;
                }
                self.set_current(&target)?;
                self.rec_set("cutover", "1")?;
                report.reversible = !self.irreversible;
            }
            Step::Verify => {
                if self.defect("signal") {
                    // SAFETY: raise(3) of SIGKILL on this process; no memory is touched.
                    unsafe { libc::raise(libc::SIGKILL) };
                }
                let target = self.rec_get("target").unwrap_or_default();
                if self.rec_get("cutover").is_none() || self.current() != target {
                    report.detail = Some("the new version is not the live one".to_owned());
                    report.class = Class::Release;
                    return Ok((Outcome::Failure, report));
                }
                self.rec_set("verified", "1")?;
                report.reversible = !self.lost();
            }
            Step::Rollback => {
                if let Some(decline) = self.declines_rollback(&params) {
                    if let Some((dir, name)) = decline.takes {
                        self.take(dir, name)?;
                    }
                    report.reason = Some(decline.reason.to_owned());
                    report.reversible = decline.reversible;
                    if decline.outcome == Outcome::Deferred {
                        report.retry_after_secs = Some(30);
                    }
                    return Ok((decline.outcome, report));
                }
                let rerun = self.rec_get("prepared").is_none();
                if self.behaves("rollback-rerun-fails") && rerun {
                    report.detail = Some("no record to roll back".to_owned());
                    return Ok((Outcome::Failure, report));
                }
                if self.behaves("rollback-rerun-touches-world") && rerun {
                    self.world_set("rerun", "1")?;
                }
                if self.behaves("rollback-rerun-touches-record") && rerun {
                    self.rec_set("rerun", "1")?;
                }
                let lost = self.lost();
                if self.defect("rollback-diverges") {
                    let resumed = self.world_has("rb-marker") && self.rec_get("previous").is_some();
                    self.world_set("rb-marker", if resumed { "resumed" } else { "fresh" })?;
                }
                if let Some(previous) = self.rec_get("previous") {
                    self.set_current(&previous)?;
                }
                self.take(&self.world, "next")?;
                if !self.defect("leaves-cordon") && !self.defect("rollback-says-irreversible") {
                    self.lift_cordon()?;
                }
                // The tombstone `stop_begun` stays, so that every rollback says the same, unless
                // the hook forgets the commit with the rest of its record.
                for key in ["prepared", "cutover", "verified", "target", "previous"] {
                    self.take(&self.record, key)?;
                }
                if self.behaves("forgets-commit") {
                    self.take(&self.record, "stop_begun")?;
                }
                report.reversible = self.defect("reversible-lie")
                    || (!lost && !self.defect("rollback-says-irreversible"));
            }
            Step::Cleanup => {
                let seen = self.world_has("retired");
                if self.rec_get("verified").is_none() && !self.defect("cleanup-commits") {
                    report.detail = Some("verify has not succeeded".to_owned());
                    return Ok((Outcome::Failure, report));
                }
                if self.defect("second-run-fails") && self.world_has("retired") {
                    report.detail = Some("already retired".to_owned());
                    return Ok((Outcome::Failure, report));
                }
                let previous = self.rec_get("previous").unwrap_or_else(|| "v1".to_owned());
                self.world_set("retired", &previous)?;
                self.take(&self.world, "next")?;
                if !self.defect("cleanup-leaves-cordon") {
                    self.lift_cordon()?;
                }
                report.reversible = self.defect("reversible-flips") && seen;
            }
        }
        Ok(self.finish(step, report))
    }
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "update-hook") {
        args.remove(0);
    }
    let mut hook = Hook {
        world: var("EXAMPLE_WORLD"),
        record: var("EXAMPLE_RECORD"),
        outside: var("EXAMPLE_OUTSIDE"),
        faults: var("EXAMPLE_FAULTS"),
        irreversible: false,
        defect: String::new(),
        behaviours: Vec::new(),
        late: false,
        ungated: false,
        params_file: PathBuf::new(),
    };
    while let Some(flag) = args.first().and_then(|a| a.to_str()) {
        if flag == "--irreversible-cutover" {
            hook.irreversible = true;
        } else if let Some(name) = flag.strip_prefix("--defect=") {
            hook.defect = name.to_owned();
        } else if let Some(name) = flag.strip_prefix("--behaviour=") {
            hook.behaviours.push(name.to_owned());
        } else {
            break;
        }
        args.remove(0);
    }
    hook.late = hook.defect("flaky-gates") && hook.runs() > 4;
    let inv = match Invocation::parse(args) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("update-hook-example: {e}");
            return ExitCode::from(2);
        }
    };
    hook.ungated =
        hook.defect("ungated") || (hook.defect("ungated-cutover") && inv.step == Step::Cutover);
    hook.params_file = inv.params.clone().unwrap_or_default();
    let (outcome, report) = match hook.run(&inv) {
        Ok(end) => end,
        Err(e) => {
            eprintln!("update-hook-example: {}: {e}", inv.step);
            let report = Report {
                detail: Some(e.to_string()),
                ..Report::default()
            };
            (Outcome::Failure, report)
        }
    };
    // The defects that break how a step ends rather than what it does.
    if inv.step == Step::Verify {
        match hook.defect.as_str() {
            "no-line-on-success" => return ExitCode::SUCCESS,
            "exit-2" => {
                eprintln!("update-hook-example: giving up with 2");
                return ExitCode::from(2);
            }
            "bad-line" => {
                println!("{{\"class\":");
                return outcome.into();
            }
            _ => {}
        }
    }
    let _ = report.write_to(&mut io::stdout().lock());
    outcome.into()
}
