//! Fetch, execution and pack per where the phase times came from.

use serde_json::{Value, json};

use crate::builds::{Spread, spread};
use crate::compare::{count_row, delta, duration_row};
use crate::row::Row;

/// The phase durations of the rows of one provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseSet {
    /// Rows of this provenance.
    pub rows: u64,
    /// Input fetch percentiles.
    pub fetch: Spread,
    /// Execution percentiles.
    pub execution: Spread,
    /// Pack percentiles.
    pub pack: Spread,
}

/// One arm's phase numbers, never pooled across provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseStats {
    /// Rows a daemon measured, and rows that name no provenance.
    pub measured: PhaseSet,
    /// Rows whose phase times a task claimed.
    pub task_claim: PhaseSet,
    /// Rows naming a provenance this tool does not know.
    pub other: u64,
}

/// The values of one provenance, before percentiles.
#[derive(Default)]
struct Values {
    rows: u64,
    fetch: Vec<u64>,
    execution: Vec<u64>,
    pack: Vec<u64>,
}

impl Values {
    fn set(self) -> PhaseSet {
        PhaseSet {
            rows: self.rows,
            fetch: spread(self.fetch),
            execution: spread(self.execution),
            pack: spread(self.pack),
        }
    }
}

/// The phase numbers of `rows`: a row with no marker is measured, one with a marker this tool
/// does not know is counted and in no number.
pub fn analyze(rows: &[Row]) -> PhaseStats {
    let (mut measured, mut claimed) = (Values::default(), Values::default());
    let mut other = 0;
    for row in rows {
        let into = match row.phase_times.as_deref() {
            None | Some("measured") => &mut measured,
            Some("task_claim") => &mut claimed,
            Some(_) => {
                other += 1;
                continue;
            }
        };
        into.rows += 1;
        into.fetch.extend(row.fetch_us);
        into.execution.extend(row.execution_us);
        into.pack.extend(row.pack_us);
    }
    PhaseStats {
        measured: measured.set(),
        task_claim: claimed.set(),
        other,
    }
}

type Get = fn(&PhaseSet) -> Spread;

type Kind = fn(&PhaseStats) -> &PhaseSet;

/// The two provenances the numbers are kept apart by.
const KINDS: [(&str, Kind); 2] = [
    ("measured", |s| &s.measured),
    ("task_claim", |s| &s.task_claim),
];

const PHASES: [(&str, Get, &str); 3] = [
    ("fetch", |s| s.fetch, "fetch_us"),
    ("execution", |s| s.execution, "execution_us"),
    ("pack", |s| s.pack, "pack_us"),
];

/// Appends the phase rows of the table: the rows of each provenance, then each phase's p50
/// and p90 once for measured rows and once for claimed rows. There is no pooled row.
pub(crate) fn rows(out: &mut String, a: &PhaseStats, b: &PhaseStats) {
    for (name, get) in KINDS {
        count_row(
            out,
            &format!("attempts, phase times {name}"),
            get(a).rows,
            get(b).rows,
        );
    }
    count_row(out, "attempts, phase times other", a.other, b.other);
    for (phase, spread_of, _) in PHASES {
        for (name, get) in KINDS {
            let (sa, sb) = (spread_of(get(a)), spread_of(get(b)));
            duration_row(out, &format!("{phase} p50 {name} (s)"), sa.p50, sb.p50);
            duration_row(out, &format!("{phase} p90 {name} (s)"), sa.p90, sb.p90);
        }
    }
}

fn spread_json(s: Spread) -> Value {
    json!({"p50": s.p50, "p90": s.p90})
}

fn set_json(set: &PhaseSet) -> Value {
    let mut keys = serde_json::Map::new();
    keys.insert("rows".to_owned(), set.rows.into());
    for (_, get, key) in PHASES {
        keys.insert(key.to_owned(), spread_json(get(set)));
    }
    keys.into()
}

/// One arm's phase numbers, in microseconds, per provenance.
pub(crate) fn arm_json(arm: &PhaseStats) -> Value {
    json!({
        "measured": set_json(&arm.measured),
        "task_claim": set_json(&arm.task_claim),
        "other": arm.other,
    })
}

fn set_delta(a: &PhaseSet, b: &PhaseSet) -> Value {
    let mut keys = serde_json::Map::new();
    for (_, get, key) in PHASES {
        let (sa, sb) = (get(a), get(b));
        keys.insert(
            key.to_owned(),
            json!({"p50": delta(sa.p50, sb.p50), "p90": delta(sa.p90, sb.p90)}),
        );
    }
    keys.into()
}

/// `b - a` of the phase percentiles per provenance, `null` where either arm has none.
pub(crate) fn difference_json(a: &PhaseStats, b: &PhaseStats) -> Value {
    json!({
        "measured": set_delta(&a.measured, &b.measured),
        "task_claim": set_delta(&a.task_claim, &b.task_claim),
    })
}
