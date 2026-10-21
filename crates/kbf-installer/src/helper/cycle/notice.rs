//! What the helper tells, at the end of each pass, of the host's package and its source: the
//! seed it carries when the seed is whole and above the floor of `seq` (`available` in
//! `status.json`), the word `installer.toml` holds for `release_source`, and the floor itself,
//! which is kept up with the release that runs. The rules are in [`crate::seed`] and
//! [`crate::seq_floor`]; this is where the pass applies them.
//!
//! A look is a report and nothing else. It writes no state, stages nothing, moves no link and
//! orders nothing: the only file it can make is the floor, which says what the host has
//! already confirmed. A seed is announced only after the fetcher has found it whole (the
//! package's files, the trust files they were made beside, the attestation), and only if its
//! `seq` is above the floor by the rule an order meets, so what is announced is what an order
//! would be taken on. The package is checked in full once per manifest, not at every pass; why
//! a seed is not announced is said in the log, once, and not in the status, which an operator
//! reads for the orders it answers.
//!
//! The floor is the highest `seq` the host has confirmed (see [`crate::seq_floor`]): while no
//! swap is on trial it is kept at least at the `seq` of the release that runs, asked of that
//! release's own binary once per release. So it starts, on a host that had none, at the release
//! the host already runs; it rises when a swap is confirmed (by an order, or by the operator:
//! the next pass catches up); a swap on trial raises nothing, so that a trial that is reverted
//! does not burn the seed; and a rollback lowers nothing.

use std::io::Write;

use kbf_host::Exec;

use super::Helper;
use crate::CommitId;
use crate::helper::fetcher::FetchError;
use crate::helper::memory::Memory;
use crate::seed::{self, Available, Looked};
use crate::seq_floor;

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
}

impl Notice {
    /// What the last pass found, put into `memory` for the status this pass writes.
    pub(super) fn stamp(&self, memory: &mut Memory) {
        memory.available.clone_from(&self.available);
        memory.release_source = self.source;
    }
}

impl<E: Exec> Helper<E> {
    /// Look at the package and the source, after the orders of the pass: the floor is caught up
    /// with the release that runs, the seed (if the package carries one) is checked, and the
    /// status that is written next tells what came of it.
    pub(super) fn notice_package(&mut self, memory: &mut Memory) {
        let noticed = self.fetcher.look(self.notice.looked.as_ref());
        let floor = self.synced_floor();
        self.notice.looked = None;
        let verdict = match noticed.seed {
            Ok(None) | Err(FetchError::NotWired) => Ok(None),
            Err(e) => Err(e.to_string()),
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
        self.notice.stamp(memory);
    }

    /// After a swap was confirmed: bring the floor up to the release that runs now. A failure
    /// is said at once and the confirm stands; the next pass tries again. Whatever this step
    /// says to the log begins `package: `.
    pub(super) fn raise_floor(&mut self) {
        if let Err(why) = self.synced_floor() {
            let _ = writeln!(self.log, "package: {why}");
        }
    }

    /// The floor, once it has caught up with the release that runs. While a swap is on trial
    /// nothing is raised. A state that cannot be read is no reason to say more: the pass has
    /// read it already and would have stopped.
    fn synced_floor(&mut self) -> Result<Option<u64>, String> {
        let state = self.state().unwrap_or_default();
        if state.pending_confirm.is_none()
            && let Some(current) = &state.current
        {
            self.raise_to(current)?;
        }
        seed::read_floor(&self.host).map_err(|e| e.to_string())
    }

    /// Raise the floor to the `seq` of the installed release `current`, asking the release for
    /// it the first time. A fetcher that cannot run a binary leaves the floor as it is, and a
    /// release that declares no `seq` raises nothing.
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
        let raised = seq_floor::raise(&self.host, seq);
        raised.map(|_| ()).map_err(|e| e.to_string())
    }
}
