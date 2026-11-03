//! The start pass of the root part (docs/design/host-updates.md, 3.5): the read of every step
//! record after a boot, the start of each service (an unfinished update run to its verdict, a
//! declaration installed for an update that never began put back), and the boot entry.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use kbf_step_runner::record::{Record, RecordStore, State};
use kbf_step_runner::runner::REASON_ARTIFACT_CHANGED;
use kbf_step_runner::{Finished, StepLauncher, UpdateOrder};
use update_hook::Verdict;
use update_rights::{boot_unit_name, check_state_dir, run_boot};

use super::launch::declared;
use super::socket::Answer;
use super::{
    Refusal, Resumed, RootPart, Service, because, boot, committed, declare, make_record_dir,
    record_generation, resumed, run_refusal, stored,
};

/// The records a read after a boot has not read yet.
#[derive(Debug, Default)]
pub(super) struct Unread {
    /// The state directory could not be listed: every service is unread until it is read.
    all: bool,
    services: BTreeSet<String>,
    /// Read since, when `all`.
    read: BTreeSet<String>,
}

impl Unread {
    fn holds(&self, service: &str) -> bool {
        self.services.contains(service) | (self.all & !self.read.contains(service))
    }
}

impl RootPart {
    /// After a boot, reads every step record under the state directory with
    /// [`boot::read_as_killed`]: those of the declared services and those of services whose
    /// declaration the load refused, which an update may have left unfinished all the same. Run
    /// before the start pass and before the agent is served, so no update of a service is running.
    /// The services whose record could not be read, and why (an empty name when the directory
    /// itself could not be listed). Each stays unread, and nothing of it is resumed or updated,
    /// until a later request reads it ([`RootPart::update`], [`RootPart::start`]).
    pub fn read_after_boot(&self) -> Vec<(String, String)> {
        let boot_id = self.parts.clock.boot_id();
        let mut unread = Vec::new();
        // The state directory is made when the root part is (`RootPart::new`).
        let listed = fs::read_dir(&self.config.state)
            .and_then(|entries| entries.collect::<io::Result<Vec<_>>>());
        match listed {
            Ok(entries) => {
                for entry in entries {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    if !is_dir || !update_rights::names::is_service_name(&name) {
                        continue;
                    }
                    let store = RecordStore::new(&entry.path());
                    if let Err(e) = boot::read_as_killed(&store, &boot_id) {
                        unread.push((name, e.to_string()));
                    }
                }
            }
            Err(e) => unread.push((String::new(), e.to_string())),
        }
        let mut held = self.unread.lock().unwrap_or_else(PoisonError::into_inner);
        for (name, _) in &unread {
            if name.is_empty() {
                held.all = true;
            } else {
                held.services.insert(name.clone());
            }
        }
        unread
    }

    /// Puts back, for every service whose state directory keeps a declaration installed for an
    /// update that never began (the record still has the generation it had at the install), the
    /// one that was active, then loads the declarations again. It reads only the root part's own
    /// bookkeeping, so a service whose installed declaration the load refuses (an account the host
    /// does not have) gets its valid one back. Run at the start, before the declarations are used.
    /// The services it could not put back, and why (an empty name for the directory or the load).
    pub fn recover_installs(&self) -> Vec<(String, String)> {
        let listed = fs::read_dir(&self.config.state)
            .and_then(|entries| entries.collect::<io::Result<Vec<_>>>());
        let mut failed = Vec::new();
        let entries = listed.unwrap_or_else(|e| {
            failed.push((String::new(), e.to_string()));
            Vec::new()
        });
        let services = entries
            .into_iter()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                )
            })
            .filter(|(name, _)| update_rights::names::is_service_name(name));
        for (name, dir) in services {
            let place = self.place(&dir);
            let generation = record_generation(&dir);
            let put_back = declare::began(&place).and_then(|began| {
                began
                    .filter(|before| *before == generation)
                    .map(|_| declare::restore(&place, &name))
                    .transpose()
            });
            failed.extend(put_back.err().map(|e| (name, e.to_string())));
        }
        failed.extend(self.reload().err().map(|e| (String::new(), e.to_string())));
        failed
    }

    /// Reads the record of `service` after the boot, if the read after the boot could not.
    pub(super) fn read_if_unread(&self, service: &str) -> Result<(), Refusal> {
        let mut held = self.unread.lock().unwrap_or_else(PoisonError::into_inner);
        if !held.holds(service) {
            return Ok(());
        }
        let store = RecordStore::new(&self.config.state.join(service));
        boot::read_as_killed(&store, &self.parts.clock.boot_id()).map_err(|e| {
            (
                "boot_unread",
                format!("the record was not read after the boot: {e}"),
            )
        })?;
        held.services.remove(service);
        held.read.insert(service.to_owned());
        Ok(())
    }

    /// The start pass of one service, after the read after a boot ([`RootPart::read_after_boot`])
    /// if there was one: a declaration installed for an update that never began is put back; an update the record holds unfinished is run to its verdict;
    /// then the declaration's boot entry runs, from the release of the last update that
    /// succeeded. The answer carries the verdict of a resumed update and the outcome of the boot
    /// entry (`boot`). A record of a newer schema stops all of it.
    pub fn start(&self, service: &str) -> Answer {
        self.try_start(service)
            .unwrap_or_else(|(token, why)| Answer::refused(token, why))
    }

    fn try_start(&self, service: &str) -> Result<Answer, Refusal> {
        declared(&self.loaded, service).ok_or_else(|| self.unknown(service))?;
        self.read_if_unread(service)?;
        let unit = self.service(service)?;
        let _busy = unit.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let store = RecordStore::new(&unit.dir);
        let rec = stored(&unit.dir)?;
        // A declaration kept for an update that never began (the record still has the generation
        // it had at the install: the root part stopped between the two) goes back.
        let place = self.place(&unit.dir);
        let began = declare::began(&place).map_err(because("internal"))?;
        if began.is_some_and(|before| before == rec.as_ref().map(|rec| rec.generation)) {
            declare::restore(&place, service).map_err(because("internal"))?;
            self.reload().map_err(because("internal"))?;
        }
        let mut answer = Answer {
            ok: true,
            ..Answer::default()
        };
        let Some(declared) = declared(&self.loaded, service) else {
            return Ok(answer);
        };
        let generation = rec.as_ref().map_or(0, |rec| rec.generation);
        if let Some(rec) = rec {
            let finished = match rec.state.clone() {
                State::Finished { verdict, reason } => Finished { verdict, reason },
                _ => {
                    make_record_dir(&declared).map_err(because("internal"))?;
                    let finished = match resumed(&declared, &rec)? {
                        // A post-success watcher does not hold the start pass (nor the agent's
                        // socket, bound after it): it watches on a thread of its own, and its
                        // committed release is `current` now.
                        Resumed::Order(order) if rec.progress.watching.is_some() => {
                            self.watch_in_background(&unit, order);
                            committed(None)
                        }
                        Resumed::Order(order) => unit.runner.run(&order).map_err(run_refusal)?,
                        Resumed::Changed(why) => self.halt_changed(&store, &rec, &why)?,
                    };
                    answer = Answer::finished(finished.clone());
                    finished
                }
            };
            // What the verdict leaves (the `current` link, the declaration kept or put back) is
            // done again: a root part killed between the verdict and those has not done them.
            self.after(service, &rec.target_version, &unit.dir, &finished)?;
        }
        answer.boot = self.boot_entry(service, generation)?;
        Ok(answer)
    }

    /// Resumes the post-success watcher of `unit`'s update on a thread of its own: it takes the
    /// service's lock once the start pass lets it go, and runs until the promotion clears, its
    /// ceiling, or a new order asks it to yield. One left waiting when the root part stops runs
    /// nothing.
    fn watch_in_background(&self, unit: &Arc<Service>, order: Box<UpdateOrder>) {
        let unit = Arc::clone(unit);
        let stop = Arc::clone(&self.stop);
        let watcher = std::thread::spawn(move || {
            let _busy = unit.busy.lock().unwrap_or_else(PoisonError::into_inner);
            if !stop.load(Ordering::Acquire) {
                let _ = unit.runner.run(&order);
            }
        });
        let mut watchers = self.watchers.lock().unwrap_or_else(PoisonError::into_inner);
        watchers.push(watcher);
    }

    /// Asks every watcher the start pass resumed to yield, and waits for each to end.
    pub(super) fn stop_watchers(&self) {
        let watchers =
            std::mem::take(&mut *self.watchers.lock().unwrap_or_else(PoisonError::into_inner));
        let services = self.services.lock().unwrap_or_else(PoisonError::into_inner);
        for service in services.values() {
            service.runner.yield_watcher();
        }
        drop(services);
        for watcher in watchers {
            let _ = watcher.join();
        }
    }

    /// Ends the update `rec` holds as the step runner ends one whose release changed after it
    /// was verified: a `Halt` with the reason `artifact_changed`, kept until an operator clears
    /// it. A step the record holds as running (the root part stopped while it ran, in this boot)
    /// has its unit stopped first, as the runner stops one before it moves on.
    fn halt_changed(
        &self,
        store: &RecordStore,
        rec: &Record,
        why: &str,
    ) -> Result<Finished, Refusal> {
        if matches!(rec.state, State::Running { .. }) {
            self.launcher
                .stop(&rec.unit(rec.step))
                .map_err(because("runner"))?;
        }
        eprintln!(
            "kbf-updater root: {}: {why}; the update is halted",
            rec.service
        );
        let finished = Finished {
            verdict: Verdict::Halt,
            reason: Some(REASON_ARTIFACT_CHANGED.to_owned()),
        };
        let mut halted = rec.clone();
        halted.state = State::Finished {
            verdict: finished.verdict.clone(),
            reason: finished.reason.clone(),
        };
        store.save(&halted).map_err(because("record"))?;
        Ok(finished)
    }

    /// Runs the boot entry of `service`, if its declaration names one and a release of it was
    /// installed by an update: the tree is checked against its seal first, and a unit of the
    /// same name left by an earlier start is stopped. The outcome, `None` when nothing ran.
    fn boot_entry(&self, service: &str, generation: u64) -> Result<Option<String>, Refusal> {
        let declared = declared(&self.loaded, service).ok_or_else(|| self.unknown(service))?;
        let binary = &declared.declaration().binary;
        let current = match declared.declaration().boot {
            Some(_) => self
                .releases()
                .current(service)
                .map_err(because("not_staged"))?,
            None => None,
        };
        let Some(current) = current else {
            return Ok(None);
        };
        current
            .staged
            .check(Path::new(binary))
            .map_err(because("artifact"))?;
        make_record_dir(&declared).map_err(because("internal"))?;
        // As before every step (`launch::HookLauncher`): the record directory is checked again,
        // on what it is now; one the service's account can write runs nothing.
        check_state_dir(
            &declared.declaration().state_dir,
            declared.service().uid,
            declared.service_gids(),
        )
        .map_err(because("boot"))?;
        self.launcher
            .stop(&boot_unit_name(service, generation))
            .map_err(because("boot"))?;
        let ran = run_boot(
            &*self.parts.systemd_run,
            &declared,
            generation,
            current.staged.dir(),
        )
        .map_err(because("boot"))?;
        Ok(ran.map(|run| format!("{:?}", run.outcome).to_lowercase()))
    }
}
