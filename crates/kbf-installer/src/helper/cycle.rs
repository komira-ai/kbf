//! One pass of the helper (`Helper::cycle`), and the verbs it carries out.
//!
//! The order within a pass is the safe one: finish a restart that is owed, revert a swap
//! whose window ran out, then act on the order records. Orders are acted on in (epoch, id)
//! order, once each: the pair is written to `helper.json` before the file is removed, so a
//! crash between the two finds the order acknowledged and only removes it. An activation
//! fetches the release (if it is not installed) before it owes a restart or moves a link, so
//! a refused or failed fetch changes nothing. The links move before the service restarts and
//! the restart is recorded as owed before it is tried, so a crash anywhere leaves a state the
//! next pass completes.

use std::io::Write;

use crate::CommitId;
use crate::activate::{self, Activated, Arm, Confirmed, Expiry};
use crate::frontend::{Begun, Frontend, FrontendError, Progress};
use crate::layout::{Layout, Platform};
use crate::release::ReleaseError;
use crate::spool::{Entry, read_capped};
use crate::state::{self, State, StateError};
use kbf_host::{Exec, HostPath, Perms, Step};

use crate::helper::fetcher::{Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::leases::LeaseSource;
use crate::helper::memory::{Event, Memory, StagedFacts};
use crate::helper::order::{self, Claims, Order, Verb};
use crate::helper::restart::Restart;
use crate::helper::status;

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
}

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
    #[error("the frontend update: {0}")]
    Frontend(#[from] FrontendError),
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
    fetcher: Box<dyn Fetcher>,
    leases: Box<dyn LeaseSource>,
    /// The frontend's update, when this host was set up for one.
    frontend: Option<Frontend>,
    /// A frontend update is in flight: read at the start of every pass, before any order is
    /// acted on, and set when this pass begins one. A node `activate` waits for it (the
    /// update's flip moves `current`).
    frontend_in_flight: bool,
}

fn memory_path(host: &Layout) -> HostPath {
    HostPath::new(&format!("{}/helper.json", host.state_dir().as_str()))
        .expect("the state directory and a plain name")
}

impl<E: Exec> Helper<E> {
    pub fn new(
        host: Layout,
        exec: E,
        cfg: Config,
        fetcher: Box<dyn Fetcher>,
        leases: Box<dyn LeaseSource>,
    ) -> Helper<E> {
        Helper {
            host,
            exec,
            cfg,
            log: Vec::new(),
            written: Vec::new(),
            fetcher,
            leases,
            frontend: None,
            frontend_in_flight: false,
        }
    }

    /// Set the host up to update its frontend: a `frontend_update` record is acted on, and
    /// a pending one is moved on every pass. Without it such a record is refused.
    #[must_use]
    pub fn with_frontend(mut self, frontend: Frontend) -> Helper<E> {
        self.frontend = Some(frontend);
        self
    }

    /// The host's executor, for a test to read what was done.
    pub fn exec(&self) -> &E {
        &self.exec
    }

    /// The host's executor, for a test to change how it answers.
    pub fn exec_mut(&mut self) -> &mut E {
        &mut self.exec
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

    /// Run once at start: make the links agree with the state after an interrupted verb.
    /// A link that moved means the service may still run the old release: restart it.
    pub fn recover(&mut self) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        match activate::reconcile(&mut self.exec, &self.host, &mut self.log) {
            Ok(true) => memory.restart_due = true,
            Ok(false) => {}
            Err(e) => refuse(&mut memory, &e.to_string()),
        }
        self.save(&memory)
    }

    /// One pass. `now` is seconds since the epoch.
    pub fn cycle(&mut self, now: u64) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        self.restart_if_owed(&mut memory)?;
        self.expire(now, &mut memory)?;
        self.advance_frontend(now, &mut memory)?;
        self.orders(now, &mut memory)?;
        self.restart_if_owed(&mut memory)?;
        self.report(&memory)
    }

    fn restart_if_owed(&mut self, memory: &mut Memory) -> Result<(), HelperError> {
        if !memory.restart_due {
            return Ok(());
        }
        if let Err(e) = self.exec.apply(&self.cfg.restart.step()) {
            refuse(memory, &format!("the restart failed: {e}"));
            return self.save(memory);
        }
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
            .and_then(|p| p.deadline)
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
        let dir = Entry::open(&self.orders_path()).ok();
        let mut files: Vec<((u64, u64), String)> = Vec::new();
        if let Some(dir) = &dir {
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
            files = names
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
        }
        for (index, ((epoch, id), name)) in files.into_iter().enumerate() {
            let Some(dir) = &dir else { break };
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
    /// The floor it is held to is its kind's: a node's orders and the frontend's own update
    /// are separate sequences.
    fn take(
        &mut self,
        parsed: Result<Order, String>,
        name: &str,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), HelperError> {
        match parsed {
            Ok(o) => {
                let kind = o.verb.kind();
                let (acked_epoch, acked_id) = memory.floor(kind);
                if acked_epoch != 0 && o.epoch > acked_epoch.saturating_add(order::MAX_EPOCH_JUMP) {
                    refuse(
                        memory,
                        &format!(
                            "order {} is from epoch {}, more than {} above the last acted on ({}): \
                             not acted on",
                            o.id,
                            o.epoch,
                            order::MAX_EPOCH_JUMP,
                            acked_epoch
                        ),
                    );
                } else if o.epoch < acked_epoch {
                    refuse(
                        memory,
                        &format!(
                            "order {} is from epoch {}, older than the last acted on ({})",
                            o.id, o.epoch, acked_epoch
                        ),
                    );
                } else if o.epoch == acked_epoch && o.id <= acked_id {
                    // The same epoch and an id not above the last: a replay, dropped silently.
                } else {
                    self.act(&o, now, memory)?;
                    // (see `moves_floor` for the one order that does not)
                    if moves_floor(o.verb, self.frontend.is_some()) {
                        memory.acknowledge(kind, o.epoch, o.id);
                    }
                }
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
    fn act(&mut self, order: &Order, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        if order.verb == Verb::FrontendUpdate {
            return self.frontend_update(order, now, memory);
        }
        let before = self.state()?;
        if order.verb == Verb::Activate
            && let Err(why) = self.prepare(order, &before, memory)
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
            Verb::Activate => self.activate(order, now),
            Verb::Confirm => self.confirm(order, &before),
            Verb::Revert => self.revert(),
            Verb::FrontendUpdate => unreachable!("taken above"),
        };
        let moved = self.moved(&before)?;
        match result {
            Ok(done) => {
                memory.event = done.event;
                memory.refusal = done.note;
                memory.restart_due = prior | done.restart | moved;
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
    fn prepare(
        &mut self,
        order: &Order,
        before: &State,
        memory: &mut Memory,
    ) -> Result<(), String> {
        let id = order.id;
        // Fail closed: an activation without the release's claims is not taken.
        let Some(claims) = &order.claims else {
            return Err(format!(
                "order {id}: an activation carries no release claims"
            ));
        };
        let (prefix, needs) = match self.host.platform() {
            Platform::Linux => ("linux-", ["kbf"].as_slice()),
            Platform::MacOs => ("darwin-", ["kbf-daemon"].as_slice()),
        };
        if !claims.platform.starts_with(prefix) {
            return Err(format!(
                "order {id} is for platform {:?}, not this host's ({prefix}*)",
                claims.platform
            ));
        }
        let mut names: Vec<&str> = order.components.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        if names != needs {
            return Err(format!(
                "order {id} names components {names:?}; this platform needs exactly {needs:?}"
            ));
        }
        // A frontend update moves `current` when it is confirmed: not while one is in flight.
        node_waits_for_frontend(self.frontend_in_flight, id)?;
        // One confirm window at a time, never two timers. The same order again (a pass
        // that died after the swap) is the repeat `activate` recognises.
        let this = format!("{}-{}", order.epoch, order.id);
        if let Some(p) = &before.pending_confirm
            && p.order_id.as_deref() != Some(this.as_str())
        {
            return Err(format!(
                "order {id}: {} is on trial and not yet confirmed or reverted; not activating",
                p.commit
            ));
        }
        // The lease check again, from what the installer observes.
        match self.leases.live() {
            Ok(0) => {}
            Ok(n) => return Err(format!("{n} leases are live; not activating")),
            Err(why) => return Err(format!("cannot tell whether leases are live: {why}")),
        }
        // Already the running release: nothing to fetch, `activate` says so.
        if before.current.as_ref() == Some(&order.commit) {
            return Ok(());
        }
        // A directory on the host is not proof: the fetcher verifies the release whether it
        // pulls it or finds it.
        let rec = ReleaseLocator {
            commit: order.commit.clone(),
            components: order.components.clone(),
        };
        // The node is told a fetch is under way; it may take minutes.
        memory.fetching = true;
        self.report(memory).map_err(|e| e.to_string())?;
        let fetched = self.fetcher.fetch(&rec);
        memory.fetching = false;
        let got = fetched.map_err(|e| format!("order {id}: {e}"))?;
        // What the fetched binary declared must be what the order expected, before a link
        // moves: a release that is attested but is another build than the one ordered is
        // refused.
        if let Err(why) = matches_claims(&got, order, claims) {
            return Err(format!(
                "order {id}: the fetched release is not the one ordered: {why}"
            ));
        }
        memory.staged = Some(facts(&got));
        Ok(())
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

    fn activate(&mut self, order: &Order, now: u64) -> Result<Done, String> {
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
            Ok(Activated::Swapped | Activated::Repeat) => Ok(Done::restart()),
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
            .and_then(|p| p.order_id.clone())
            .unwrap_or_default();
        match activate::confirm(&self.host, &activation, &order.commit) {
            Ok(Confirmed::Kept | Confirmed::AlreadyKept) => Ok(Done::quiet(Event::Confirmed)),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn revert(&mut self) -> Result<Done, String> {
        match activate::revert(&mut self.exec, &self.host, &mut self.log) {
            Ok(r) => Ok(Done {
                event: Event::Reverted,
                note: format!("reverted by order: {} is current", r.to),
                restart: true,
            }),
            Err(e) => Err(release_err(&e)),
        }
    }

    /// A `frontend_update` order: handed to the frontend step, which writes its own record and
    /// owes no restart of the node's service. Refused when this host has no frontend set up.
    fn frontend_update(
        &mut self,
        order: &Order,
        now: u64,
        memory: &mut Memory,
    ) -> Result<(), HelperError> {
        let Some(frontend) = &self.frontend else {
            refuse(
                memory,
                "frontend_update: this installer was not set up to update a frontend",
            );
            return self.save(memory);
        };
        match frontend.begin(&mut self.exec, &self.host, &*self.fetcher, order, now)? {
            Begun::Refused(why) => refuse(memory, &why),
            Begun::Started => {
                self.frontend_in_flight = true;
                memory.event = Event::Idle;
                memory.refusal = String::new();
            }
            Begun::AlreadyRunning => {
                memory.event = Event::Idle;
                memory.refusal = format!("{} already runs on the frontend", order.commit);
            }
        }
        self.save(memory)
    }

    /// Move a pending frontend update one step, and say what came of it.
    fn advance_frontend(&mut self, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        let Some(frontend) = &self.frontend else {
            return Ok(());
        };
        let before = memory.clone();
        let progress = frontend.advance(&mut self.exec, &self.host, now, &mut self.log)?;
        self.frontend_in_flight =
            matches!(progress, Progress::Waiting | Progress::NeedsOperator(_));
        match progress {
            Progress::Idle | Progress::Waiting => return Ok(()),
            Progress::Confirmed(_) => {
                memory.event = Event::Confirmed;
                memory.refusal = String::new();
            }
            Progress::Reverted(why) => {
                memory.event = Event::Reverted;
                memory.refusal = format!("the frontend update was undone: {why}");
            }
            Progress::NeedsOperator(why) => refuse(memory, &format!("needs_operator: {why}")),
        }
        // Said once, not rewritten every pass while an operator is awaited.
        if *memory == before {
            return Ok(());
        }
        self.save(memory)
    }

    fn report(&mut self, memory: &Memory) -> Result<(), HelperError> {
        let mut memory = memory.clone();
        memory.frontend_pending = self.frontend_in_flight;
        let memory = &memory;
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

/// What an order did.
struct Done {
    event: Event,
    note: String,
    restart: bool,
}

impl Done {
    fn restart() -> Done {
        Done {
            event: Event::Idle,
            note: String::new(),
            restart: true,
        }
    }

    fn quiet(event: Event) -> Done {
        Done {
            event,
            note: String::new(),
            restart: false,
        }
    }
}

/// Whether acting on an order of `verb` moves its kind's floor. All do, but a frontend update
/// on an installer with no frontend set up: refused like any record the reader does not take,
/// it moves no floor, so the next real order still runs.
fn moves_floor(verb: Verb, frontend_set_up: bool) -> bool {
    verb != Verb::FrontendUpdate || frontend_set_up
}

/// A node `activate` waits while a frontend update is in flight: the update's flip moves
/// `current`.
fn node_waits_for_frontend(in_flight: bool, id: u64) -> Result<(), String> {
    if in_flight {
        Err(format!(
            "order {id}: a frontend update is in flight; not activating"
        ))
    } else {
        Ok(())
    }
}

fn refuse(memory: &mut Memory, why: &str) {
    memory.event = Event::Refused;
    memory.refusal = why.to_owned();
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

/// Whether the release the fetcher installed is the one the order named and expected.
pub(crate) fn matches_claims(
    got: &InstalledRelease,
    order: &Order,
    claims: &Claims,
) -> Result<(), String> {
    if got.commit != order.commit {
        return Err(format!("it is commit {}", got.commit.as_str()));
    }
    let same = (
        &got.platform,
        got.protocol,
        got.min_protocol,
        got.state_schema,
        &got.profile,
    ) == (
        &claims.platform,
        claims.protocol,
        claims.min_protocol,
        claims.state_schema,
        &claims.profile,
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
            claims.platform,
            claims.protocol,
            claims.min_protocol,
            claims.state_schema,
            claims.profile
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
