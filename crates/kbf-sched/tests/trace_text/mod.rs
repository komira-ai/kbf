//! The scheduler's inputs as trace lines spell them.
//!
//! Every field is written by hand, in the shapes of `kbf_sim::text`, never through
//! `Debug`: the golden trace hashes (`tests/golden/trace-hashes.txt`) rest on this text,
//! and moving or renaming a type must not change it. Shared by the simulation worlds
//! that trace the scheduler's inputs and effects (F1, F3, F4).

use std::collections::{BTreeMap, BTreeSet};

use kbf_caps::NodeCaps;
use kbf_sched::{Event, Request};
use kbf_sim::text;
use kbf_types::{NodePhase, Qos};

/// The text of a map: `{k=v; k=v}`, keys in order.
fn map<V>(entries: &BTreeMap<String, V>, value: impl Fn(&V) -> String) -> String {
    let parts: Vec<String> = entries
        .iter()
        .map(|(k, v)| format!("{}={}", text::quoted(k), value(v)))
        .collect();
    format!("{{{}}}", parts.join("; "))
}

fn strings(set: &BTreeSet<String>) -> String {
    let all: Vec<&String> = set.iter().collect();
    text::list(&all, |s| text::quoted(s))
}

/// A node's capabilities, every part named.
pub fn caps(caps: &NodeCaps) -> String {
    let features: BTreeSet<String> = caps.cpu.features().clone();
    let level = caps.cpu.level().map(|l| l.name());
    let consumables: Vec<String> = caps
        .consumables
        .iter()
        .map(|(c, n)| format!("{}={n}", c.name()))
        .collect();
    format!(
        "caps(arch={} level={} features={} exact={} members={} consumables={{{}}} \
         drivers={} devices={})",
        caps.cpu.arch().name(),
        level.unwrap_or("-"),
        strings(&features),
        map(&caps.exact, |v| text::quoted(v)),
        map(&caps.members, strings),
        consumables.join("; "),
        strings(&caps.drivers),
        map(&caps.devices, |d| map(d, |v| text::quoted(v))),
    )
}

fn qos(qos: &Qos) -> String {
    format!("{}/{}", qos.name(), qos.urgency())
}

/// A request for work, every field named.
pub fn request(request: &Request) -> String {
    let needs: Vec<String> = request
        .needs
        .properties()
        .into_iter()
        .map(|(k, v)| format!("{}={}", text::quoted(k), text::quoted(&v)))
        .collect();
    format!(
        "request(instance={} action={} qos={} kind={} {} needs=[{}] hermetic={} \
         do_not_cache={})",
        text::quoted(&request.key.instance),
        text::digest(&request.key.action),
        qos(&request.qos),
        text::kind(request.kind),
        text::resources(&request.resources),
        needs.join("; "),
        request.hermetic,
        request.do_not_cache,
    )
}

fn phase(phase: &NodePhase) -> String {
    match phase {
        NodePhase::Serving => "serving".to_owned(),
        NodePhase::Preparing(lease) => format!("preparing({})", text::lease(*lease)),
        NodePhase::Restoring(lease) => format!("restoring({})", text::lease(*lease)),
        NodePhase::Quarantined(why) => format!("quarantined({})", text::quoted(why)),
        NodePhase::MemoryPressure => "memory_pressure".to_owned(),
        NodePhase::Draining => "draining".to_owned(),
    }
}

/// One input event.
pub fn event(event: &Event) -> String {
    match event {
        Event::WorkerUp {
            worker,
            instance,
            predecessor_stopped,
            capacity,
            caps: c,
        } => format!(
            "worker_up(worker={} instance={} predecessor_stopped={predecessor_stopped} \
             capacity={} {})",
            text::worker(worker),
            text::quoted(instance.as_str()),
            text::resources(capacity),
            caps(c),
        ),
        Event::Capacity {
            worker,
            capacity,
            caps: c,
        } => format!(
            "capacity(worker={} capacity={} {})",
            text::worker(worker),
            text::resources(capacity),
            caps(c),
        ),
        Event::Heartbeat { worker, running } => format!(
            "heartbeat(worker={} running={})",
            text::worker(worker),
            text::list(running, |l| text::lease(*l))
        ),
        Event::Phase { worker, phase: p } => {
            format!("phase(worker={} {})", text::worker(worker), phase(p))
        }
        Event::Submit { waiter, request: r } => {
            format!("submit(waiter={} {})", text::waiter(*waiter), request(r))
        }
        Event::Committed(record) => format!("committed({})", text::record(record)),
        Event::Started { operation, lease } => format!(
            "started(op={} lease={})",
            text::operation(*operation),
            text::lease(*lease)
        ),
        Event::Report {
            operation,
            lease,
            outcome,
        } => format!(
            "report(op={} lease={} outcome={})",
            text::operation(*operation),
            text::lease(*lease),
            text::outcome(*outcome)
        ),
        Event::Tick => "tick".to_owned(),
        Event::Cordon { worker } => format!("cordon(worker={})", text::worker(worker)),
        Event::Drain { worker, deadline } => format!(
            "drain(worker={} deadline={})",
            text::worker(worker),
            text::time(*deadline)
        ),
        Event::Uncordon { worker } => format!("uncordon(worker={})", text::worker(worker)),
        Event::Busy {
            worker,
            busy_millis,
        } => format!("busy(worker={} millis={busy_millis})", text::worker(worker)),
    }
}
