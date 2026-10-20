use super::*;
use crate::floors;

fn counts(found: u64, hit: u64) -> Counts {
    Counts::new(found, hit).expect("valid")
}

fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
    CrateCoverage {
        lines: counts(lf, lh),
        branches: counts(brf, brh),
    }
}

fn file(path: &str, c: CrateCoverage) -> FileRecord {
    FileRecord {
        path: path.into(),
        coverage: c,
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

/// A row for crate `k` with floors from a floors-file line (`"90.00 -"`, or `""`).
fn row(measured: Option<CrateCoverage>, floor: &str) -> Row {
    let floors = floors::parse(&if floor.is_empty() {
        String::new()
    } else {
        format!("k {floor}")
    })
    .expect("floors");
    Row {
        krate: "k".into(),
        measured,
        floor: floors.get("k").copied().unwrap_or_default(),
    }
}

/// 10000 lines or branches with `hit` of them run.
fn of(hit: u64) -> Counts {
    counts(10_000, hit)
}

fn lines(hit: u64) -> CrateCoverage {
    CrateCoverage {
        lines: of(hit),
        branches: of(10_000),
    }
}

fn branches(hit: u64) -> CrateCoverage {
    CrateCoverage {
        lines: of(10_000),
        branches: of(hit),
    }
}

/// Catches: records summed into the wrong crate, a crate without records left out
/// of the table, or a source outside every crate silently dropped.
#[test]
fn sums_records_per_crate() {
    let crates = set(&["a", "b", "empty"]);
    let files = [
        file("/w/crates/a/src/lib.rs", cov(10, 9, 4, 3)),
        file("/w/crates/a/src/x/y.rs", cov(5, 5, 0, 0)),
        file("/w/crates/b/src/main.rs", cov(2, 0, 2, 0)),
    ];
    let got = by_crate(Path::new("/w"), &crates, &files).expect("attributed");
    assert_eq!(got["a"], cov(15, 14, 4, 3));
    assert_eq!(got["b"], cov(2, 0, 2, 0));
    assert_eq!(got["empty"], CrateCoverage::default());
    assert_eq!(got.len(), 3);
    for outside in [
        "/w/src/lib.rs",           // not under crates/
        "/v/crates/a/src/lib.rs",  // another root
        "/w/crates/zz/src/lib.rs", // not a workspace crate
        "/w/crates/a",             // the crate directory itself, not a file in it
    ] {
        assert_eq!(
            by_crate(Path::new("/w"), &crates, &[file(outside, cov(1, 1, 0, 0))]),
            Err(Unattributed(outside.into())),
        );
    }
    assert_eq!(
        Unattributed("/w/x.rs".into()).to_string(),
        "/w/x.rs is not under <root>/crates/<crate>/ for any workspace crate"
    );
}

/// Catches: the default threshold off by one hit or one hundredth: a crate at 98.99%
/// with no floor must fail, and one at exactly 99.00% must pass (`>` for `>=`).
#[test]
fn with_no_floor_99_00_passes_and_98_99_fails() {
    assert_eq!(row(Some(lines(9_900)), "").status(), Status::Ok);
    assert_eq!(
        row(Some(lines(9_899)), "").status(),
        Status::Below(vec![Metric::Lines])
    );
    assert_eq!(row(Some(lines(10_000)), "").status(), Status::Ok);
}

/// Catches: branches left out of the gate, or gated against the lines' threshold:
/// the same two cases hold for branches, and both metrics are reported when both fail.
#[test]
fn branches_are_gated_like_lines() {
    assert_eq!(row(Some(branches(9_900)), "").status(), Status::Ok);
    assert_eq!(
        row(Some(branches(9_899)), "").status(),
        Status::Below(vec![Metric::Branches])
    );
    let both = CrateCoverage {
        lines: of(9_899),
        branches: of(9_899),
    };
    assert_eq!(
        row(Some(both), "").status(),
        Status::Below(vec![Metric::Lines, Metric::Branches])
    );
    // A floor on one metric does not loosen the other.
    assert_eq!(
        row(Some(branches(9_000)), "50.00 -").status(),
        Status::Below(vec![Metric::Branches])
    );
    assert_eq!(
        row(Some(lines(9_000)), "- 50.00").status(),
        Status::Below(vec![Metric::Lines])
    );
}

/// Catches: a crate with no branches failing the branch gate (0 of 0 is 0%), or a
/// crate with no lines failing the line gate.
#[test]
fn nothing_to_measure_passes() {
    let no_branches = cov(10_000, 10_000, 0, 0);
    assert_eq!(row(Some(no_branches), "").status(), Status::Ok);
    assert_eq!(row(Some(CrateCoverage::default()), "").status(), Status::Ok);
}

/// Catches: a floor ignored (the crate held to 99 anyway), a floor applied with `>`,
/// or a crate allowed to fall under its floor.
#[test]
fn a_floor_replaces_the_threshold_for_its_metric() {
    assert_eq!(row(Some(lines(9_000)), "90.00 -").status(), Status::Ok);
    assert_eq!(
        row(Some(lines(8_999)), "90.00 -").status(),
        Status::Below(vec![Metric::Lines])
    );
    assert_eq!(row(Some(lines(9_500)), "90.00 -").status(), Status::Ok);
    assert_eq!(row(Some(branches(7_748)), "- 77.48").status(), Status::Ok);
    assert_eq!(
        row(Some(branches(7_747)), "- 77.48").status(),
        Status::Below(vec![Metric::Branches])
    );
}

/// Catches: a floor kept for code no test runs any more, which would let a crate
/// lose its instrumentation and still pass its floor; and a floors line for a crate
/// the workspace lost.
#[test]
fn a_floor_needs_data_and_a_workspace_crate() {
    assert_eq!(
        row(Some(CrateCoverage::default()), "90.00 80.00").status(),
        Status::NoData(vec![Metric::Lines, Metric::Branches])
    );
    assert_eq!(
        row(Some(cov(10_000, 9_000, 0, 0)), "90.00 80.00").status(),
        Status::NoData(vec![Metric::Branches])
    );
    // A shortfall is reported ahead of missing data.
    assert_eq!(
        row(Some(cov(10_000, 100, 0, 0)), "90.00 80.00").status(),
        Status::Below(vec![Metric::Lines])
    );
    assert_eq!(row(None, "90.00 -").status(), Status::Stale);
    for s in [
        Status::Ok,
        Status::Below(vec![Metric::Lines, Metric::Branches]),
        Status::NoData(vec![Metric::Branches]),
        Status::Stale,
    ] {
        assert_eq!(s.fails(), s != Status::Ok);
    }
}

/// Catches: crates missing from the report: workspace crates with no floor, and floors
/// lines for crates the workspace lacks, each appear once, in name order.
#[test]
fn rows_cover_workspace_and_floors() {
    let measured: BTreeMap<String, CrateCoverage> = [
        ("a".to_owned(), cov(4, 4, 0, 0)),
        ("c".to_owned(), cov(4, 2, 0, 0)),
    ]
    .into();
    let floors = floors::parse("b 10.00 -\nc 50.00 -\n").expect("floors");
    let rows = rows(&measured, &floors);
    let names: Vec<&str> = rows.iter().map(|r| r.krate.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    assert_eq!(rows[0].floor, Floor::default());
    assert_eq!(rows[1].measured, None);
    assert_eq!(rows[1].status(), Status::Stale);
    assert_eq!(rows[2].status(), Status::Ok);
    assert_eq!(rows[2].floor.threshold(Metric::Lines).to_string(), "50.00");
}

/// Catches: a table that hides a failing crate, shows the wrong threshold, or a
/// verdict line that disagrees with the rows.
#[test]
fn renders_the_table_and_the_verdict() {
    let measured: BTreeMap<String, CrateCoverage> = [
        ("a".to_owned(), cov(10_000, 9_899, 0, 0)),
        ("b".to_owned(), cov(4, 4, 2, 1)),
        ("e".to_owned(), CrateCoverage::default()),
    ]
    .into();
    let floors = floors::parse("b - 50.00\n").expect("floors");
    let text = render(&rows(&measured, &floors));
    let want = "\
| crate | lines | line % | line threshold | branches | branch % | branch threshold | status |
|---|---:|---:|---:|---:|---:|---:|---|
| a | 9899/10000 | 98.99 | 99.00 | - | - | 99.00 | BELOW THRESHOLD (lines) |
| b | 4/4 | 100.00 | 99.00 | 1/2 | 50.00 | 50.00 | ok |
| e | - | - | 99.00 | - | - | 99.00 | ok |
| **workspace** | 9903/10004 | 98.99 | 99.00 | 1/2 | 50.00 | 99.00 |  |

coverage gate: FAIL (1 crate(s) need attention; see status)
";
    assert_eq!(text, want);
    let ok = render(&rows(&measured, &floors)[1..]);
    assert!(ok.ends_with("coverage gate: PASS\n"), "{ok}");
}

/// Catches: status labels that stop naming the metric or the way out.
#[test]
fn status_labels() {
    assert_eq!(Status::Ok.to_string(), "ok");
    assert_eq!(
        Status::Below(vec![Metric::Lines, Metric::Branches]).to_string(),
        "BELOW THRESHOLD (lines, branches)"
    );
    assert_eq!(
        Status::NoData(vec![Metric::Branches]).to_string(),
        "FLOOR BUT NOTHING MEASURED (branches); remove the floor"
    );
    assert_eq!(Status::Stale.to_string(), "NOT IN WORKSPACE");
}
