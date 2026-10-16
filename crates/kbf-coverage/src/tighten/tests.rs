use super::*;

fn e(lines: Option<u64>, branches: Option<u64>) -> Entry {
    Entry { lines, branches }
}

fn file(rows: &[(&str, Entry)]) -> Baseline {
    rows.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

fn lowered(krate: &str, metric: Metric, from: Option<u64>, to: Option<u64>) -> Change {
    Change::Lowered {
        krate: krate.into(),
        metric,
        from,
        to,
    }
}

fn raised(krate: &str, metric: Metric, recorded: Option<u64>, measured: Option<u64>) -> Refusal {
    Refusal::Raised {
        krate: krate.into(),
        metric,
        recorded,
        measured,
    }
}

/// Catches: a looser value left in place, lowered to something other than the
/// measurement, or reported on the wrong metric; and `-` recorded where the crate now
/// measures 0 (looser, per the ratchet) left as `-`.
#[test]
fn lowers_each_looser_metric_to_the_measured_value() {
    let recorded = file(&[
        ("a", e(Some(4), Some(15))),
        ("b", e(Some(47), Some(8))),
        ("c", e(None, None)),
        ("d", e(Some(3), Some(0))),
    ]);
    let measured = file(&[
        ("a", e(Some(4), Some(12))),
        ("b", e(Some(45), Some(8))),
        ("c", e(Some(0), None)),
        ("d", e(Some(0), Some(0))),
    ]);
    let changes = plan(&recorded, &measured, &set(&["a", "b", "c", "d"])).expect("only lowers");
    assert_eq!(
        changes,
        [
            lowered("a", Metric::Branches, Some(15), Some(12)),
            lowered("b", Metric::Lines, Some(47), Some(45)),
            lowered("c", Metric::Lines, None, Some(0)),
            lowered("d", Metric::Lines, Some(3), Some(0)),
        ]
    );
    assert_eq!(apply(&recorded, &changes), measured);
    let shown: Vec<String> = changes.iter().map(Change::to_string).collect();
    assert_eq!(
        shown,
        [
            "a branches: 15 -> 12",
            "b lines: 47 -> 45",
            "c lines: - -> 0",
            "d lines: 3 -> 0",
        ]
    );
}

/// Catches: each kind of rise written instead of refused (a higher count, `-` to a
/// count above 0, a count to no data), or refused without naming the crate, the
/// metric and both values.
#[test]
fn refuses_every_kind_of_rise() {
    for (recorded, measured, metric) in [
        (e(Some(4), Some(15)), e(Some(5), Some(15)), Metric::Lines),
        (e(Some(4), Some(15)), e(Some(4), Some(16)), Metric::Branches),
        (e(None, None), e(Some(1), None), Metric::Lines),
        (e(Some(0), Some(0)), e(Some(0), None), Metric::Branches),
    ] {
        let got = plan(
            &file(&[("k", recorded)]),
            &file(&[("k", measured)]),
            &set(&["k"]),
        );
        let (from, to) = match metric {
            Metric::Lines => (recorded.lines, measured.lines),
            Metric::Branches => (recorded.branches, measured.branches),
        };
        assert_eq!(got, Err(vec![raised("k", metric, from, to)]), "{metric}");
    }
    let messages: Vec<String> = [
        raised("k", Metric::Lines, Some(4), Some(7)),
        raised("k", Metric::Lines, None, Some(1)),
        raised("k", Metric::Branches, Some(0), None),
    ]
    .iter()
    .map(Refusal::to_string)
    .collect();
    assert_eq!(
        messages,
        [
            "k lines: recorded 4, measured 7: more missed than recorded; cover the code",
            "k lines: recorded -, measured 1: more missed than recorded; cover the code",
            "k branches: recorded 0, measured -: more missed than recorded; cover the code",
        ]
    );
}

/// Catches: the lowerings of a run that also holds a rise written anyway, or a
/// refusal list that stops at the first offender instead of naming every one.
#[test]
fn one_rise_refuses_the_whole_run_and_every_rise_is_named() {
    let recorded = file(&[
        ("a", e(Some(4), Some(15))),
        ("b", e(Some(2), Some(2))),
        ("c", e(Some(9), Some(9))),
    ]);
    let measured = file(&[
        ("a", e(Some(3), Some(12))),
        ("b", e(Some(3), Some(1))),
        ("c", e(Some(9), Some(10))),
    ]);
    assert_eq!(
        plan(&recorded, &measured, &set(&["a", "b", "c"])),
        Err(vec![
            raised("b", Metric::Lines, Some(2), Some(3)),
            raised("c", Metric::Branches, Some(9), Some(10)),
        ])
    );
}

/// Catches: a new crate added whatever it measures (new code must arrive covered),
/// a fully covered one refused, or one added with values other than measured.
#[test]
fn adds_a_new_crate_only_when_it_arrives_covered() {
    let recorded = file(&[("a", e(Some(1), Some(1)))]);
    let ws = set(&["a", "new"]);
    for covered in [
        e(Some(0), None),
        e(None, None),
        e(Some(0), Some(0)),
        e(None, Some(0)),
    ] {
        let measured = file(&[("a", e(Some(1), Some(1))), ("new", covered)]);
        let changes = plan(&recorded, &measured, &ws).expect("covered");
        assert_eq!(
            changes,
            [Change::Added {
                krate: "new".into(),
                entry: covered,
            }]
        );
        assert_eq!(apply(&recorded, &changes), measured);
    }
    for uncovered in [e(Some(1), None), e(None, Some(2)), e(Some(0), Some(1))] {
        let measured = file(&[("a", e(Some(1), Some(1))), ("new", uncovered)]);
        assert_eq!(
            plan(&recorded, &measured, &ws),
            Err(vec![Refusal::NewCrateUncovered {
                krate: "new".into(),
                measured: uncovered,
            }])
        );
    }
    assert_eq!(
        Change::Added {
            krate: "kbf-storage".into(),
            entry: e(Some(0), None)
        }
        .to_string(),
        "kbf-storage: added as 0 -"
    );
    assert_eq!(
        Refusal::NewCrateUncovered {
            krate: "n".into(),
            measured: e(Some(3), None)
        }
        .to_string(),
        "n: not in the baseline, measured 3 -: a new crate must arrive covered (every value 0 or -)"
    );
}

/// Catches: a crate deleted from the workspace kept in the file (the ratchet would
/// fail it as NOT IN WORKSPACE), or a crate dropped from the file while its
/// `Cargo.toml` still exists (a measured file of another tree must not delete lines).
#[test]
fn removes_a_crate_only_when_it_is_gone_from_the_workspace() {
    let recorded = file(&[("a", e(Some(1), Some(1))), ("old", e(Some(3), None))]);
    let measured = file(&[("a", e(Some(1), Some(1)))]);
    let changes = plan(&recorded, &measured, &set(&["a"])).expect("gone");
    assert_eq!(
        changes,
        [Change::Removed {
            krate: "old".into(),
            entry: e(Some(3), None),
        }]
    );
    assert_eq!(apply(&recorded, &changes), measured);
    assert_eq!(
        changes[0].to_string(),
        "old: removed (was 3 -; crates/old/Cargo.toml is gone)"
    );
    assert_eq!(
        plan(&recorded, &measured, &set(&["a", "old"])),
        Err(vec![Refusal::Unmeasured {
            krate: "old".into()
        }])
    );
}

/// Catches: a crate-set difference other than an add or a remove written: a
/// workspace crate on neither side, and a measured crate with no `Cargo.toml` here
/// (whether or not the file lists it).
#[test]
fn refuses_other_crate_set_differences() {
    let one = file(&[("a", e(Some(0), Some(0)))]);
    assert_eq!(
        plan(&one, &one, &set(&["a", "b"])),
        Err(vec![Refusal::Unmeasured { krate: "b".into() }])
    );
    let measured = file(&[("a", e(Some(0), Some(0))), ("x", e(Some(0), Some(0)))]);
    assert_eq!(
        plan(&one, &measured, &set(&["a"])),
        Err(vec![Refusal::NotInWorkspace { krate: "x".into() }])
    );
    assert_eq!(
        plan(&measured, &measured, &set(&["a"])),
        Err(vec![Refusal::NotInWorkspace { krate: "x".into() }])
    );
    let messages: Vec<String> = [
        Refusal::Unmeasured { krate: "b".into() },
        Refusal::NotInWorkspace { krate: "x".into() },
    ]
    .iter()
    .map(Refusal::to_string)
    .collect();
    assert_eq!(
        messages,
        [
            "b: crates/b/Cargo.toml exists but the measured file has no line for it; measure this tree",
            "x: measured, but there is no crates/x/Cargo.toml; the measurement is of another tree",
        ]
    );
}

/// Catches: a second run that finds something to change after the first (the
/// result must equal the measurement), or a matching file reported as changed.
#[test]
fn a_tightened_file_has_nothing_left_to_change() {
    let recorded = file(&[
        ("a", e(Some(4), Some(15))),
        ("gone", e(Some(1), Some(1))),
        ("z", e(None, None)),
    ]);
    let measured = file(&[
        ("a", e(Some(4), Some(12))),
        ("new", e(Some(0), None)),
        ("z", e(Some(0), None)),
    ]);
    let ws = set(&["a", "new", "z"]);
    let once = apply(&recorded, &plan(&recorded, &measured, &ws).expect("ok"));
    assert_eq!(once, measured);
    assert_eq!(plan(&once, &measured, &ws), Ok(vec![]));
}
