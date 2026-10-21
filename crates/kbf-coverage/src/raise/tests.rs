use super::*;
use crate::Counts;
use crate::floors;

fn floors_of(text: &str) -> Floors {
    floors::parse(text).expect("floors")
}

fn cov(lines: (u64, u64), branches: (u64, u64)) -> CrateCoverage {
    CrateCoverage {
        lines: Counts::new(lines.0, lines.1).expect("valid"),
        branches: Counts::new(branches.0, branches.1).expect("valid"),
    }
}

fn measured(rows: &[(&str, CrateCoverage)]) -> BTreeMap<String, CrateCoverage> {
    rows.iter().map(|(k, c)| ((*k).to_owned(), *c)).collect()
}

/// Catches: a floor that does not follow improved coverage up, one that follows it
/// down, or a rounded-up floor the measuring run would then fail.
#[test]
fn a_floor_rises_to_the_measurement_and_never_falls() {
    let f = floors_of("a 90.00 80.00\nb 90.00 80.00\n");
    // a: lines 9100/10000 (rises), branches 7000/10000 (below the floor: stays).
    // b: lines 3/4 = 75 (below: stays), branches 2/3 = 66.66 (below: stays).
    let m = measured(&[
        ("a", cov((10_000, 9_100), (10_000, 7_000))),
        ("b", cov((4, 3), (3, 2))),
    ]);
    let (got, changes) = plan(&f, &m);
    assert_eq!(got, floors_of("a 91.00 80.00\nb 90.00 80.00\n"));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].to_string(), "a lines: 90.00 -> 91.00");
    // 2/3 is 66.66, floored; 1000003 items with one missing is 99.99, never 100.
    let (got, _) = plan(
        &floors_of("c 10.00 10.00\n"),
        &measured(&[("c", cov((3, 2), (200, 199)))]),
    );
    assert_eq!(got, floors_of("c 66.66 -\n"));
}

/// Catches: a floor kept after the metric reached 99%, or after the crate's last
/// code went, either of which leaves a looser threshold than the default.
#[test]
fn a_floor_goes_once_99_is_reached_or_nothing_is_measured() {
    let f = floors_of("a 90.00 80.00\nb 90.00 -\nc 90.00 -\nd 90.00 -\n");
    let m = measured(&[
        ("a", cov((100, 99), (10, 8))),
        ("b", cov((100, 98), (0, 0))),
        ("c", cov((0, 0), (0, 0))),
    ]);
    let (got, changes) = plan(&f, &m);
    assert_eq!(got, floors_of("a - 80.00\nb 98.00 -\n"));
    let shown: Vec<String> = changes.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "a lines: 90.00 -> removed (99.00 applies)",
            "b lines: 90.00 -> 98.00",
            "c lines: 90.00 -> removed (99.00 applies)",
            "d lines: 90.00 -> removed (99.00 applies)",
        ]
    );
    // The other metric of a crate that lost its last floor goes with the line.
    let (got, _) = plan(
        &floors_of("e 50.00 -\n"),
        &measured(&[("e", cov((1, 1), (0, 0)))]),
    );
    assert!(got.is_empty());
}

/// Catches: `raise` creating a floor for a crate or metric that has none, which would
/// lower its threshold from 99.00.
#[test]
fn raise_never_adds_a_floor() {
    let f = floors_of("a 90.00 -\n");
    let m = measured(&[
        ("a", cov((100, 95), (10, 1))),
        ("new", cov((100, 10), (10, 1))),
    ]);
    let (got, _) = plan(&f, &m);
    assert_eq!(got.len(), 1);
    assert_eq!(got["a"].branches, None);
    assert_eq!(got["a"].lines, Percent::parse("95.00"));
}

/// Catches: a floor lowered, or a floor added, passing the no-lowering check; a raise
/// or a removal failing it.
#[test]
fn lowered_reports_every_threshold_that_fell() {
    let base = floors_of("a 90.00 80.00\nb 50.00 -\n");
    assert_eq!(lowered(&base, &base), []);
    // Raised, removed: fine.
    assert_eq!(lowered(&base, &floors_of("a 91.00 80.00\n")), []);
    assert_eq!(lowered(&base, &Floors::new()), []);
    // Lowered by one hundredth.
    let got = lowered(&base, &floors_of("a 89.99 80.00\nb 50.00 -\n"));
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].to_string(),
        "a lines: threshold 90.00 -> 89.99 (a floor may only rise)"
    );
    // Branches count too, and a floor on a crate that had none lowers it from 99.
    let got = lowered(&base, &floors_of("a 90.00 79.00\nb 50.00 -\nnew - 10.00\n"));
    let shown: Vec<String> = got.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "a branches: threshold 80.00 -> 79.00 (a floor may only rise)",
            "new branches: threshold 99.00 -> 10.00 (a floor was added; a crate under 99% needs tests, not a floor)",
        ]
    );
    // A floor added for the metric of an existing line lowers that metric.
    let got = lowered(&base, &floors_of("a 90.00 80.00\nb 50.00 10.00\n"));
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].krate.as_str(), got[0].metric),
        ("b", Metric::Branches)
    );
    // Lines and branches both lowered on one crate.
    assert_eq!(
        lowered(&base, &floors_of("a 1.00 1.00\nb 50.00 -\n")).len(),
        2
    );
}

/// Catches: advice shown when nothing can rise, or without the command to run.
#[test]
fn advice_names_each_change_and_the_command() {
    assert_eq!(advice(&[], "x"), None);
    let change = Change {
        krate: "k".into(),
        metric: Metric::Branches,
        from: Percent::parse("1.00").expect("p"),
        to: None,
    };
    let text = advice(&[change], "cmd --flag").expect("advice");
    assert_eq!(
        text,
        "floors that can rise (not a failure):\n  k branches: 1.00 -> removed (99.00 applies)\nraise them with: cmd --flag\n"
    );
}
