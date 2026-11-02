//! The root half of the frontend proxy's three orders. Nothing writes them since the proxy's
//! update API was removed; the root half still understands them (`kbf-installer.md`
//! section 6). None of them moves a unit, so none owes a restart:
//!
//! - `frontend_update{commit}`: the fetcher pulls, verifies and installs the release into
//!   `releases/<commit>/` (no component list: the source is the installer's own, or, for an
//!   order whose source is `staged`, the release the host's package carries); once the
//!   checks below pass, this half (never the fetcher) makes the state's `staged` name it,
//!   and its facts go to `status.json` under `staged{}`. No link moves.
//! - `confirm{commit}`: if `commit` is the staged release and no node swap is on trial,
//!   `previous` then `current` are flipped to it, state first. A repeat finds it current.
//! - `revert{commit}`: if `commit` is the staged release it is dropped from `staged`.
//!   `current` is never touched; a repeat finds nothing staged and is a no-op.
//!
//! A refusal is a result like any other: `<word>: <why>` in `status.json`'s `refusal`, the
//! word one of KI section 5's, and nothing changes. `window_s` is the confirm window of the
//! node's swap and means nothing here: a staging has no timer of its own except the
//! fetcher's `fetch_timeout_s`.

use kbf_host::Exec;
use kbf_update::Source;

use super::{Done, FrontendStep, Helper, Memory, Order, facts, platform_prefix, refuse};
use crate::activate::{self, Confirmed};
use crate::helper::fetcher::{FetchError, ReleaseLocator};
use crate::helper::memory::Event;
use crate::release::ReleaseError;
use crate::state;

/// Why an order was not carried out.
struct Refused {
    word: &'static str,
    why: String,
}

impl Refused {
    fn new(word: &'static str, why: impl Into<String>) -> Refused {
        Refused {
            word,
            why: why.into(),
        }
    }
}

impl From<ReleaseError> for Refused {
    fn from(e: ReleaseError) -> Refused {
        let word = match e {
            ReleaseError::FrontendBusy(_) => "frontend_busy",
            ReleaseError::AlreadyInstalled(_) | ReleaseError::NotStaged { .. } => "commit_or_ref",
            ReleaseError::Pending(_) => "pending",
            _ => "stage",
        };
        Refused::new(word, e.to_string())
    }
}

impl Refused {
    /// The refusal of order `id` whose fetch said `e`. A refusal of the order itself carries
    /// its own word (`denied`, `commit_or_ref`) and is said with the order it is for.
    fn of_fetch(e: FetchError, id: u64) -> Refused {
        // `stage` is the word of a failure the installer cannot place; the real fetcher's
        // rejections become the verify words when it is wired.
        let word = match e {
            FetchError::Unavailable(_) | FetchError::NotWired => "fetch",
            FetchError::Rejected(_) => "stage",
            FetchError::Order { word, why } => {
                return Refused::new(word, format!("order {id}: {why}"));
            }
        };
        Refused::new(word, e.to_string())
    }
}

impl<E: Exec> Helper<E> {
    /// Carry out one of the proxy's orders; what came of it goes into `memory`.
    pub(super) fn frontend(&mut self, step: FrontendStep, order: &Order, memory: &mut Memory) {
        // Releases are collected after a confirm only: a dropped stage is no confirm.
        let confirm = matches!(step, FrontendStep::Confirm);
        let result = match step {
            FrontendStep::Update => self.stage_release(order, memory),
            FrontendStep::Confirm => self.flip(order),
            FrontendStep::Revert => self.drop_staged(order),
        };
        match result {
            Ok(done) => {
                memory.event = done.event;
                memory.refusal = done.note;
                self.gc_due |= done.kept && confirm;
                if done.kept {
                    self.raise_floor();
                }
            }
            Err(r) => refuse(memory, &format!("{}: {}", r.word, r.why)),
        }
    }

    fn stage_release(&mut self, order: &Order, memory: &mut Memory) -> Result<Done, Refused> {
        let before = state::read_unlocked(&self.host)
            .map_err(ReleaseError::from)?
            .unwrap_or_default();
        // Before anything is fetched: a refused order costs no download.
        activate::check_stageable(&before, &order.commit)?;
        // The package's release is held to the same rules of the seed as a node's staged
        // activation, also before anything is done.
        if order.source == Source::Staged {
            let admitted = self.admitted_seed(&order.commit);
            admitted.map_err(|e| Refused::of_fetch(e, order.id))?;
        }
        let rec = ReleaseLocator {
            commit: order.commit.clone(),
            components: Vec::new(),
            source: order.source,
        };
        memory.fetching = true;
        let told = self.report(memory);
        let fetched = match told {
            Ok(()) => {
                let pulled = self.fetcher.fetch(&rec);
                pulled.map_err(|e| Refused::of_fetch(e, order.id))
            }
            Err(e) => Err(Refused::new("stage", e.to_string())),
        };
        memory.fetching = false;
        let got = fetched?;
        if got.commit != order.commit {
            return Err(Refused::new(
                "facts",
                format!("the fetched release is commit {}", got.commit.as_str()),
            ));
        }
        let prefix = platform_prefix(&self.host);
        if !got.platform.starts_with(prefix) {
            return Err(Refused::new(
                "platform",
                format!(
                    "the fetched release is for platform {:?}, not this host's ({prefix}*)",
                    got.platform
                ),
            ));
        }
        activate::stage_frontend(&self.exec, &self.host, &order.commit)?;
        memory.staged = Some(facts(&got));
        memory.staged_profile = Some(got.profile.clone());
        Ok(Done::quiet(Event::Idle))
    }

    fn flip(&mut self, order: &Order) -> Result<Done, Refused> {
        let kept =
            activate::confirm_frontend(&mut self.exec, &self.host, &order.commit, &mut self.log)?;
        Ok(Done {
            kept: kept == Confirmed::Kept,
            ..Done::quiet(Event::Confirmed)
        })
    }

    fn drop_staged(&mut self, order: &Order) -> Result<Done, Refused> {
        let dropped = activate::revert_frontend(&self.host, &order.commit)?;
        let note = if dropped {
            format!("{} dropped from staged; current untouched", order.commit)
        } else {
            String::new()
        };
        Ok(Done {
            note,
            kept: dropped,
            ..Done::quiet(Event::Reverted)
        })
    }
}
