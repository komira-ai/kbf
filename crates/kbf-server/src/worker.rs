//! What a node report says, and the `kbf.worker.v1` versions this build speaks.
//!
//! The farm registers a node with what [`reported`] reads from its `Hello`: the capacity
//! placement books ([`capacity`]), the entries it matches platforms against ([`caps`]: `arch`
//! and the rest that `NodeCaps::from_report` reads) and the node's phase ([`phase`]). The
//! executor's virtual workers register through it ([`crate::executor`]); no node opens a
//! session to a server any more.
//!
//! [`PROTOCOL_VERSION`] and [`OLDEST_PROTOCOL_VERSION`] are the versions this build speaks and
//! the oldest it accepts: `kbf --version --json` reports them, the registration of a storage
//! node names them, and `GET /v1/nodes` lists the version each node speaks.

use kbf_caps::NodeCaps;
use kbf_proto::worker::{Capability, Hello, Phase};
use kbf_sched::NodePhase;
use kbf_types::{LeaseId, Resources};

use crate::farm::Reported;

/// The `kbf.worker.v1` version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version it accepts. The protocol promises N-1 and N; version 1 is the
/// first, so there is no N-1 yet.
pub const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// The protocol version a wire field names: `None` for 0. Versions start at 1, and
/// proto3 cannot tell a 0 from a field the sender never set (a `Hello` or a storage
/// registration from a build that predates the field, or a stored row written before
/// the field existed), so a 0 is a node whose version is not known and not a node that
/// speaks version 0.
#[must_use]
pub const fn named_protocol(wire: u32) -> Option<u32> {
    if wire == 0 { None } else { Some(wire) }
}

/// What a `Hello` reports: [`capacity`], [`caps`] and [`phase`], and the version it
/// speaks ([`named_protocol`] of `protocol_version`).
///
/// # Errors
/// Any of the three is malformed.
pub fn reported(hello: &Hello, floor: HostFloor) -> Result<Reported, String> {
    Ok(Reported {
        capacity: capacity(hello, floor)?,
        caps: caps(hello)?,
        phase: phase(hello)?,
        protocol: named_protocol(hello.protocol_version),
    })
}

/// The node's phase: serving when the `Hello` names none. A phase this server does not
/// know is read as a quarantine that says so: the node takes no lease and is listed for
/// attention, rather than being offered work it may not be ready for.
///
/// # Errors
/// A preparing or restoring phase that names no lease.
pub fn phase(hello: &Hello) -> Result<NodePhase, String> {
    let Some(reported) = &hello.phase else {
        return Ok(NodePhase::Serving);
    };
    let lease = || {
        reported
            .lease_id
            .map(|l| LeaseId::new(l.term, l.seq))
            .ok_or_else(|| "a preparing or restoring phase names no lease".to_owned())
    };
    Ok(match Phase::try_from(reported.phase) {
        Ok(Phase::Serving) => NodePhase::Serving,
        Ok(Phase::Preparing) => NodePhase::Preparing(lease()?),
        Ok(Phase::Restoring) => NodePhase::Restoring(lease()?),
        Ok(Phase::Quarantined) => NodePhase::Quarantined(reported.reason.clone()),
        Ok(Phase::MemoryPressure) => NodePhase::MemoryPressure,
        Ok(Phase::Draining) => NodePhase::Draining,
        // `Tainted` is on the wire before this server acts on it: the fallback applies.
        Ok(Phase::Tainted) | Err(_) => NodePhase::Quarantined(format!(
            "the node reports phase {}, which this server does not know",
            reported.phase
        )),
    })
}

/// The most macOS guests one Mac runs at once (Apple's limit); a node's `vm.slots` is
/// clamped to it.
pub const MAX_VM_SLOTS: u64 = 2;

/// What a node that reports VM slots keeps for the host itself: subtracted from its
/// capacity so that guests cannot take the host's own share of cores and memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFloor {
    /// CPU, in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory, in bytes.
    pub memory_bytes: u64,
}

impl HostFloor {
    /// The floor of `cpus` cores and `mem_gib` GiB.
    #[must_use]
    pub const fn new(cpus: u64, mem_gib: u64) -> Self {
        Self {
            cpu_millis: cpus.saturating_mul(1_000),
            memory_bytes: mem_gib.saturating_mul(1 << 30),
        }
    }

    /// 2 cores and 8 GiB.
    pub const DEFAULT: Self = Self::new(2, 8);
}

impl Default for HostFloor {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What placement may book on a node: its `cpus` and `mem_gib` entries, its `gpu` entry
/// (0 when absent: a worker that does not detect GPUs has none to book) and its
/// `vm.slots` entry (0 when absent), clamped to [`MAX_VM_SLOTS`]. Its
/// `action_slots` entry, when reported (it counts the usable slots, busy ones
/// included), caps how many Action and VM leases it holds at once
/// (`Resources::leases`); a node that reports none has no cap. A node that reports
/// at least one VM slot gives up `floor` of its CPUs and memory to the host; any other
/// node books the whole machine. The other protected floors (planned,
/// `docs/design/capabilities.md#planned`) are subtracted once they are reported.
///
/// # Errors
/// `cpus` or `mem_gib` is missing; an entry is repeated or not a whole number.
pub fn capacity(hello: &Hello, floor: HostFloor) -> Result<Resources, String> {
    let required = |key| entry(&hello.capabilities, key)?.ok_or_else(|| missing(key));
    let cpus = required("cpus")?;
    let mem_gib = required("mem_gib")?;
    let gpus = entry(&hello.capabilities, "gpu")?.unwrap_or(0);
    let vms = entry(&hello.capabilities, "vm.slots")?
        .unwrap_or(0)
        .min(MAX_VM_SLOTS);
    let mut cpu_millis = cpus.saturating_mul(1_000);
    let mut memory_bytes = mem_gib.saturating_mul(1 << 30);
    if vms > 0 {
        cpu_millis = cpu_millis.saturating_sub(floor.cpu_millis);
        memory_bytes = memory_bytes.saturating_sub(floor.memory_bytes);
    }
    let booked = Resources::new(cpu_millis, memory_bytes)
        .with_gpus(gpus)
        .with_vms(vms);
    // The entry counts usable slots, those running a lease included: it is a cap on the
    // leases held, not a count of the free ones.
    Ok(match entry(&hello.capabilities, "action_slots")? {
        Some(slots) => booked.with_lease_cap(slots),
        None => booked,
    })
}

/// What placement matches an action's platform against: the node report read by
/// `kbf_caps::NodeCaps::from_report` (`arch`, `cpu.features`, `os`, labels, ...).
///
/// # Errors
/// `arch` is missing, repeated or unknown; another single-valued entry is repeated; a
/// countable entry is not a whole number.
pub fn caps(hello: &Hello) -> Result<NodeCaps, String> {
    let entries = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()));
    NodeCaps::from_report(entries).map_err(|e| e.to_string())
}

fn missing(key: &str) -> String {
    format!("the node report needs exactly one {key:?} entry")
}

/// The value of the `key` entry, `None` if there is none.
fn entry(capabilities: &[Capability], key: &str) -> Result<Option<u64>, String> {
    let mut values = capabilities.iter().filter(|c| c.key == key);
    let (only, None) = (values.next(), values.next()) else {
        return Err(missing(key));
    };
    only.map(|only| {
        only.value.parse().map_err(|_| {
            format!(
                "node report entry {key}={:?} is not a whole number",
                only.value
            )
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use kbf_proto::worker::{Capability, Hello, NodePhase as Wire, Phase};
    use kbf_sched::NodePhase;
    use kbf_types::{LeaseId, Resources};

    use super::{HostFloor, MAX_VM_SLOTS, capacity, caps, named_protocol, phase, reported};

    /// A `Hello` reporting 4 CPUs, 8 GiB and a Linux x86-64 platform, with `more` entries.
    fn hello(more: &[(&str, &str)]) -> Hello {
        let entry = |key: &str, value: &str| Capability {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let mut capabilities = vec![
            entry("cpus", "4"),
            entry("mem_gib", "8"),
            entry("arch", "x86_64"),
            entry("os", "linux"),
        ];
        capabilities.extend(more.iter().map(|(key, value)| entry(key, value)));
        Hello {
            protocol_version: 1,
            node_id: "node-a".to_owned(),
            capabilities,
            ..Hello::default()
        }
    }

    /// Catches: a wire 0 (a field its sender never set) taken for a version, so that a
    /// node naming none would read as speaking version 0 and pull the oldest version of
    /// a list down to it; and a version that is changed or dropped on the way.
    #[test]
    fn a_wire_zero_names_no_protocol_and_any_other_number_is_itself() {
        assert_eq!(named_protocol(0), None);
        assert_eq!(named_protocol(1), Some(1));
        assert_eq!(named_protocol(u32::MAX), Some(u32::MAX));
    }

    /// Catches: a node report that books the whole machine for a node that holds VM slots
    /// (the host's floor not subtracted), a floor subtracted below zero, VM slots above
    /// Apple's limit booked, a GPU entry or an `action_slots` cap that is dropped, and a
    /// node without them that is capped.
    #[test]
    fn a_report_books_the_machine_less_the_floor_of_a_node_with_vm_slots() {
        let plain = capacity(&hello(&[]), HostFloor::DEFAULT).expect("capacity");
        assert_eq!(plain, Resources::new(4_000, 8 << 30));

        let slots = hello(&[("vm.slots", "9"), ("gpu", "1"), ("action_slots", "3")]);
        let booked = capacity(&slots, HostFloor::new(1, 2)).expect("capacity");
        let want = Resources::new(3_000, 6 << 30)
            .with_gpus(1)
            .with_vms(MAX_VM_SLOTS)
            .with_lease_cap(3);
        assert_eq!(booked, want);

        let small = hello(&[("vm.slots", "1")]);
        let none = capacity(&small, HostFloor::new(64, 64)).expect("capacity");
        assert_eq!(none, Resources::new(0, 0).with_vms(1));
    }

    /// Catches: a node report without `cpus` or `mem_gib`, with one repeated, or with one that
    /// is not a whole number, taken for a valid one; and an error that does not name the entry.
    #[test]
    fn a_report_that_lacks_garbles_or_repeats_an_entry_is_refused_naming_it() {
        let mut no_cpus = hello(&[]);
        no_cpus.capabilities.retain(|c| c.key != "cpus");
        let said = capacity(&no_cpus, HostFloor::DEFAULT).expect_err("no cpus");
        assert_eq!(said, "the node report needs exactly one \"cpus\" entry");
        let twice = hello(&[("mem_gib", "9")]);
        let said = capacity(&twice, HostFloor::DEFAULT).expect_err("two");
        assert_eq!(said, "the node report needs exactly one \"mem_gib\" entry");
        let mut garbled = hello(&[]);
        garbled.capabilities[1].value = "lots".to_owned();
        let said = capacity(&garbled, HostFloor::DEFAULT).expect_err("garbled");
        assert_eq!(
            said,
            "node report entry mem_gib=\"lots\" is not a whole number"
        );
        let said = capacity(&hello(&[("gpu", "x")]), HostFloor::DEFAULT).expect_err("gpu");
        assert_eq!(said, "node report entry gpu=\"x\" is not a whole number");
        assert!(reported(&no_cpus, HostFloor::DEFAULT).is_err());
    }

    /// Catches: a node whose `arch` is missing or unknown taken for one placement can match
    /// platforms against.
    #[test]
    fn a_report_without_a_known_arch_has_no_platform() {
        assert!(caps(&hello(&[])).is_ok());
        let mut nameless = hello(&[]);
        nameless.capabilities.retain(|c| c.key != "arch");
        assert!(caps(&nameless).is_err());
        let sparc = hello(&[]);
        let mut sparc = Hello {
            capabilities: sparc.capabilities,
            ..Hello::default()
        };
        sparc.capabilities.iter_mut().for_each(|c| {
            if c.key == "arch" {
                c.value = "sparc".to_owned();
            }
        });
        assert!(caps(&sparc).is_err());
    }

    fn phased(phase: i32, lease: Option<kbf_proto::worker::LeaseId>, reason: &str) -> Hello {
        Hello {
            phase: Some(Wire {
                phase,
                lease_id: lease,
                reason: reason.to_owned(),
            }),
            ..hello(&[])
        }
    }

    /// Catches: a `Hello` that names no phase taken for anything but serving; a preparing or
    /// restoring phase that loses its lease or is accepted without one; a quarantine that
    /// loses its reason; and a phase this build does not know (and `Tainted`, on the wire
    /// before it is acted on) read as serving, which would offer a node work it may not be
    /// ready for.
    #[test]
    fn a_phase_is_read_as_reported_and_an_unknown_one_quarantines_the_node() {
        assert_eq!(phase(&hello(&[])), Ok(NodePhase::Serving));
        let lease = kbf_proto::worker::LeaseId { term: 3, seq: 4 };
        let held = LeaseId::new(3, 4);
        let read = |phase: Phase, lease| phase_of(phase as i32, lease, "why");
        assert_eq!(read(Phase::Serving, None), Ok(NodePhase::Serving));
        assert_eq!(
            read(Phase::Preparing, Some(lease)),
            Ok(NodePhase::Preparing(held))
        );
        assert_eq!(
            read(Phase::Restoring, Some(lease)),
            Ok(NodePhase::Restoring(held))
        );
        assert_eq!(
            read(Phase::Quarantined, None),
            Ok(NodePhase::Quarantined("why".to_owned()))
        );
        assert_eq!(
            read(Phase::MemoryPressure, None),
            Ok(NodePhase::MemoryPressure)
        );
        assert_eq!(read(Phase::Draining, None), Ok(NodePhase::Draining));
        let said = "a preparing or restoring phase names no lease";
        assert_eq!(read(Phase::Preparing, None), Err(said.to_owned()));
        assert_eq!(read(Phase::Restoring, None), Err(said.to_owned()));
        for unknown in [Phase::Tainted as i32, 99] {
            let NodePhase::Quarantined(reason) = phase_of(unknown, None, "").expect("a phase")
            else {
                panic!("an unknown phase must quarantine");
            };
            assert!(reason.contains("does not know"), "{reason}");
        }
    }

    fn phase_of(
        phase: i32,
        lease: Option<kbf_proto::worker::LeaseId>,
        reason: &str,
    ) -> Result<NodePhase, String> {
        super::phase(&phased(phase, lease, reason))
    }
}
