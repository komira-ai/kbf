//! What the helper tells, at the end of each pass, of the host's package and its source: the
//! seed it carries when the seed is whole and above the floor of `seq` (`available` in
//! `status.json`), the word `installer.toml` holds for `release_source`, and the floor itself,
//! which is kept up with the release that runs. The rules are in [`crate::seed`] and
//! [`crate::seq_floor`]; this is where the pass applies them.
//!
//! A look is a report and nothing else. It writes no state, stages nothing, moves no link and
//! orders nothing. The files it makes are the scratch directory `work/<commit>.look`, which
//! `gh` is run in and which is gone when the look returns, and the floor (below). A seed is
//! announced only after the fetcher has found it whole (the package's files, the trust files
//! they were made beside, the attestation), and only if its `seq` is above the floor by the
//! rule an order meets, so what is announced is what an order would be taken on. A package
//! found whole is checked in full once per manifest, not at every pass: while the manifest is
//! the one it was found by the answer stands, and a file changed under the same manifest is
//! found when an order is taken on it, which checks everything again. A package that fails its
//! look is looked at again after a minute and not before, so that one that stays broken does
//! not cost a read of the tarball and a run of `gh` at every pass. Why a seed is not announced
//! is said in the log, once, and not in the status, which an operator reads for the orders it
//! answers.
//!
//! The floor is the highest `seq` the host has confirmed (see [`crate::seq_floor`]): while no
//! swap is on trial it is kept at least at the `seq` of the release that runs, asked of that
//! release's own binary once per release. So it starts, on a host that had none, at the release
//! the host already runs; it rises when a swap is confirmed (by an order, which raises it at
//! once, or by the operator: the next pass, or the next staged order, whichever comes first,
//! catches up); a swap on trial raises nothing, so that a trial that is reverted does not burn
//! the seed; and a rollback lowers nothing. A staged order catches the floor up before it is
//! judged (`catch_up`), so that none is judged against a floor that is behind the release
//! that runs.

use std::io::Write;

use kbf_host::Exec;

use super::Helper;
use crate::CommitId;
use crate::helper::fetcher::FetchError;
use crate::helper::memory::Memory;
use crate::seed::{self, Available, Looked};
use crate::seq_floor;

/// Seconds a failed look waits before the package is looked at again: long enough that a
/// package that stays broken is not read in full at every pass, short enough that one that was
/// half way through an upgrade is found whole soon after.
const RETRY_SECS: u64 = 60;

/// What the helper remembers from one pass to the next about the package and the source.
#[derive(Default)]
pub(super) struct Notice {
    /// The seed last found whole, by the digest of the manifest it was found by: while the
    /// manifest is the same one the package is not checked again.
    looked: Option<Looked>,
    /// The seed told available and the source told, as of the last pass: put in the status
    /// again at the start of the next, so that a pass that spends minutes on a fetch does not
    /// take them out of it.
    available: Option<Available>,
    source: Option<&'static str>,
    /// What the log was last told about the package, so that a trouble that stays is said once.
    told: String,
    /// The release that runs, as last asked for its `seq` (`None` when it declares none). A
    /// failure to ask is not kept: the next pass asks again.
    asked: Option<(CommitId, Option<u64>)>,
    /// When (the `now` of the pass) the last look failed, if the last look did.
    failed_at: Option<u64>,
}

impl Notice {
    /// What the last pass found, put into `memory` for the status this pass writes.
    pub(super) fn stamp(&self, memory: &mut Memory) {
        memory.available.clone_from(&self.available);
        memory.release_source = self.source;
    }

    /// Whether the last look failed less than [`RETRY_SECS`] before `now`. A clock that
    /// stepped back holds nothing up.
    fn held(&self, now: u64) -> bool {
        let age = self.failed_at.and_then(|at| now.checked_sub(at));
        age.is_some_and(|age| age < RETRY_SECS)
    }
}

impl<E: Exec> Helper<E> {
    /// Look at the package and the source, after the orders of the pass: the floor is caught up
    /// with the release that runs, the seed (if the package carries one) is checked unless the
    /// last look failed a moment ago, and the status that is written next tells what came of it.
    pub(super) fn notice_package(&mut self, now: u64, memory: &mut Memory) {
        let floor = self.synced_floor();
        if !self.notice.held(now) {
            self.look_at_package(now, floor);
        }
        self.notice.stamp(memory);
    }

    /// One look at the package, judged against `floor`: what is to be told and said is kept in
    /// `self.notice`.
    fn look_at_package(&mut self, now: u64, floor: Result<Option<u64>, String>) {
        let noticed = self.fetcher.look(self.notice.looked.as_ref());
        self.notice.looked = None;
        self.notice.failed_at = None;
        let verdict = match noticed.seed {
            Ok(None) | Err(FetchError::NotWired) => Ok(None),
            Err(e) => {
                self.notice.failed_at = Some(now);
                Err(e.to_string())
            }
            Ok(Some(looked)) => {
                let seed = looked.seed.clone();
                self.notice.looked = Some(looked);
                let judged = floor.and_then(|floor| seed::above_floor(seed.seq, floor));
                judged.map(|()| Some(Available::of(&seed)))
            }
        };
        let (available, said) = match verdict {
            Ok(available) => (available, String::new()),
            Err(said) => (None, said),
        };
        self.notice.available = available;
        self.notice.source = noticed.source;
        if said != self.notice.told {
            if !said.is_empty() {
                let _ = writeln!(self.log, "package: {said}");
            }
            self.notice.told = said;
        }
    }

    /// After a swap was confirmed: bring the floor up to the release that runs now. A failure
    /// is said at once and the confirm stands; the next pass tries again. Whatever this step
    /// says to the log begins `package: `.
    pub(super) fn raise_floor(&mut self) {
        if let Err(why) = self.synced_floor() {
            let _ = writeln!(self.log, "package: {why}");
        }
    }

    /// The floor, once it has caught up with the release that runs (see `catch_up`).
    fn synced_floor(&mut self) -> Result<Option<u64>, String> {
        self.catch_up()?;
        seed::read_floor(&self.host).map_err(|e| e.to_string())
    }

    /// Bring the floor up to the `seq` of the release that runs, unless a swap is on trial (a
    /// trial that is reverted must not burn the seed). A state that cannot be read is no reason
    /// to say more: the pass has read it already and would have stopped. A staged order calls
    /// this before it is judged, and refuses if it fails: the floor it would read may be behind
    /// the release that runs (an operator's own `confirm`, a crash between a confirm and its
    /// raise), and an order for a package older than that release is the downgrade the floor
    /// is there to stop.
    pub(super) fn catch_up(&mut self) -> Result<(), String> {
        let state = self.state().unwrap_or_default();
        if state.pending_confirm.is_none()
            && let Some(current) = &state.current
        {
            self.raise_to(current)?;
        }
        Ok(())
    }

    /// Raise the floor to the `seq` of the installed release `current`, asking the release for
    /// it the first time. A fetcher that cannot run a binary leaves the floor as it is, and a
    /// release that declares no `seq` raises nothing. The lock is not taken when the floor is
    /// there already.
    fn raise_to(&mut self, current: &CommitId) -> Result<(), String> {
        let seq = match self.notice.asked.clone() {
            Some((commit, seq)) if commit == *current => seq,
            _ => match self.fetcher.probe(current) {
                Ok(version) => {
                    self.notice.asked = Some((current.clone(), version.seq));
                    version.seq
                }
                Err(FetchError::NotWired) => return Ok(()),
                Err(e) => {
                    let why = format!("seq_floor: cannot ask {current} for its seq: {e}");
                    return Err(why);
                }
            },
        };
        let Some(seq) = seq else { return Ok(()) };
        let held = seed::read_floor(&self.host);
        if held.is_ok_and(|floor| floor.is_some_and(|floor| floor >= seq)) {
            return Ok(());
        }
        let raised = seq_floor::raise(&self.host, seq);
        raised.map(|_| ()).map_err(|e| e.to_string())
    }
}
