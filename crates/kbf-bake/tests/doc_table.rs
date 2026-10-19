//! The evaluator's gate table and the table of `docs/design/kbf-cicd.md` section 7 are the same
//! data. The page says a change to a `[P]` threshold is one change to the page and to the
//! table; this test is what makes that so: it reads the page and compares it with
//! `kbf_bake::table`, and fails when either has moved without the other.

use std::path::Path;

use kbf_bake::table::{prose, rows};

fn page() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/kbf-cicd.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Section 7 of the page: from its heading to the next `### ` or `## ` heading after the table.
fn section_seven(page: &str) -> Result<&str, String> {
    let from = page.find("## 7. The ").ok_or("the page has no section 7")?;
    let rest = &page[from..];
    let to = rest.find("### 7.1").ok_or("section 7 has no 7.1")?;
    Ok(&rest[..to])
}

/// The cells of each row of the gate table, header and separator skipped.
fn doc_rows(section: &str) -> Vec<Vec<String>> {
    section
        .lines()
        .filter(|l| l.starts_with('|'))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(collapse)
                .collect::<Vec<_>>()
        })
        .filter(|cells| cells.len() == 4 && cells[0] != "gate" && !cells[0].starts_with("---"))
        .collect()
}

/// Whether `page` and the table agree; the error names the first difference.
fn check(page: &str) -> Result<(), String> {
    let section = section_seven(page)?;
    let flat = collapse(section);
    for needle in prose() {
        if !flat.contains(&needle) {
            return Err(format!("the page does not say `{needle}`"));
        }
    }
    let doc = doc_rows(section);
    let table = rows();
    if doc.len() != table.len() {
        return Err(format!(
            "the page has {} gates, the table {}",
            doc.len(),
            table.len()
        ));
    }
    for (cells, row) in doc.iter().zip(&table) {
        let gate = row.gate;
        if cells[0] != gate {
            return Err(format!(
                "gate `{}` on the page, `{gate}` in the table",
                cells[0]
            ));
        }
        if cells[3] != row.hard {
            return Err(format!(
                "`{gate}`: hard? `{}` on the page, `{}` in the table",
                cells[3], row.hard
            ));
        }
        for m in &row.measure_has {
            if !cells[1].contains(m) {
                return Err(format!("`{gate}`: the measure does not name `{m}`"));
            }
        }
        for t in &row.threshold_has {
            if !cells[2].contains(t.as_str()) {
                return Err(format!(
                    "`{gate}`: the threshold does not say `{t}`: `{}`",
                    cells[2]
                ));
            }
        }
    }
    Ok(())
}

/// Catches a threshold changed in the table and not on the page, or on the page and not in the
/// table: the page as committed matches every row and every load number.
#[test]
fn the_page_and_the_table_say_the_same() {
    assert_eq!(check(&page()), Ok(()));
}

/// Catches a check that cannot fail: each planted difference in the page is reported.
#[test]
fn each_difference_is_found() {
    let page = page();
    for (from, to, want) in [
        (
            "**[P]** 1.5 times the baseline",
            "**[P]** 1.6 times the baseline",
            "`latency`: the threshold does not say `not above **[P]** 1.5 times the baseline`",
        ),
        (
            "yes if above 3 times",
            "yes if above 4 times",
            "`latency`: hard?",
        ),
        (
            "more than **[P]** 10 minutes",
            "more than **[P]** 15 minutes",
            "`under-replicated`: the threshold does not say",
        ),
        (
            "the larger of twice the baseline",
            "the larger of thrice the baseline",
            "`error rate`: the threshold does not say",
        ),
        (
            "**[P]** 0.5 percentage points",
            "**[P]** 1 percentage points",
            "`error rate`: the threshold does not say",
        ),
        (
            "**[P]** 1000 executed actions",
            "**[P]** 2000 executed actions",
            "the page does not say `at least **[P]** 1000 executed actions",
        ),
        ("and 10 builds", "and 12 builds", "the page does not say"),
        (
            "at least 3 distinct hours",
            "at least 4 distinct hours",
            "the page does not say",
        ),
        (
            "## 7. The 24 hour bake",
            "## 7. The 12 hour bake",
            "the page does not say `## 7. The 24 hour bake`",
        ),
        (
            "| restarts |",
            "| reboots |",
            "gate `reboots` on the page, `restarts` in the table",
        ),
        (
            "`kbf_cache_lost_blobs` at every scrape",
            "the lost blobs gauge at every scrape",
            "`lost blobs`: the measure does not name `kbf_cache_lost_blobs`",
        ),
    ] {
        assert!(
            page.contains(from),
            "the page no longer has `{from}`: update this test with it"
        );
        let err = check(&page.replacen(from, to, 1)).unwrap_err();
        assert!(err.contains(want), "{from} -> {to}: {err}");
    }
    // A gate added on the page and not in the table.
    let extra = page.replacen(
        "| restarts |",
        "| disk |  `kbf_disk` | 0 | yes |\n| restarts |",
        1,
    );
    assert!(
        check(&extra)
            .unwrap_err()
            .contains("the page has 9 gates, the table 8")
    );
    assert!(
        check("no such section")
            .unwrap_err()
            .contains("no section 7")
    );
    assert!(
        check("## 7. The 24 hour bake\n")
            .unwrap_err()
            .contains("no 7.1")
    );
}
