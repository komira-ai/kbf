//! One pass of the helper (`Helper::cycle`), and the verbs it carries out.
//!
//! The order within a pass is the safe one: finish a restart that is owed, revert a swap
//! whose window ran out, then act on the order records. Orders are acted on in (epoch, id)
//! order, once each: the pair is written to `helper.json` before the file is removed, so a
//! crash between the two finds the order acknowledged and only removes it. An activation
//! fetches the release (if it is not installed) before it owes a restart or moves a link, so
//! a refused or failed fetch changes nothing. The links move before the service restarts and
//! the restart is recorded as owed before it is tried, so a crash anywhere leaves a state the
//! next pass completes. An order may name the release the host's package carries instead of
//! one to fetch (`source = staged`, `staged.rs`): it is held to the package's manifest and to
//! a floor on its `seq`, and then goes through the same fetcher, pipeline and swap. At the end
//! of a pass the helper looks at the package and the source and tells the frontend what it
//! found, and keeps the floor up with what the host has confirmed (`notice.rs`); the status
//! says it, nothing else moves.

use std::io::Write;
use std::time::Duration;

use crate::CommitId;
use crate::activate::{self, Activated, Arm, Confirmed, Expiry};
use crate::layout::{self, Layout, Platform};
use crate::meta_log;
use crate::release::{self, ReleaseError};
use crate::spool::{Entry, read_capped};
use crate::stage;
use crate::state::{self, Locked, State, StateError};
use kbf_host::plan::Mode;
use kbf_host::{Exec, HostPath, Perms, Step};
use kbf_update::{Facts, Source};

mod frontend;
mod notice;
mod staged;

use self::notice::Notice;
use self::staged::{Held, refusal_of};
use crate::database::SchemaStep;
use crate::helper::fetcher::{Fetcher, InstalledRelease};
use crate::helper::leases::{self, LeaseSource};
use crate::helper::memory::{Event, Memory, StagedFacts};
use crate::helper::order::{self, Claims, Order, Verb};
use crate::helper::restart::{self, Restart};
use crate::helper::service::{self, Procs};
use crate::helper::status;
use crate::host_mode;

/// The most names looked at in the order spool in one pass; more is said, not followed.
const SCAN_LIMIT: usize = 256;

/// How the helper is set up on a host, by its own flags.
#[derive(Clone, Debug)]
pub struct Config {
    /// Root's directory: `status.json` goes here.
    pub update_dir: HostPath,
    /// The order spool: owned by the service user, mode 0700, inside a root-owned parent.
    pub orders_dir: HostPath,
    pub restart: Restart,
    /// The window the boot check gives a swap to be confirmed (Linux).
    pub window_secs: u64,
    /// The starts a swap on trial gets (Linux).
    pub starts: u32,
    /// The fewest seconds between one activation and the next (`min_activation_interval_s`).
    pub min_activation_interval_secs: u64,
}

/// How many releases `gc` keeps besides the ones that must stay (`retain`).
pub const GC_KEEP: usize = crate::retain::DEFAULT_KEEP;

/// Something the helper cannot go on from: the host's own files are wrong. A refused
/// offer or order is not one; it is recorded and reported.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("the host state: {0}")]
    State(#[from] StateError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("helper.json: {0}")]
    Memory(String),
}

/// A host's helper: its layout, how it is changed, and its flags.
pub struct Helper<E: Exec> {
    host: Layout,
    exec: E,
    cfg: Config,
    /// What the verbs printed; kept for the log, never trusted.
    log: Vec<u8>,
    /// The last `status.json` written.
    written: Vec<u8>,
    /// Brings the state database to what a release needs before it is activated; none on
    /// a host that has no database (a node).
    schema: Option<Box<dyn SchemaStep>>,
    fetcher: Box<dyn Fetcher>,
    leases: Box<dyn LeaseSource>,
    procs: Box<dyn Procs>,
    /// A swap was confirmed in this pass: the release it replaced may now be collected.
    gc_due: bool,
    /// What the helper remembers of the package and the source between passes (`notice.rs`).
    notice: Notice,
}

fn memory_path(host: &Layout) -> HostPath {
    HostPath::new(&format!("{}/{}", host.state_dir().as_str(), layout::MEMORY))
        .expect("the state directory and a plain name")
}

impl<E: Exec> Helper<E> {
    pub fn new(
        host: Layout,
        exec: E,
        cfg: Config,
        fetcher: Box<dyn Fetcher>,
        leases: Box<dyn LeaseSource>,
        procs: Box<dyn Procs>,
    ) -> Helper<E> {
        Helper {
            host,
            exec,
            cfg,
            log: Vec::new(),
            written: Vec::new(),
            schema: None,
            fetcher,
            leases,
            procs,
            gc_due: false,
            notice: Notice::default(),
        }
    }

    /// Runs `step` before every activation of the release this helper has staged, so the
    /// state database is migrated before the new child can start. A refusal keeps the
    /// links where they are.
    pub fn set_schema_step(&mut self, step: Box<dyn SchemaStep>) {
        self.schema = Some(step);
    }

    /// The schema step, for a test to see what a caller attached.
    #[cfg(test)]
    pub(crate) fn schema_step_mut(&mut self) -> Option<&mut (dyn SchemaStep + 'static)> {
        self.schema.as_deref_mut()
    }

    /// The lease source, for a test to see what a caller wired.
    #[cfg(test)]
    pub(crate) fn leases(&self) -> &dyn LeaseSource {
        self.leases.as_ref()
    }

    /// Replace the lease source, for a test that plays a Mac's.
    #[cfg(test)]
    pub(crate) fn set_leases(&mut self, leases: Box<dyn LeaseSource>) {
        self.leases = leases;
    }

    /// The host's executor, for a test to read what was done.
    pub fn exec(&self) -> &E {
        &self.exec
    }

    /// The host's executor, for a test to change how it answers.
    pub fn exec_mut(&mut self) -> &mut E {
        &mut self.exec
    }

    /// The fetcher, for a test to see what a caller attached.
    #[cfg(test)]
    pub(crate) fn fetcher(&self) -> &dyn Fetcher {
        &*self.fetcher
    }

    /// The fetcher, replaced by a test.
    #[cfg(test)]
    pub(super) fn set_fetcher(&mut self, fetcher: Box<dyn Fetcher>) {
        self.fetcher = fetcher;
    }

    fn orders_path(&self) -> std::path::PathBuf {
        self.host.join(&self.cfg.orders_dir)
    }

    fn load(&self) -> Result<Memory, HelperError> {
        let path = self.host.join(&memory_path(&self.host));
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| HelperError::Memory(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Memory::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn put(&mut self, path: HostPath, perms: u32, bytes: Vec<u8>) -> Result<(), HelperError> {
        self.exec.apply(&Step::WriteFile {
            path,
            perms: Perms::new(perms),
            owner: None,
            bytes,
        })?;
        Ok(())
    }

    fn save(&mut self, memory: &Memory) -> Result<(), HelperError> {
        let bytes = serde_json::to_vec(memory).expect("plain fields serialize");
        self.put(memory_path(&self.host), 0o600, bytes)
    }

    fn state(&self) -> Result<State, HelperError> {
        Ok(state::read_unlocked(&self.host)?.unwrap_or_default())
    }

    /// Run once at start: remove the scratch a killed stage left in `work/`, and make the
    /// links agree with the state after an interrupted verb. A link that moved means the
    /// service may still run the old release: restart it.
    ///
    /// The sweep is made here and only here, under the lock: this process is not staging
    /// yet, and a pass must not sweep, since `setup` stages the first release in a process
    /// of its own. A sweep that fails is logged, never a refusal (as with the gc after a swap:
    /// nothing was ordered that it could undo, and a refusal would hide the host's real
    /// state in `status.json`); the start goes on, and a commit the leftovers still wedge is
    /// refused when it is ordered.
    pub fn recover(&mut self) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        let runs_frontend = host_mode::frontend_host(&self.host).is_ok();
        if let Err(e) = self.sweep_scratch() {
            let _ = writeln!(self.log, "work sweep: {e}");
        }
        match activate::reconcile(&mut self.exec, &self.host, &mut self.log) {
            // A frontend host owes the unit a restart only for a node swap on trial: the
            // proxy owns the frontend's children, and reads `installed_commit` itself. `|`,
            // not `||`: no short-circuit for a test to have to reach.
            Ok(true) => {
                memory.restart_due |= !runs_frontend | self.state()?.pending_confirm.is_some();
            }
            Ok(false) => {}
            Err(e) => refuse(&mut memory, &e.to_string()),
        }
        self.save(&memory)
    }

    /// [`stage::sweep_work`] under the lock, which is let go before this returns (the repair
    /// that follows takes it again).
    fn sweep_scratch(&self) -> Result<(), ReleaseError> {
        let lock = Locked::acquire(&self.host)?;
        stage::sweep_work(&self.host, &lock)
    }

    /// One pass. `now` is seconds since the epoch.
    pub fn cycle(&mut self, now: u64) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        self.notice.stamp(&mut memory);
        self.report_meta_log_refusal(&mut memory)?;
        self.restart_if_owed(&mut memory)?;
        self.expire(now, &mut memory)?;
        self.orders(now, &mut memory)?;
        self.restart_if_owed(&mut memory)?;
        self.collect();
        // What the orders came to is told before the look, which reads the package in full the
        // first time and may run `gh`; the write is skipped when the second report is the same.
        self.report(&memory)?;
        self.notice_package(now, &mut memory);
        self.report(&memory)
    }

    /// Remove the releases nothing needs, once, after a swap was confirmed. A failure is
    /// logged, never a refusal: nothing was ordered that it could undo.
    fn collect(&mut self) {
        if !std::mem::take(&mut self.gc_due) {
            return;
        }
        let result = service::in_use(&*self.procs, &self.host)
            .map_err(|e| format!("cannot tell which releases run: {e}"))
            .and_then(|in_use| {
                release::gc(
                    &mut self.exec,
                    &self.host,
                    GC_KEEP,
                    &in_use,
                    Mode::Apply,
                    &mut self.log,
                )
                .map_err(|e| e.to_string())
            });
        if let Err(why) = result {
            let _ = writeln!(self.log, "gc: {why}");
        }
    }

    /// An activation `boot-check` reverted for want of a copy of the metadata log: say why in
    /// `status.json`. The note is removed once the memory that holds it is saved.
    fn report_meta_log_refusal(&mut self, memory: &mut Memory) -> Result<(), HelperError> {
        if let Some(why) = meta_log::refusal(&self.host) {
            refuse(memory, &why);
            self.save(memory)?;
            meta_log::clear_refusal(&self.host);
        }
        Ok(())
    }

    fn restart_if_owed(&mut self, memory: &mut Memory) -> Result<(), HelperError> {
        let marker = restart::owed_marker(&self.host);
        let by_operator = std::fs::symlink_metadata(self.host.join(&marker)).is_ok();
        // An operator verb that left the marker restarts the service itself while it holds
        // the lock; one that holds it now is left to finish, and is looked at next pass.
        let held = if by_operator {
            match Locked::acquire_within(&self.host, Duration::ZERO) {
                Ok(lock) => Some(lock),
                Err(StateError::Busy { .. }) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };
        if !memory.restart_due && !by_operator {
            return Ok(());
        }
        if let Err(e) = self.exec.apply(&self.cfg.restart.step()) {
            refuse(memory, &format!("the restart failed: {e}"));
            return self.save(memory);
        }
        if by_operator {
            self.exec.apply(&Step::Remove { path: marker })?;
        }
        drop(held);
        memory.restart_due = false;
        self.save(memory)
    }

    fn expire(&mut self, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        // A swap whose window has run out is about to be reverted: owe the restart first,
        // so a kill between the links moving and the save below cannot lose it.
        let before = self.state()?;
        let due = before
            .pending_confirm
            .as_ref()
            .and_then(|p| p.deadline_wall)
            .is_some_and(|d| now >= d);
        let prior = memory.restart_due;
        if due {
            memory.restart_due = true;
            self.save(memory)?;
        }
        let result = activate::expire(&mut self.exec, &self.host, now, &mut self.log);
        let moved = self.moved(&before)?;
        // `|`, not `||`: no short-circuit for a test to have to reach.
        memory.restart_due = prior | moved;
        match result {
            Ok(Expiry::Reverted(r)) => {
                memory.event = Event::Reverted;
                memory.refusal = format!(
                    "{} was not confirmed in time; back on {}",
                    short(&r.from),
                    r.to
                );
            }
            Ok(_) => {}
            Err(e) => refuse(memory, &e.to_string()),
        }
        self.save(memory)
    }

    /// Whether `current` is not what it was in `before`: the links followed the state, so
    /// the service runs a release other than the one the links name now.
    fn moved(&self, before: &State) -> Result<bool, HelperError> {
        Ok(self.state()?.current != before.current)
    }

    fn orders(&mut self, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        // The spool is opened without following a link: one the service user puts in its
        // place is refused, never followed. Its absence is not an error.
        let Ok(dir) = Entry::open(&self.orders_path()) else {
            return self.save(memory);
        };
        self.orders_in(&dir, now, memory)
    }

    /// The order records in `dir`, in (epoch, id) order.
    pub(super) fn orders_in(
        &mut self,
        dir: &Entry,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), HelperError> {
        let (names, crowded) = match dir.names_up_to(SCAN_LIMIT) {
            Ok(found) => found,
            Err(r) => {
                refuse(memory, &format!("{}: {r}", r.code()));
                return self.save(memory);
            }
        };
        if crowded {
            refuse(
                memory,
                &format!(
                    "the order spool holds more than {SCAN_LIMIT} entries; the rest are not looked at"
                ),
            );
        }
        // A name that is not a record's (a `.partial-` the client is writing, its state
        // file, or junk) is left.
        let mut files: Vec<((u64, u64), String)> = names
            .into_iter()
            .filter_map(|name| order::key_of(&name).map(|key| (key, name)))
            .collect();
        files.sort();
        // First seen by root now, or earlier; what is no longer there is forgotten.
        memory
            .first_seen
            .retain(|name, _| files.iter().any(|(_, f)| f == name));
        for (_, name) in &files {
            memory.first_seen.entry(name.clone()).or_insert(now);
        }
        self.save(memory)?;
        for (index, ((epoch, id), name)) in files.into_iter().enumerate() {
            let age = now.saturating_sub(memory.first_seen.get(&name).copied().unwrap_or(now));
            if index >= kbf_update::MAX_RECORDS {
                refuse(
                    memory,
                    &format!(
                        "the order spool holds more than {} records; order {id} was deleted unread",
                        kbf_update::MAX_RECORDS
                    ),
                );
            } else if age > kbf_update::MAX_AGE_SECS {
                refuse(
                    memory,
                    &format!(
                        "order {id} was first seen {age} s ago, more than {} s: expired, \
                         deleted unread",
                        kbf_update::MAX_AGE_SECS
                    ),
                );
            } else {
                // Opened by name beneath the descriptor, judged by fstat, read once into
                // memory, and parsed from that copy.
                let read = dir
                    .open_file(&name, order::MAX_BYTES)
                    .and_then(|mut f| read_capped(&mut f, order::MAX_BYTES, &name));
                let parsed = read
                    .map_err(|r| format!("{}: {r}", r.code()))
                    .and_then(|bytes| order::parse(epoch, id, &bytes).map_err(|e| e.to_string()));
                self.take(parsed, &name, now, memory)?;
            }
            memory.first_seen.remove(&name);
            self.save(memory)?;
            if let Err(r) = dir.remove_file(&name) {
                refuse(memory, &format!("{}: {r}", r.code()));
            }
        }
        self.save(memory)
    }

    /// Decide what to do with one record read: act on it, drop it as a replay, or refuse it.
    fn take(
        &mut self,
        parsed: Result<Order, String>,
        name: &str,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), HelperError> {
        let state = self.state()?;
        // Read each pass's orders afresh: the file is the host's, and may change under us.
        let not_frontend = host_mode::frontend_host(&self.host).err();
        let frontend_host = not_frontend.is_none();
        // Each kind of order is ordered against its own floor.
        let floor = |o: &Order, memory: &Memory| match kind_of(o, &state, frontend_host) {
            Kind::Frontend(_) => (memory.frontend_epoch, memory.frontend_order_id),
            Kind::Node => (memory.acked_epoch, memory.acked_order_id),
        };
        // A frontend order's refusals say their word first (KI section 5).
        let word = |o: &Order| match kind_of(o, &state, frontend_host) {
            Kind::Frontend(_) => "floor: ",
            Kind::Node => "",
        };
        // The proxy blames a refusal on its update by the order's id alone, so only its own
        // kinds of order are named: the node's ids are another counter's.
        let named = |o: &Order| match kind_of(o, &state, frontend_host) {
            Kind::Frontend(_) => Some(o.id),
            Kind::Node => None,
        };
        match parsed {
            // A frontend order on a host that runs no frontend: refused, and like any
            // refusal of a record it does not move a floor.
            Ok(o) if !frontend_host && o.verb == Verb::FrontendUpdate => refuse_order(
                memory,
                named(&o),
                &format!(
                    "denied: order {}: this host runs no frontend (frontend_update is refused): {}",
                    o.id,
                    not_frontend.as_deref().unwrap_or_default()
                ),
            ),
            Ok(o)
                if floor(&o, memory).0 != 0
                    && o.epoch > floor(&o, memory).0.saturating_add(order::MAX_EPOCH_JUMP) =>
            {
                refuse_order(
                    memory,
                    named(&o),
                    &format!(
                        "{}order {} is from epoch {}, more than {} above the last acted on ({}): \
                         not acted on",
                        word(&o),
                        o.id,
                        o.epoch,
                        order::MAX_EPOCH_JUMP,
                        floor(&o, memory).0
                    ),
                );
            }
            Ok(o) if o.epoch < floor(&o, memory).0 => {
                refuse_order(
                    memory,
                    named(&o),
                    &format!(
                        "{}order {} is from epoch {}, older than the last acted on ({})",
                        word(&o),
                        o.id,
                        o.epoch,
                        floor(&o, memory).0
                    ),
                );
            }
            // The same epoch and an id not above the last: a replay, dropped silently.
            Ok(o) if o.epoch == floor(&o, memory).0 && o.id <= floor(&o, memory).1 => {}
            Ok(o) => {
                // Acted on includes refused: the floor moves either way.
                match kind_of(&o, &state, frontend_host) {
                    Kind::Frontend(step) => {
                        self.frontend(step, &o, memory);
                        memory.frontend_order_id = o.id;
                        memory.frontend_epoch = o.epoch;
                    }
                    Kind::Node => {
                        self.act(&o, now, memory)?;
                        memory.acked_order_id = o.id;
                        memory.acked_epoch = o.epoch;
                    }
                }
                memory.last_acted_id = o.id;
                // Every way of acting on an order sets the event, so a refusal here is this
                // order's: the proxy that wrote it can tell it from an older one by the id.
                memory.refused_order_id = named(&o).filter(|_| memory.event == Event::Refused);
            }
            // Not acknowledged: a garbage file must not move the floor.
            Err(why) => refuse(memory, &format!("order file {name}: {why}")),
        }
        Ok(())
    }

    /// Carry out one order; what came of it goes into `memory`. An activation is vetted and
    /// its release fetched first, while nothing is owed and nothing moves. An activate or a
    /// revert then moves the links, so the restart is owed (and saved) before the verb runs;
    /// a kill between the links and the next save then leaves the restart for the next pass.
    pub(super) fn act(
        &mut self,
        order: &Order,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), HelperError> {
        let before = self.state()?;
        if order.verb == Verb::Activate
            && let Err(why) = self.prepare(order, &before, now, memory)
        {
            refuse(memory, &why);
            return self.save(memory);
        }
        let prior = memory.restart_due;
        if order.verb != Verb::Confirm {
            memory.restart_due = true;
            self.save(memory)?;
        }
        let result = match order.verb {
            Verb::Activate => self.activate(order, now, memory),
            Verb::Confirm => self.confirm(order, &before),
            Verb::Revert => self.revert(),
            // Taken before this point, by `kind_of`; never carried out as a node verb.
            Verb::FrontendUpdate => Err("frontend_update is not a node order".to_owned()),
        };
        let moved = self.moved(&before)?;
        match result {
            Ok(done) => {
                memory.event = done.event;
                memory.refusal = done.note;
                memory.restart_due = prior | done.restart | moved;
                if done.swapped {
                    memory.last_activation_at = now;
                }
                self.gc_due |= done.kept;
                if done.kept {
                    self.raise_floor();
                }
            }
            Err(why) => {
                refuse(memory, &why);
                memory.restart_due = prior | moved;
            }
        }
        Ok(())
    }

    /// The checks an activation passes before anything is moved, then the fetch. Ok means
    /// `current` may be swapped to the order's commit. The rule on the order's generation
    /// and the operator's floors are the next change's.
    pub(super) fn prepare(
        &mut self,
        order: &Order,
        before: &State,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), String> {
        let id = order.id;
        // A frontend holds no leases, so the lease check below would let this through, and
        // the swap would restart it: a frontend moves by `frontend_update`, never by this.
        if let Some(unit) = leases::frontend_unit(&self.cfg.restart) {
            return Err(format!(
                "denied: order {id}: {unit:?} is a frontend's unit; a frontend moves by \
                 frontend_update, and an activate would restart it"
            ));
        }
        // An activation that fetches claims the release it expects; a staged one claims none,
        // since the package's manifest says what its release is.
        let claims = match order.source {
            Source::Fetch => Some(self.vet_claims(order)?),
            Source::Staged => None,
        };
        // One confirm window at a time, never two timers. The same order again (a pass
        // that died after the swap) is the repeat `activate` recognises.
        let this = format!("{}-{}", order.epoch, order.id);
        if let Some(p) = &before.pending_confirm
            && p.activation != this
        {
            return Err(format!(
                "order {id}: {} is on trial and not yet confirmed or reverted; not activating",
                p.commit
            ));
        }
        // The lease check again, from what the installer observes.
        match self.leases.live_noted() {
            Ok((0, note)) => {
                if let Some(note) = note {
                    let _ = writeln!(self.log, "leases: {note}");
                }
            }
            Ok((n, _)) => return Err(format!("{n} leases are live; not activating")),
            Err(why) => return Err(format!("cannot tell whether leases are live: {why}")),
        }
        // Already the running release: nothing to fetch, `activate` says so.
        if before.current.as_ref() == Some(&order.commit) {
            return Ok(());
        }
        // Not two swaps in quick succession, whatever the frontend orders. A clock that
        // stepped back (the last swap is "in the future") does not hold an order up.
        let last = memory.last_activation_at;
        let min = self.cfg.min_activation_interval_secs;
        if last != 0 && last <= now && now - last < min {
            return Err(format!(
                "order {id}: the last activation was {} s ago; at least {min} s between activations",
                now - last
            ));
        }
        // A staged order is held to the rules of the seed now: after the shortcut above (a
        // repeat of the order for the running release is not a refusal that holds the run),
        // before anything is done.
        let held = match claims {
            Some(claims) => Held::Claims(claims),
            None => {
                let seed = self.admitted_seed(&order.commit);
                Held::Seed(seed.map_err(|e| refusal_of(id, e))?)
            }
        };
        // The node is told a fetch is under way; it may take minutes. A directory on the host
        // is not proof: the fetcher verifies the release whether it pulls it or finds it.
        memory.fetching = true;
        self.report(memory).map_err(|e| e.to_string())?;
        let fetched = self.fetch_held(&order.commit, &held);
        memory.fetching = false;
        let got = fetched.map_err(|e| refusal_of(id, e))?;
        // What the fetched binary declared must be what the order expected, before a link
        // moves: a release that is attested but is another build than the one ordered is
        // refused. A staged order expects what the manifest said when it was admitted, and
        // the package must still say it.
        let checked = match &held {
            Held::Claims(claims) => matches_facts(&got, &order.commit, &claims.facts()),
            Held::Seed(seed) => matches_facts(&got, &order.commit, &seed.facts)
                .and_then(|()| self.seed_unchanged(seed)),
        };
        if let Err(why) = checked {
            return Err(format!(
                "order {id}: the fetched release is not the one ordered: {why}"
            ));
        }
        memory.staged = Some(facts(&got));
        memory.staged_profile = Some(got.profile.clone());
        Ok(())
    }

    /// An activation that fetches carries the release's claims, complete and for this host:
    /// fail closed without them. `kbf` is the one component a release of either platform
    /// carries that an order may name (a Mac is given no `kbf-daemon`).
    fn vet_claims<'o>(&self, order: &'o Order) -> Result<&'o Claims, String> {
        let id = order.id;
        let Some(claims) = &order.claims else {
            return Err(format!(
                "order {id}: an activation carries no release claims"
            ));
        };
        let prefix = platform_prefix(&self.host);
        let needs = ["kbf"].as_slice();
        if !claims.platform.starts_with(prefix) {
            return Err(format!(
                "order {id} is for platform {:?}, not this host's ({prefix}*)",
                claims.platform
            ));
        }
        let mut names: Vec<&str> = claims.components.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        if names != needs {
            return Err(format!(
                "order {id} names components {names:?}; this platform needs exactly {needs:?}"
            ));
        }
        Ok(claims)
    }

    fn arm(&self, order: &Order) -> Arm {
        match self.host.platform() {
            Platform::Linux => Arm::FromFirstStart {
                starts: self.cfg.starts,
            },
            Platform::MacOs => Arm::FromActivation {
                window_secs: u64::from(order.window_s)
                    .clamp(activate::WINDOW_MIN_SECS, activate::WINDOW_MAX_SECS),
            },
        }
    }

    fn activate(&mut self, order: &Order, now: u64, memory: &Memory) -> Result<Done, String> {
        // Before the links move: a release must not start on a schema it cannot run on.
        // The schema is known here only for the release this helper has staged (its own
        // report): the rule is "the staged commit equals the order's". An order for any other
        // release (one fetched by an earlier run, say) asks for no migration and says so in
        // the log: kbf's own start check guards it. The staged release stays staged after its
        // swap, so a repeat of its activation asks again (idempotent, but refused while the
        // database is down).
        if let Some(step) = self.schema.as_mut() {
            match memory.staged.as_ref().filter(|f| f.commit == order.commit) {
                Some(staged) => {
                    let note = step
                        .before_start(staged.state_schema)
                        .map_err(|why| format!("state schema: {why}"))?;
                    let _ = writeln!(self.log, "state schema: {note}");
                }
                None => {
                    let _ = writeln!(
                        self.log,
                        "state schema: no migration for order {}: {} was not fetched for it \
                         (the staged release is {}), so the schema it needs is not known \
                         here; its own start check guards it",
                        order.id,
                        &order.commit.as_str()[..12],
                        memory
                            .staged
                            .as_ref()
                            .map_or("none", |f| &f.commit.as_str()[..12]),
                    );
                }
            }
        }
        let ordered = activate::Order {
            commit: order.commit.clone(),
            // The epoch is part of the name: a restored store counts ids again, and an activation
            // under the same id as a pending one would be taken for its repeat.
            order_id: format!("{}-{}", order.epoch, order.id),
        };
        let arm = self.arm(order);
        match activate::activate(
            &mut self.exec,
            &self.host,
            &ordered,
            arm,
            now,
            &mut self.log,
        ) {
            Ok(Activated::Swapped) => Ok(Done {
                swapped: true,
                ..Done::restart()
            }),
            Ok(Activated::Repeat) => Ok(Done::restart()),
            Ok(Activated::AlreadyCurrent) => Ok(Done::quiet(Event::Idle)),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn confirm(&mut self, order: &Order, state: &State) -> Result<Done, String> {
        // The activation's own id: the confirm order names the commit, the state names the swap.
        let activation = state
            .pending_confirm
            .as_ref()
            .filter(|p| p.commit == order.commit)
            .map(|p| p.activation.clone())
            .unwrap_or_default();
        // The new release must be what runs before it is kept: a record saying so is the
        // service user's word, and the service user may be the one that is broken.
        if let Some(p) = state
            .pending_confirm
            .as_ref()
            .filter(|p| p.commit == order.commit)
        {
            service::accept_confirm(&self.exec, &*self.procs, &self.host, &self.cfg.restart, p)?;
        }
        match activate::confirm(&self.host, &activation, &order.commit) {
            Ok(Confirmed::Kept) => Ok(Done {
                kept: true,
                ..Done::quiet(Event::Confirmed)
            }),
            Ok(Confirmed::AlreadyKept) => Ok(Done::quiet(Event::Confirmed)),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn revert(&mut self) -> Result<Done, String> {
        match activate::revert(&mut self.exec, &self.host, &mut self.log) {
            Ok(r) => Ok(Done {
                event: Event::Reverted,
                note: format!("reverted by order: {} is current", r.to),
                restart: true,
                swapped: false,
                kept: false,
            }),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn report(&mut self, memory: &Memory) -> Result<(), HelperError> {
        let bytes = status::render(&self.state()?, memory);
        if bytes == self.written {
            return Ok(());
        }
        let path = HostPath::new(&format!("{}/status.json", self.cfg.update_dir.as_str()))
            .expect("the update directory and a plain name");
        self.put(path, 0o644, bytes.clone())?;
        self.written = bytes;
        Ok(())
    }

    /// Print what the installer verbs wrote to `out`.
    pub fn drain_log(&mut self, out: &mut dyn Write) {
        let _ = out.write_all(&std::mem::take(&mut self.log));
    }
}

/// Whose order a record is: the node's (acted on against `acked_*`, restarting the unit) or
/// the frontend proxy's (against the `frontend_*` floor, never restarting a unit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Node,
    Frontend(FrontendStep),
}

/// The three orders of the frontend proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontendStep {
    Update,
    Confirm,
    Revert,
}

/// The kind of `order` on a host in `state`. `frontend_update` is the proxy's always. On a
/// frontend host a `confirm` or `revert` is the proxy's unless it names the release a node
/// swap has on trial (then it is that swap's). Elsewhere they are the node's.
fn kind_of(order: &Order, state: &State, frontend_host: bool) -> Kind {
    let on_trial = state
        .pending_confirm
        .as_ref()
        .is_some_and(|p| p.commit == order.commit);
    match order.verb {
        Verb::Activate => Kind::Node,
        Verb::FrontendUpdate => Kind::Frontend(FrontendStep::Update),
        Verb::Confirm if frontend_host & !on_trial => Kind::Frontend(FrontendStep::Confirm),
        Verb::Revert if frontend_host & !on_trial => Kind::Frontend(FrontendStep::Revert),
        Verb::Confirm | Verb::Revert => Kind::Node,
    }
}

/// What an order did.
struct Done {
    event: Event,
    note: String,
    restart: bool,
    /// The links moved to a new release on trial.
    swapped: bool,
    /// A pending swap was confirmed.
    kept: bool,
}

impl Done {
    fn restart() -> Done {
        Done {
            event: Event::Idle,
            note: String::new(),
            restart: true,
            swapped: false,
            kept: false,
        }
    }

    fn quiet(event: Event) -> Done {
        Done {
            event,
            note: String::new(),
            restart: false,
            swapped: false,
            kept: false,
        }
    }
}

/// What a release for this host's platform declares its platform starts with.
fn platform_prefix(host: &Layout) -> &'static str {
    match host.platform() {
        Platform::Linux => "linux-",
        Platform::MacOs => "darwin-",
    }
}

/// Record a refusal that is no order's (a record that could not be read, a restart that failed,
/// the spool crowded): it names none.
fn refuse(memory: &mut Memory, why: &str) {
    memory.event = Event::Refused;
    memory.refusal = why.to_owned();
    memory.refused_order_id = None;
}

/// Record the refusal of an order, named by `id` when it is one of the proxy's, so that whoever
/// wrote the order can tell its refusal from an older one.
fn refuse_order(memory: &mut Memory, id: Option<u64>, why: &str) {
    refuse(memory, why);
    memory.refused_order_id = id;
}

fn release_err(e: &ReleaseError) -> String {
    e.to_string()
}

fn short(c: &Option<CommitId>) -> String {
    // A swap that expires always has a release it came from; the empty case is never named.
    c.as_ref()
        .map(|c| c.as_str()[..12].to_owned())
        .unwrap_or_default()
}

/// Whether the release the fetcher installed is the one the order named (`commit`) and
/// expected (`want`).
fn matches_facts(got: &InstalledRelease, commit: &CommitId, want: &Facts) -> Result<(), String> {
    if got.commit != *commit {
        return Err(format!("it is commit {}", got.commit.as_str()));
    }
    let same = (
        &got.platform,
        got.protocol,
        got.min_protocol,
        got.state_schema,
        &got.profile,
    ) == (
        &want.platform,
        want.protocol,
        want.min_protocol,
        want.state_schema,
        &want.profile,
    );
    if same {
        Ok(())
    } else {
        Err(format!(
            "it declares platform {:?}, protocol {}, min_protocol {}, state_schema {}, profile {:?}; \
             the order expected {:?}, {}, {}, {}, {:?}",
            got.platform,
            got.protocol,
            got.min_protocol,
            got.state_schema,
            got.profile,
            want.platform,
            want.protocol,
            want.min_protocol,
            want.state_schema,
            want.profile
        ))
    }
}

fn facts(got: &InstalledRelease) -> StagedFacts {
    StagedFacts {
        commit: got.commit.clone(),
        protocol: got.protocol,
        min_protocol: got.min_protocol,
        state_schema: got.state_schema,
    }
}
