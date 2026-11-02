//! An operator's force: roll a service to a chosen version past every health gate
//! (`docs/design/gamma-cicd.md`, C1; `docs/deploy/kbf-updater.md`, "Emergency: force a
//! version").
//!
//! `{"action":"force","service":S,"version":V,"reason":R,"max_updating":{role:n}}` on
//! `POST /v1/operator`, with the operator token only. It is refused for a version older than
//! the service's `min_version` (the first release with the host-updates hook: an older one is
//! put on a host by hand, by its first install). Otherwise, in one write of the ops store,
//! each part audited ([`OpsStore::decide_changes`]):
//!
//! - `force`: the force is recorded ([`Force`]) with the next force generation and the raised
//!   `max_updating`;
//! - `unmark_bad`: `V` loses its bad mark, if it had one;
//! - `mark_bad`: the version the desired state named before (the version left) is marked bad,
//!   and held so: CI's desired state does not lift that mark, only an operator's `unmark_bad`
//!   does ([`before_ci_write`]), so CI cannot roll it out again by accident and the automatic rollback
//!   never goes back to it;
//! - `clear_halt`: the halt is cleared by the one path of a clear ([`crate::ops::clear_halt`]):
//!   the reports are acknowledged and the clear generation goes up, so an agent that holds a
//!   halt lifts it and takes the order. The generation goes up when no halt stands, too.
//!
//! The failure reports of `V` the hosts make at that moment are acknowledged in the same write
//! (`failure_acked`): a host that failed `V` before is ordered again, as a new desired version
//! would order it, as often as the order is lost or declined. The acknowledgement goes when the
//! host's beat shows it updating (`acks::stale_acks`), so a failure after that attempt holds
//! the host as rule 4 does.
//!
//! Then the desired state is set to `V`, its other fields as they were (none before: the
//! default limits, no minimum). The fleet's lock is held across both, so no heartbeat is decided
//! between them. A desired state that cannot be written fails the force with the marks written:
//! the operator asks again.
//!
//! The force stands while the desired version is `V` ([`standing`]). While it stands the
//! decision is [`crate::decide::decide_forced`]: the canary, the bake, `min_up`, a deferral,
//! the failures acknowledged above, a bad mark and a halt hold no host back; the quiet start,
//! the role order, the order between services and the one update at a time of a host stay, and
//! so does `max_updating`, as raised. A host that runs `V` holds no slot, healthy or not; one
//! whose agent did not announce [`FEATURE`] holds its slot the ordinary way, since it goes back
//! by its own rule when `V` is not healthy. The automatic rollback does nothing while a force
//! stands but record the last good version: `V`, once it has converged and baked
//! ([`crate::reconcile`]).
//!
//! An order given while a force stands carries `forced: true` to an agent that announced
//! [`FEATURE`]; an older agent gets the order it has always read. The next desired state CI
//! sets ends the force once that state is written ([`end_after_ci_write`]): a state that
//! cannot be written leaves the force standing.

use std::collections::BTreeMap;

use kbf_updater::version::Version;
use kbf_updater::wire::Outcome;

use crate::desired::{Desired, DesiredStore, MAX_COUNT, RoleLimits};
use crate::fleet::Fleet;
use crate::ops::{
    self, AuditEntry, FailureAck, Force, OperateError, OpsError, OpsState, OpsStore, ServiceOps,
    Write,
};
use crate::reconcile::CI;
use crate::services::{ServiceDef, Services};

/// The feature an agent announces to be told that an order is forced.
pub const FEATURE: &str = "forced-v1";

/// A force an operator asked for.
pub(crate) struct Request {
    pub service: String,
    pub version: Version,
    pub reason: String,
    pub max_updating: BTreeMap<String, u32>,
}

/// The force of `service` in `marks` that stands under `desired`: the one whose version is the
/// desired version.
#[must_use]
pub(crate) fn standing<'a>(marks: &'a ServiceOps, desired: Option<&Desired>) -> Option<&'a Force> {
    let desired = desired?;
    marks
        .force
        .as_ref()
        .filter(|force| force.version == desired.version)
}

/// Whether a force of `service` stands in `state` under `desired`.
#[must_use]
pub(crate) fn stands(state: &OpsState, service: &str, desired: Option<&Desired>) -> bool {
    state
        .services
        .get(service)
        .and_then(|marks| standing(marks, desired))
        .is_some()
}

fn entry(by: &str, action: &str, service: &str, reason: String) -> AuditEntry {
    let mut entry = AuditEntry::new(by, action);
    entry.service = Some(service.to_owned());
    entry.reason = Some(reason);
    entry
}

/// The raised limits of `request` for `def`: each a role of the service, 1 to [`MAX_COUNT`].
fn limits(def: &ServiceDef, asked: &BTreeMap<String, u32>) -> Result<(), OperateError> {
    if asked.keys().any(|role| !def.roles.contains_key(role)) {
        return Err(OperateError::Invalid(
            "max_updating names a role the service does not run on",
        ));
    }
    if asked.values().any(|n| !(1..=MAX_COUNT).contains(n)) {
        return Err(OperateError::Invalid(
            "max_updating is 1 to 10000 hosts per role",
        ));
    }
    Ok(())
}

/// The desired state of a force to `version` of `def`, when the service had none: the default
/// limits, and no host that must stay up.
fn first_desired(def: &ServiceDef, version: Version) -> Desired {
    let mut min_up = RoleLimits::default();
    for role in &def.require_min_up {
        min_up.set(role, 0);
    }
    Desired {
        version,
        max_updating: RoleLimits::max_updating_default(),
        min_up,
    }
}

/// The failure reports of `version` the hosts of `service` make now, one per host.
fn failures(fleet: &Fleet, service: &str, version: &Version) -> Vec<FailureAck> {
    fleet
        .hosts_of(service)
        .filter_map(|(id, host)| {
            let report = host.beat.last_update.as_ref()?;
            (report.outcome == Outcome::Failed && report.target == *version).then(|| FailureAck {
                node_id: id.to_string(),
                report: report.clone(),
            })
        })
        .collect()
}

/// Makes the force `request`, as `by`, of a service of `services`: the marks in `ops`, then
/// the desired state in `store` (the module docs say what and in which order). The caller holds
/// the fleet's lock, `fleet`, across it.
///
/// # Errors
/// [`OperateError::Invalid`]: an unknown service, a bad reason or bad limits.
/// [`OperateError::BelowMinVersion`]: the version is older than the service's `min_version`.
/// [`OperateError::Store`]: the ops file could not be written, and nothing changed.
/// [`OperateError::Desired`]: the desired state could not be written after the marks were.
pub(crate) fn force(
    ops: &OpsStore,
    services: &Services,
    store: &DesiredStore,
    fleet: &Fleet,
    by: &str,
    request: Request,
) -> Result<AuditEntry, OperateError> {
    let def = services.get(&request.service).ok_or(OperateError::Invalid(
        "service is not one this coordinator manages",
    ))?;
    let name = def.name.clone();
    let why = ops::reason(&request.reason)?;
    limits(def, &request.max_updating)?;
    let version = request.version;
    if let Some(floor) = def.min_version.as_ref().filter(|f| !version.at_least(f)) {
        tracing::warn!(service = %name, %version, %floor, "refused a force below min_version");
        return Err(OperateError::BelowMinVersion {
            version,
            floor: floor.clone(),
        });
    }
    let before = store.get(&name);
    let left = before
        .as_ref()
        .map(|desired| desired.version.clone())
        .filter(|left| *left != version);
    let acks = failures(fleet, &name, &version);
    let head = ops.decide_changes::<OperateError>(Write::BeforeMemory, |state| {
        let marks = state.services.entry(name.clone()).or_default();
        marks.force_generation = marks.force_generation.saturating_add(1);
        let generation = marks.force_generation;
        marks.force = Some(Force {
            version: version.clone(),
            generation,
            reason: why.clone(),
            max_updating: request.max_updating,
        });
        let head = entry(
            by,
            "force",
            &name,
            format!("{version}, generation {generation}: {why}"),
        );
        let mut parts = Vec::new();
        if marks.bad.contains(&version) {
            marks.bad.retain(|bad| *bad != version);
            marks.bad_by_force.retain(|bad| *bad != version);
            let why = format!("{version} is forced");
            parts.push(entry(by, "unmark_bad", &name, why));
        }
        if let Some(left) = &left {
            once(&mut marks.bad, left);
            once(&mut marks.bad_by_force, left);
            let why = format!("{left} was left by the force to {version}");
            parts.push(entry(by, "mark_bad", &name, why));
        }
        for ack in acks {
            marks.failure_acked.retain(|old| old.node_id != ack.node_id);
            marks.failure_acked.push(ack);
        }
        let why = format!("the force to {version}: {why}");
        parts.push(ops::clear_halt(marks, by, &name, why));
        Ok((head, parts))
    })?;
    let desired = match before {
        Some(mut desired) => {
            desired.version = version;
            desired
        }
        None => first_desired(def, version),
    };
    store.set(&name, desired)?;
    tracing::warn!(service = %name, from = ?left, reason = %why, "forced");
    Ok(head)
}

/// Adds `version` to `list` unless it is there.
fn once(list: &mut Vec<Version>, version: &Version) {
    if !list.contains(version) {
        list.push(version.clone());
    }
}

/// Why CI's desired state was not let through by [`before_ci_write`].
#[derive(Debug)]
pub(crate) enum CiBlocked {
    /// A force left the version and holds its bad mark.
    Held,
    /// The ops file could not be written.
    Ops(OpsError),
}

impl From<OpsError> for CiBlocked {
    fn from(why: OpsError) -> Self {
        Self::Ops(why)
    }
}

/// What [`before_ci_write`] decided under the writers' lock.
enum Decided {
    Blocked(CiBlocked),
    Nothing,
}

impl From<OpsError> for Decided {
    fn from(why: OpsError) -> Self {
        Self::Blocked(CiBlocked::Ops(why))
    }
}

/// Readies the ops store for CI's desired state of `version` of `service`, in one write decided
/// on the state it is applied to (under the writers' lock, so a force that holds the version
/// after the caller read the state is seen): refused when a force holds the version's mark;
/// otherwise the version's bad mark goes, with the failure reports of it the hosts make now
/// acknowledged (CI set it by hand), and a force record that no longer stands under `current`,
/// the desired state now, goes (`force_ended`: a force whose desired state was never written,
/// or one CI's earlier write did not get to end), so that writing its version does not make it
/// stand again. A force that stands is left standing: [`end_after_ci_write`] ends it once CI's
/// state is written, so a write that fails, or a stop between the two, leaves the force as it
/// was. The caller holds the fleet's lock across this, the desired state's write and the end,
/// as a force does, so neither comes between the other's parts.
///
/// # Errors
/// [`CiBlocked::Held`], and nothing changes; [`CiBlocked::Ops`]: the ops file could not be
/// written, and nothing changes.
pub(crate) fn before_ci_write(
    ops: &OpsStore,
    fleet: &Fleet,
    service: &str,
    version: &Version,
    current: Option<&Desired>,
) -> Result<(), CiBlocked> {
    let acks = crate::acks::release_failures(fleet, service, version);
    let changed = ops.decide_changes::<Decided>(Write::BeforeMemory, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        if marks.bad_by_force.contains(version) {
            return Err(Decided::Blocked(CiBlocked::Held));
        }
        let mut entries = Vec::new();
        if marks.bad.contains(version) {
            marks.bad.retain(|bad| bad != version);
            let why = format!(
                "{version} was set by CI; {} failure reports acked",
                acks.len()
            );
            for ack in acks {
                marks.failure_acked.retain(|old| old.node_id != ack.node_id);
                marks.failure_acked.push(ack);
            }
            entries.push(entry(CI, "unmark_bad", service, why));
        }
        if standing(marks, current).is_none()
            && let Some(force) = marks.force.take()
        {
            let why = format!(
                "the force to {} did not stand and ends: {version} is to be set",
                force.version
            );
            entries.push(entry(CI, "force_ended", service, why));
        }
        if entries.is_empty() {
            return Err(Decided::Nothing);
        }
        let head = entries.remove(0);
        Ok((head, entries))
    });
    match changed {
        Ok(_) | Err(Decided::Nothing) => Ok(()),
        Err(Decided::Blocked(why)) => Err(why),
    }
}

/// Ends the force of `service` once CI's desired state of `version` is written. An end that
/// cannot be written is logged: the record stays, and stands only while the desired version is
/// its version (CI set the forced version itself); the next CI write ends it
/// ([`before_ci_write`]).
pub(crate) fn end_after_ci_write(ops: &OpsStore, service: &str, version: &Version) {
    let ended = ops.decide_change::<Decided>(Write::BeforeMemory, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        let force = marks.force.take().ok_or(Decided::Nothing)?;
        let why = format!("the force to {} ends: {version} was set", force.version);
        Ok(entry(CI, "force_ended", service, why))
    });
    if let Err(Decided::Blocked(CiBlocked::Ops(why))) = ended {
        tracing::error!(service, %why, "the end of a force could not be written: it ends with the next desired state");
    }
}

/// The operator's `unmark_bad` of `version` of `service`, as `by`: the mark goes, the hold a
/// force put on it too, and the failure reports of it the hosts make now are acknowledged
/// ([`crate::acks::clear_bad`]), so CI may set it again.
///
/// # Errors
/// [`OperateError::Invalid`]: an unknown service, a bad reason, or a version that is not marked
/// bad. [`OperateError::Store`]: the ops file could not be written.
pub(crate) fn unmark_bad(
    ops: &OpsStore,
    services: &Services,
    fleet: &Fleet,
    by: &str,
    service: &str,
    version: &Version,
    reason: &str,
) -> Result<AuditEntry, OperateError> {
    let name = ops::service(services, service)?;
    let why = ops::reason(reason)?;
    crate::acks::clear_bad(ops, fleet, by, name, version, &why)?
        .ok_or(OperateError::Invalid("the version is not marked bad"))
}
