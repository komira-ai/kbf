use super::{LegacyReads, Path};

/// Catches a counter whose rows are mixed up or whose scrape text drifts from what an
/// alert reads: a fresh counter renders every path at 0 under one HELP and TYPE, one count on
/// a path shows on that path's row only, and `total` is the sum.
#[test]
fn every_path_renders_and_a_count_moves_only_its_own_row() {
    let reads = LegacyReads::default();
    let mut text = String::new();
    reads.render(&mut text);
    let name = "kbf_storage_legacy_read_total";
    assert_eq!(text.matches(&format!("# TYPE {name} counter")).count(), 1);
    assert_eq!(text.matches(&format!("# HELP {name} ")).count(), 1);
    for labels in [
        r#"{kind="cas",source="bytestream"}"#,
        r#"{kind="cas",source="segments"}"#,
        r#"{kind="cas",source="in_process"}"#,
        r#"{kind="ac",source="action_store"}"#,
        r#"{kind="ac",source="in_process"}"#,
    ] {
        let row = format!("{name}{labels} 0");
        assert!(text.lines().any(|line| line == row), "{row} in {text}");
    }
    assert_eq!(reads.total(), 0);

    reads.count(Path::AcActionStore);
    reads.count(Path::AcActionStore);
    reads.count(Path::CasSegments);
    let mut text = String::new();
    reads.render(&mut text);
    let ac = format!(r#"{name}{{kind="ac",source="action_store"}} 2"#);
    let segments = format!(r#"{name}{{kind="cas",source="segments"}} 1"#);
    let untouched = format!(r#"{name}{{kind="cas",source="bytestream"}} 0"#);
    for row in [ac, segments, untouched] {
        assert!(text.lines().any(|line| line == row), "{row} in {text}");
    }
    assert_eq!(reads.get(Path::AcActionStore), 2);
    assert_eq!(reads.total(), 3);
}
