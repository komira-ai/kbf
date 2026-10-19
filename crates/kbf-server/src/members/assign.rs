//! The member id and boot a registration is given, and how a node's own history raises them
//! (`docs/design/frontend-proxy-u0.md`, section (e)).
//!
//! A storage node's metadata log refuses a boot of a member that is not after the latest it
//! set for that member (`kbf_meta::StaleWriter`). A state store that is new (a fresh
//! Postgres, a restored older copy) counts from 1, so a node that had run before would exit
//! 75 and restart once per boot it ever had before it opened. A registration therefore
//! carries [`LastBoot`], the member and boot the node's log last wrote as, and the store
//! gives the node a boot above it.
//!
//! **A new instance** (no `instance/<name>` row) is given the member id its log names when
//! that id is free, and otherwise a new id from the `member-id` counter, as before. An id is
//! free when no `instance/` row has it and `member-owner/<id>` is absent; the claim is
//! written with the instance row (`Cond::Absent`), so two nodes naming one id, or a node
//! naming an id the counter hands out at that moment, cannot both get it. When it takes the
//! id, the same transaction that creates the instance and its `unfilled/<id>` mark raises
//! `boot/<id>` by the boot reported plus one, so the first boot is above the log's. The
//! counter only goes up, whatever it held.
//!
//! **A known instance** keeps its id and takes the next boot, as ever. If its log names that
//! same id with a boot at or above what the store gave (a store restored from an older
//! copy), the counter is raised until the boot is above the log's. A report that names
//! another id changes nothing: the id the store holds for the instance stands.
//!
//! **Bounds.** The counter has one operation, "next", so a raise is that many of them in
//! one transaction; the boot a node reports is clamped to [`MAX_SEED`], and a node whose
//! log is further ahead restarts for the rest, as it did before. A node that lies about its
//! history can only waste its own boot numbers (and, naming a free id, take it before a
//! node that would have drawn it): it cannot take an id an instance holds, lower a counter,
//! or skip the joiner mark.

use kbf_map::MemberId;
use kbf_state::{Cond, Txn};

use super::{ATTEMPTS, ID_COUNTER, Members, MembershipError, boot_counter, decode_id};
use super::{instance_key, owner_key};

/// The most boots a node's report raises a counter by in one registration.
pub const MAX_SEED: u64 = 4096;

/// The member and boot a node's metadata log last wrote as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastBoot {
    /// The member id the log set last.
    pub member: u64,
    /// That member's boot.
    pub boot: u64,
}

impl LastBoot {
    /// The report as the wire carries it, `None` when it says nothing (a zero member or
    /// boot names no writer).
    #[must_use]
    pub const fn from_wire(member: u64, boot: u64) -> Option<Self> {
        if member == 0 || boot == 0 {
            None
        } else {
            Some(Self { member, boot })
        }
    }

    /// The boot to raise a counter past, within [`MAX_SEED`].
    const fn floor(self) -> u64 {
        if self.boot > MAX_SEED {
            MAX_SEED
        } else {
            self.boot
        }
    }
}

impl Members {
    /// The id of `instance` (assigned on first sight, with its `unfilled/` mark if the farm
    /// holds data) and a new boot counter for it, above `last` when it names the id.
    pub(super) fn assign(
        &self,
        instance: &str,
        last: Option<LastBoot>,
    ) -> Result<(MemberId, u64), MembershipError> {
        let key = instance_key(instance);
        for _ in 0..ATTEMPTS {
            let (id, txn) = match self.store.get(&key)? {
                Some(known) => {
                    let id = MemberId(decode_id(&known.value)?);
                    let txn = Txn::new().check(&key, Cond::Version(known.version));
                    if let Ok(commit) = self.store.txn(txn.next(boot_counter(id)))? {
                        return self.raise(id, commit.counters[0], last);
                    }
                    continue;
                }
                None => self.new_instance(&key, instance, last)?,
            };
            if let Ok(commit) = self.store.txn(txn)? {
                let boot = commit.counters.last().copied().unwrap_or(1);
                return Ok((id, boot));
            }
        }
        Err(MembershipError::Contended)
    }

    /// The id and the creating transaction for an instance the store has not seen.
    fn new_instance(
        &self,
        key: &str,
        instance: &str,
        last: Option<LastBoot>,
    ) -> Result<(MemberId, Txn), MembershipError> {
        let named = match last {
            Some(last) if self.id_is_free(MemberId(last.member))? => Some(last),
            _ => None,
        };
        let (id, seeds) = match named {
            Some(last) => (MemberId(last.member), last.floor() + 1),
            None => {
                // A counter has no precondition, so it cannot conflict.
                let taken = self.store.txn(Txn::new().next(ID_COUNTER))?;
                let id = MemberId(taken.map_err(|_| MembershipError::Contended)?.counters[0]);
                (id, 1)
            }
        };
        let txn = Txn::new().put(key, id.0.to_be_bytes(), Cond::Absent).put(
            owner_key(id),
            instance.as_bytes(),
            Cond::Absent,
        );
        let mut txn = self.unfilled_if_data(txn, id)?;
        for _ in 0..seeds {
            txn = txn.next(boot_counter(id));
        }
        Ok((id, txn))
    }

    /// Whether no instance holds `id`: no `member-owner/` claim and no `instance/` row
    /// whose value is it (rows written before the claim existed have no claim).
    fn id_is_free(&self, id: MemberId) -> Result<bool, MembershipError> {
        if self.store.get(&owner_key(id))?.is_some() {
            return Ok(false);
        }
        for (_, row) in self.store.scan_prefix("instance/")? {
            if decode_id(&row.value)? == id.0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `boot` of a known instance `id`, lifted above `last` if `last` names this id.
    fn raise(
        &self,
        id: MemberId,
        boot: u64,
        last: Option<LastBoot>,
    ) -> Result<(MemberId, u64), MembershipError> {
        let Some(last) = last.filter(|l| l.member == id.0 && boot <= l.floor()) else {
            return Ok((id, boot));
        };
        let mut txn = Txn::new();
        for _ in 0..=(last.floor() - boot) {
            txn = txn.next(boot_counter(id));
        }
        let lifted = self
            .store
            .txn(txn)?
            .ok()
            .ok_or(MembershipError::Contended)?;
        let boot = lifted.counters.last().copied().unwrap_or(boot);
        tracing::info!(
            member = id.0,
            boot,
            "boot counter raised above the node's log"
        );
        Ok((id, boot))
    }
}
