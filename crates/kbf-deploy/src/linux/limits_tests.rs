//! Rules `caps`, `swap` and `oom` read on their own.

use super::*;

fn rules(text: &str) -> Vec<&'static str> {
    let unit = UnitFile::parse(text).expect("a unit");
    check(&unit).into_iter().map(|(rule, _)| rule).collect()
}

const OK: &str = "[Service]\nOOMScoreAdjust=-900\nCPUWeight=200\n";

/// Catches each limit the unit must not have read as fine (a cap in `[Unit]` or `[Slice]`
/// is still the unit's), a limit reset by an empty value refused, a score of 0, a
/// positive one or a word accepted, a missing score passing, and the last of two scores
/// not the one read.
#[test]
fn a_build_units_own_limits_are_read_as_systemd_reads_them() {
    assert_eq!(rules(OK), Vec::<&str>::new());
    let rows: [(&str, Vec<&str>); 13] = [
        ("MemoryMax=1G", vec!["caps"]),
        ("MemoryHigh=1G", vec!["caps"]),
        ("CPUQuota=100%", vec!["caps"]),
        ("AllowedCPUs=0-3", vec!["caps"]),
        ("MemorySwapMax=0", vec!["swap"]),
        ("MemorySwapMax=0\nMemorySwapMax=", vec![]),
        ("MemoryMax=1G\nMemoryMax=", vec![]),
        ("OOMScoreAdjust=500", vec!["oom"]),
        ("OOMScoreAdjust=0", vec!["oom"]),
        ("OOMScoreAdjust=soon", vec!["oom"]),
        ("OOMScoreAdjust=500\nOOMScoreAdjust=-1", vec![]),
        ("OOMScoreAdjust=-1\nOOMScoreAdjust=500", vec!["oom"]),
        ("MemoryMax=1G\nMemorySwapMax=0", vec!["caps", "swap"]),
    ];
    for (extra, want) in rows {
        // The score of OK stays unless the row sets its own.
        let base = if extra.contains("OOMScoreAdjust") {
            "[Service]\n"
        } else {
            "[Service]\nOOMScoreAdjust=-900\n"
        };
        assert_eq!(rules(&format!("{base}{extra}\n")), want, "{extra}");
    }
    assert_eq!(rules("[Service]\n"), vec!["oom"], "no score");
    assert_eq!(
        rules("[Unit]\nMemoryMax=1G\n[Service]\nOOMScoreAdjust=-1\n"),
        vec!["caps"],
        "a cap in another section"
    );
}
