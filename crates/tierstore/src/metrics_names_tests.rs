//! The text of the proxy's counters is the text its family names make, and the names differ.

use std::collections::BTreeSet;

use crate::metrics::{
    BYTES_READ_TOTAL, BYTES_SERVED_TOTAL, Counters, HITS_TOTAL, MISSES_TOTAL, PROMOTED_BYTES_TOTAL,
    PROMOTIONS_SKIPPED_TOTAL, PROMOTIONS_TOTAL, READ_ERRORS_TOTAL, READS_TOTAL, Read, Skipped,
    Tier,
};

const FAMILIES: [&str; 9] = [
    HITS_TOTAL,
    MISSES_TOTAL,
    READ_ERRORS_TOTAL,
    READS_TOTAL,
    BYTES_SERVED_TOTAL,
    BYTES_READ_TOTAL,
    PROMOTIONS_TOTAL,
    PROMOTED_BYTES_TOTAL,
    PROMOTIONS_SKIPPED_TOTAL,
];

/// Asserts that the nine family names differ, start with `tierstore_` and end with `_total`.
#[test]
fn the_family_names_are_distinct_and_start_with_the_prefix() {
    let distinct: BTreeSet<&str> = FAMILIES.into_iter().collect();
    assert_eq!(distinct.len(), FAMILIES.len());
    for name in FAMILIES {
        assert!(name.starts_with("tierstore_"), "{name}");
        assert!(name.ends_with("_total"), "{name}");
    }
}

/// Asserts that the rendered text is, line by line and in order, the text built from the family
/// names and the labels, for counters driven to a different value in every cell.
#[test]
fn the_rendered_text_is_the_text_built_from_the_names() {
    let tiers = [(Tier::Hot, "hot"), (Tier::Cold, "cold")];
    let reads = [(Read::Get, "get"), (Read::Head, "head")];
    let skips = [
        (Skipped::Critical, "critical"),
        (Skipped::Full, "full"),
        (Skipped::Raced, "raced"),
        (Skipped::Failed, "failed"),
    ];
    let counters = Counters::default();
    let mut expected = String::new();
    let mut cell = 0_u64;
    for (tier, tier_label) in tiers {
        for (kind, op) in reads {
            cell += 1;
            let (hits, misses, errors) = (cell, 10 + cell, 20 + cell);
            for _ in 0..hits {
                counters.hit(tier, kind);
            }
            for _ in 0..misses {
                counters.miss(tier, kind);
            }
            for _ in 0..errors {
                counters.error(tier, kind);
            }
            let labels = format!("{{tier=\"{tier_label}\",op=\"{op}\"}}");
            expected.push_str(&format!("{HITS_TOTAL}{labels} {hits}\n"));
            expected.push_str(&format!("{MISSES_TOTAL}{labels} {misses}\n"));
            expected.push_str(&format!("{READ_ERRORS_TOTAL}{labels} {errors}\n"));
            let all = hits + misses + errors;
            expected.push_str(&format!("{READS_TOTAL}{labels} {all}\n"));
        }
    }
    for (index, (tier, tier_label)) in tiers.into_iter().enumerate() {
        let (served, read) = (100 * (index + 1), 1000 * (index + 1));
        counters.served(tier, served);
        counters.bytes_read(tier, read);
        let labels = format!("{{tier=\"{tier_label}\"}}");
        expected.push_str(&format!("{BYTES_SERVED_TOTAL}{labels} {served}\n"));
        expected.push_str(&format!("{BYTES_READ_TOTAL}{labels} {read}\n"));
    }
    counters.promoted(7);
    counters.promoted(5);
    expected.push_str(&format!("{PROMOTIONS_TOTAL} 2\n"));
    expected.push_str(&format!("{PROMOTED_BYTES_TOTAL} 12\n"));
    for (index, (why, label)) in skips.into_iter().enumerate() {
        for _ in 0..=index {
            counters.skipped(why);
        }
        let count = index + 1;
        expected.push_str(&format!(
            "{PROMOTIONS_SKIPPED_TOTAL}{{reason=\"{label}\"}} {count}\n"
        ));
    }
    assert_eq!(counters.render(), expected);
}

/// Asserts that every family name is on the metrics page outside its "Not in a release yet"
/// section, so a name the code exports is never missing from the page.
#[test]
fn every_family_name_is_on_the_metrics_page() {
    let page = include_str!("../docs/metrics.md");
    let released = page
        .split("\n## Not in a release yet")
        .next()
        .expect("a page has a start");
    for name in FAMILIES {
        assert!(released.contains(name), "{name} is not on the metrics page");
    }
}
