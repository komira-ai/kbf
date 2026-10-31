//! `paper-scan`: the open places of the paper against the map that says which cell fills each.
//!
//! A place is open when its line carries `class="tbd"` or the words "not measured" (any case).
//! The map is the JSON array of cells; each entry has a `paper_line`. The two must name the
//! same lines: a marked line no entry names is a place no cell was planned for, and an entry
//! on a line with no marker is stale (the paper was edited, or the cell was already filled).

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::error::BenchError;

/// The result of the comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaperScan {
    /// Lines with a marker that no map entry names.
    pub unmapped: Vec<u32>,
    /// Lines a map entry names that carry no marker.
    pub stale: Vec<u32>,
    /// How many lines carry a marker.
    pub marked: usize,
}

impl PaperScan {
    /// Whether the paper and the map agree.
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.unmapped.is_empty() && self.stale.is_empty()
    }
}

/// One entry of the map, of which only the line is read.
#[derive(Deserialize)]
struct Entry {
    paper_line: u32,
}

/// Whether a line of the paper marks an open place.
fn is_open(line: &str) -> bool {
    line.contains("class=\"tbd\"") || line.to_lowercase().contains("not measured")
}

/// Compares `page` with the map in `map_json`.
///
/// # Errors
/// [`BenchError::Ops`] if `map_json` is not an array of entries with a `paper_line`.
pub fn scan(page: &str, map_json: &str) -> Result<PaperScan, BenchError> {
    let entries: Vec<Entry> = serde_json::from_str(map_json).map_err(|_| {
        BenchError::Ops("the paper map is not a JSON array of entries with a paper_line".to_owned())
    })?;
    let mapped: BTreeSet<u32> = entries.iter().map(|e| e.paper_line).collect();
    let marked: BTreeSet<u32> = (1..)
        .zip(page.lines())
        .filter(|(_, line)| is_open(line))
        .map(|(number, _)| number)
        .collect();
    Ok(PaperScan {
        unmapped: marked.difference(&mapped).copied().collect(),
        stale: mapped.difference(&marked).copied().collect(),
        marked: marked.len(),
    })
}
