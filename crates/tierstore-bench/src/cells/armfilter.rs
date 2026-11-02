//! The `--arms` filter of `run`: which arms of the selected cells a run sends.
//!
//! The put and read cells interleave all their paths in one run, so after the first `instance`
//! pass the cold store guard does not judge the `direct` arms any more (the instance's copies to
//! the cold store go on after its pass). `run --arms direct,direct-mp` runs those two paths in a
//! run of their own, and `run --arms instance` the instance in another.
//!
//! An item of the filter is an arm *family*: the label up to its size. The arm
//! `direct-mp-4kib-c1-healthy` is of family `direct-mp`, `instance-1mib-open10-healthy` of
//! `instance`, `hot-4kib-c1-healthy` of `hot`. Cells B, C and G have arms with no family (their
//! labels start with a size), so they cannot be filtered.
//!
//! Rules:
//! - Without a filter nothing changes.
//! - An item that no selected cell has as a family is refused before anything is sent
//!   ([`check`]).
//! - A filter that leaves a selected cell with no arm is refused, naming the cell ([`select`]).
//! - Arms keep the order the cell built them in.

use crate::cells::engine::Arm;
use crate::cells::{CellId, a1, cold_put, cold_read};
use crate::error::BenchError;

/// The family of an arm label: its leading segments up to the first that starts with a digit
/// (the size, or the count of a cell that starts with one); empty if the label starts with one.
#[must_use]
pub fn family(label: &str) -> String {
    let leading = |segment: &&str| !segment.starts_with(|c: char| c.is_ascii_digit());
    label
        .split('-')
        .take_while(leading)
        .collect::<Vec<_>>()
        .join("-")
}

/// The families of the arms `cell` builds.
#[must_use]
pub fn families(cell: CellId) -> Vec<&'static str> {
    match cell {
        CellId::ColdPut => cold_put::PATHS.iter().map(|p| p.name).collect(),
        CellId::ColdRead => cold_read::PATHS.iter().map(|p| p.name).collect(),
        CellId::A1 => a1::PATHS.iter().map(|p| p.name).collect(),
        CellId::B | CellId::C | CellId::G => Vec::new(),
    }
}

/// Refuses a filter with an item that none of `cells` has as a family.
///
/// # Errors
/// [`BenchError::Setup`] naming the item and the families the cells have.
pub fn check(cells: &[CellId], filter: &[String]) -> Result<(), BenchError> {
    let known: Vec<&str> = cells.iter().flat_map(|cell| families(*cell)).collect();
    for item in filter {
        if item.is_empty() {
            return Err(BenchError::Setup(
                "--arms: an empty family in the list".to_owned(),
            ));
        }
        if !known.contains(&item.as_str()) {
            return Err(BenchError::Setup(format!(
                "--arms: no selected cell has the arm family {item:?} (the families of the selected cells: {})",
                known.join(", ")
            )));
        }
    }
    Ok(())
}

/// The arms of `cell` that `filter` names, in their order; all of them without a filter.
///
/// # Errors
/// [`BenchError::Setup`] naming the cell if the filter leaves it no arm.
pub fn select(
    cell: CellId,
    arms: Vec<Arm>,
    filter: Option<&[String]>,
) -> Result<Vec<Arm>, BenchError> {
    let Some(filter) = filter else {
        return Ok(arms);
    };
    let kept: Vec<Arm> = arms
        .into_iter()
        .filter(|arm| filter.contains(&family(&arm.label)))
        .collect();
    if kept.is_empty() {
        return Err(BenchError::Setup(format!(
            "--arms {}: leaves the cell {} no arm",
            filter.join(","),
            cell.as_str()
        )));
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::engine::{Pace, work};

    fn arm(label: &str) -> Arm {
        Arm {
            label: label.to_owned(),
            pace: Pace::Closed,
            in_flight: 1,
            requests: 1,
            cold_bytes_per_pass: 0,
            cold_bytes_once: 0,
            work: work(|_| async { Ok(vec![]) }),
            before: None,
            after: None,
            faulted: false,
            proxy: None,
            notes: vec![],
        }
    }

    fn arms() -> Vec<Arm> {
        [
            "direct-4kib-c1-healthy",
            "direct-mp-4kib-c1-healthy",
            "instance-4kib-c1-healthy",
            "direct-mp-1mib-open10-healthy",
        ]
        .into_iter()
        .map(arm)
        .collect()
    }

    fn filter(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn labels(arms: &[Arm]) -> Vec<&str> {
        arms.iter().map(|a| a.label.as_str()).collect()
    }

    /// Catches a family that swallows a neighbour (`direct` taking `direct-mp`), loses its
    /// hyphens, or is taken from a label that starts with a size.
    #[test]
    fn a_family_is_the_label_up_to_its_size() {
        assert_eq!(family("direct-4kib-c1-healthy"), "direct");
        assert_eq!(family("direct-mp-4kib-c1-healthy"), "direct-mp");
        assert_eq!(family("instance-1mib-open10-healthy"), "instance");
        assert_eq!(family("direct-100b-c1-healthy"), "direct");
        assert_eq!(family("4kib-c1"), "");
        assert_eq!(family("10x4kib-c1-x"), "");
    }

    /// Catches a family list that drifted from the arms the cells build: the family of every
    /// label a path makes is the path's name, and the cells with no path have none.
    #[test]
    fn the_families_of_a_cell_are_the_names_of_its_paths() {
        assert_eq!(
            families(CellId::ColdPut),
            ["direct", "direct-mp", "instance"]
        );
        assert_eq!(families(CellId::ColdRead), ["direct", "instance"]);
        assert_eq!(families(CellId::A1), ["hot", "instance"]);
        for cell in [CellId::B, CellId::C, CellId::G] {
            assert!(families(cell).is_empty(), "{cell:?}");
        }
        for cell in [CellId::ColdPut, CellId::ColdRead, CellId::A1] {
            for name in families(cell) {
                assert_eq!(family(&format!("{name}-4kib-c1-healthy")), name);
            }
        }
    }

    /// Catches a filter that keeps an arm it did not name (`direct` also keeping `direct-mp`),
    /// drops one it named, or reorders the arms.
    #[test]
    fn the_filter_keeps_only_the_named_arms_in_their_order() {
        let kept = select(
            CellId::ColdPut,
            arms(),
            Some(&filter(&["direct", "direct-mp"])),
        )
        .unwrap();
        assert_eq!(
            labels(&kept),
            [
                "direct-4kib-c1-healthy",
                "direct-mp-4kib-c1-healthy",
                "direct-mp-1mib-open10-healthy"
            ]
        );
        let kept = select(CellId::ColdPut, arms(), Some(&filter(&["instance"]))).unwrap();
        assert_eq!(labels(&kept), ["instance-4kib-c1-healthy"]);
    }

    /// Catches a run without `--arms` changing: every arm, in order.
    #[test]
    fn no_filter_is_no_change() {
        let kept = select(CellId::ColdPut, arms(), None).unwrap();
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[0].label, "direct-4kib-c1-healthy");
    }

    /// Catches a label no cell has being let by: refused, naming it, before anything is sent.
    #[test]
    fn a_label_no_selected_cell_has_is_refused() {
        let err = check(&[CellId::ColdPut], &filter(&["direct", "direct-mpp"])).unwrap_err();
        assert!(err.to_string().contains("direct-mpp"), "{err}");
        assert!(check(&[CellId::ColdPut], &filter(&["direct", "direct-mp"])).is_ok());
        // A family another selected cell has is enough.
        assert!(check(&[CellId::B, CellId::A1], &filter(&["hot"])).is_ok());
        // A cell with no families has none to name.
        let err = check(&[CellId::B], &filter(&["direct"])).unwrap_err();
        assert!(err.to_string().contains("direct"), "{err}");
        let err = check(&[CellId::ColdPut], &filter(&[""])).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// Catches a cell left with no arm running nothing and reporting success: refused, naming
    /// the cell.
    #[test]
    fn a_filter_that_leaves_a_cell_no_arm_is_refused_naming_the_cell() {
        let err = select(CellId::ColdPut, arms(), Some(&filter(&["hot"])))
            .err()
            .unwrap();
        assert!(err.to_string().contains("cold-put"), "{err}");
        let err = select(CellId::B, vec![arm("4kib-c1")], Some(&filter(&["direct"])))
            .err()
            .unwrap();
        assert!(err.to_string().contains("b"), "{err}");
    }
}
