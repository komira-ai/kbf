//! Merges the lcov tracefiles of a sharded test run into the tracefile one run of all the
//! tests would have written.
//!
//! CI splits the fast tests over several jobs (nextest `--partition`). Every job builds the
//! same instrumented binaries from the same sources and runs a part of the tests, so each
//! job's tracefile lists the same lines and the same branches with the counts of its part.
//! The merge adds the counts: a line (`DA:<line>,<count>`) is hit when any shard hit it,
//! and a branch (`BRDA:<line>,<block>,<branch>,<taken>`) is taken when any shard took it,
//! `-` (the branch's code never ran) standing for zero where another shard has a number
//! and staying `-` when every shard has it. The summary lines (`LF`, `LH`, `BRF`, `BRH`)
//! are written again from the merged detail.
//!
//! Refused, so that a merge cannot report a total no single run would: a tracefile whose
//! summary lines disagree with its detail lines (the totals the gate reads are then not
//! the ones merged), a detail or summary line outside a record or a malformed one, two
//! records for one source, a tracefile with no records, and shards that do not list the
//! same sources, lines and branches (different binaries, so adding their counts is
//! meaningless). Lines the merge does not read (`TN`, `FN`, `FNDA`, ...) are dropped.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Why shards were not merged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    #[error("no tracefiles to merge")]
    NoInputs,
    #[error("{input}: line {line}: {what}")]
    Malformed {
        input: String,
        line: usize,
        what: String,
    },
    #[error("{input}: {path}: {field} says {stated} but its detail lines give {listed}")]
    Summary {
        input: String,
        path: String,
        field: &'static str,
        stated: u64,
        listed: u64,
    },
    #[error("{input} holds no records")]
    Empty { input: String },
    #[error("{input} differs from {first} in {what}")]
    Differs {
        input: String,
        first: String,
        what: String,
    },
}

/// One `BRDA` line. The block and branch stay text: only their equality across shards
/// matters.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Branch {
    line: u64,
    block: String,
    branch: String,
    /// `None` is `-`.
    taken: Option<u64>,
}

/// The detail lines of one source file, in the order the tracefile lists them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Detail {
    /// `(line, count)` per `DA`.
    lines: Vec<(u64, u64)>,
    branches: Vec<Branch>,
}

type Trace = BTreeMap<String, Detail>;

/// The summary lines, in the order a record keeps what it stated.
const SUMMARY: [&str; 4] = ["LF", "LH", "BRF", "BRH"];

enum Kind {
    Da,
    Brda,
    Summary(usize),
}

impl Kind {
    fn of(field: &str) -> Option<Self> {
        match field {
            "DA" => Some(Self::Da),
            "BRDA" => Some(Self::Brda),
            _ => SUMMARY.iter().position(|f| *f == field).map(Self::Summary),
        }
    }
}

/// `<line>,<count>[,<checksum>]`.
fn parse_da(value: &str) -> Option<(u64, u64)> {
    let mut parts = value.split(',');
    let line = parts.next()?.parse().ok()?;
    let count = parts.next()?.parse().ok()?;
    Some((line, count))
}

/// `<line>,<block>,<branch>,<taken or ->`.
fn parse_brda(value: &str) -> Option<Branch> {
    let parts: Vec<&str> = value.split(',').collect();
    let &[line, block, branch, taken] = parts.as_slice() else {
        return None;
    };
    let taken = if taken == "-" {
        None
    } else {
        Some(taken.parse().ok()?)
    };
    Some(Branch {
        line: line.parse().ok()?,
        block: block.to_owned(),
        branch: branch.to_owned(),
        taken,
    })
}

impl Detail {
    /// What `LF`, `LH`, `BRF` and `BRH` must say about this detail.
    fn totals(&self) -> [u64; 4] {
        let count = |n: usize| n as u64;
        [
            count(self.lines.len()),
            count(self.lines.iter().filter(|(_, c)| *c > 0).count()),
            count(self.branches.len()),
            count(self.branches.iter().filter(|b| b.taken > Some(0)).count()),
        ]
    }
}

/// A record being read: its source, detail and the summary lines it stated.
struct Open {
    path: String,
    detail: Detail,
    stated: [Option<u64>; 4],
}

fn parse(input: &str, text: &str) -> Result<Trace, MergeError> {
    let bad = |line: usize, what: String| MergeError::Malformed {
        input: input.to_owned(),
        line,
        what,
    };
    let mut trace = Trace::new();
    let mut open: Option<Open> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim_end();
        if raw == "end_of_record" {
            let done = open
                .take()
                .ok_or_else(|| bad(line, "end_of_record outside a record".to_owned()))?;
            let listed = done.detail.totals();
            for ((field, stated), listed) in SUMMARY.into_iter().zip(done.stated).zip(listed) {
                if let Some(stated) = stated.filter(|s| *s != listed) {
                    return Err(MergeError::Summary {
                        input: input.to_owned(),
                        path: done.path,
                        field,
                        stated,
                        listed,
                    });
                }
            }
            trace.insert(done.path, done.detail);
            continue;
        }
        let Some((field, value)) = raw.split_once(':') else {
            continue;
        };
        if field == "SF" {
            if open.is_some() {
                return Err(bad(line, "SF while a record is open".to_owned()));
            }
            if trace.contains_key(value) {
                return Err(bad(line, format!("{value} appears in two records")));
            }
            open = Some(Open {
                path: value.to_owned(),
                detail: Detail::default(),
                stated: [None; 4],
            });
            continue;
        }
        let Some(kind) = Kind::of(field) else {
            continue;
        };
        let record = open
            .as_mut()
            .ok_or_else(|| bad(line, format!("{field} outside a record")))?;
        match kind {
            Kind::Da => record.detail.lines.push(
                parse_da(value)
                    .ok_or_else(|| bad(line, format!("DA:{value} is not a line and a count")))?,
            ),
            Kind::Brda => record.detail.branches.push(
                parse_brda(value)
                    .ok_or_else(|| bad(line, format!("BRDA:{value} is not a branch")))?,
            ),
            Kind::Summary(slot) => {
                let count = value
                    .parse::<u64>()
                    .map_err(|_| bad(line, format!("{field}:{value} is not a count")))?;
                if record.stated[slot].replace(count).is_some() {
                    return Err(bad(line, format!("{field} given twice in one record")));
                }
            }
        }
    }
    if let Some(o) = open {
        return Err(bad(
            text.lines().count(),
            format!("the record for {} has no end_of_record", o.path),
        ));
    }
    if trace.is_empty() {
        return Err(MergeError::Empty {
            input: input.to_owned(),
        });
    }
    Ok(trace)
}

/// Adds `other`'s counts to `into`; both list the same lines and branches.
fn add(into: &mut Detail, other: &Detail) {
    for (a, b) in into.lines.iter_mut().zip(&other.lines) {
        a.1 = a.1.saturating_add(b.1);
    }
    for (a, b) in into.branches.iter_mut().zip(&other.branches) {
        a.taken = match (a.taken, b.taken) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
        };
    }
}

/// Whether the two list the same lines and the same branches, in the same order.
fn same_shape(a: &Detail, b: &Detail) -> Option<&'static str> {
    if !a.lines.iter().map(|l| l.0).eq(b.lines.iter().map(|l| l.0)) {
        return Some("the lines it lists");
    }
    let key = |x: &Branch| (x.line, x.block.clone(), x.branch.clone());
    if !a.branches.iter().map(key).eq(b.branches.iter().map(key)) {
        return Some("the branches it lists");
    }
    None
}

fn render(trace: &Trace) -> String {
    let mut out = String::from("TN:\n");
    for (path, detail) in trace {
        let _ = writeln!(out, "SF:{path}");
        for (line, count) in &detail.lines {
            let _ = writeln!(out, "DA:{line},{count}");
        }
        for b in &detail.branches {
            let taken = b.taken.map_or_else(|| "-".to_owned(), |t| t.to_string());
            let _ = writeln!(out, "BRDA:{},{},{},{taken}", b.line, b.block, b.branch);
        }
        let [lf, lh, brf, brh] = detail.totals();
        let _ = write!(
            out,
            "LF:{lf}\nLH:{lh}\nBRF:{brf}\nBRH:{brh}\nend_of_record\n"
        );
    }
    out
}

/// Merges tracefiles given as `(name, text)`; the name is how errors call the input.
pub fn merge(inputs: &[(String, String)]) -> Result<String, MergeError> {
    let Some(((first, text), rest)) = inputs.split_first() else {
        return Err(MergeError::NoInputs);
    };
    let mut merged = parse(first, text)?;
    for (name, text) in rest {
        let trace = parse(name, text)?;
        let differs = |what: String| MergeError::Differs {
            input: name.clone(),
            first: first.clone(),
            what,
        };
        if trace.len() != merged.len() {
            return Err(differs(format!(
                "how many sources it lists ({} against {})",
                trace.len(),
                merged.len()
            )));
        }
        for (path, detail) in &trace {
            let into = merged.get_mut(path).ok_or_else(|| {
                differs(format!("the sources it lists ({path} is not in the first)"))
            })?;
            if let Some(what) = same_shape(into, detail) {
                return Err(differs(format!("{what} for {path}")));
            }
            add(into, detail);
        }
    }
    Ok(render(&merged))
}

#[cfg(test)]
mod tests;
