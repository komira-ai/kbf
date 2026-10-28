use super::*;

/// A shard's tracefile with one record.
fn shard(path: &str, da: &[(u64, u64)], brda: &[(u64, &str, &str, &str)]) -> String {
    let mut out = format!("TN:\nSF:{path}\nFN:1,f\nFNDA:1,f\n");
    for (line, count) in da {
        out.push_str(&format!("DA:{line},{count}\n"));
    }
    for (line, block, branch, taken) in brda {
        out.push_str(&format!("BRDA:{line},{block},{branch},{taken}\n"));
    }
    let hit = da.iter().filter(|(_, c)| *c > 0).count();
    let taken = brda.iter().filter(|b| b.3 != "-" && b.3 != "0").count();
    out.push_str(&format!(
        "LF:{}\nLH:{hit}\nBRF:{}\nBRH:{taken}\nend_of_record\n",
        da.len(),
        brda.len()
    ));
    out
}

fn inputs(texts: &[&str]) -> Vec<(String, String)> {
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| (format!("shard{}", i + 1), (*t).to_owned()))
        .collect()
}

fn merged(texts: &[&str]) -> Result<String, MergeError> {
    merge(&inputs(texts))
}

const A: &str = "/w/crates/a/src/lib.rs";

/// Catches: counts not added, a line hit by one shard reported missed, a branch taken
/// by one shard reported untaken, `-` mishandled in any of its four pairings with a
/// number or with `-`, the summary lines not rewritten from the merged detail, or the
/// merge of the parts differing from the one run of the whole. The parts split a whole
/// run so that no shard alone reaches what the whole does.
#[test]
fn the_parts_merge_into_the_whole() {
    let a = shard(
        A,
        &[(1, 2), (2, 0), (3, 0), (4, 0)],
        &[
            (2, "0", "0", "-"),
            (2, "0", "1", "-"),
            (3, "0", "0", "1"),
            (3, "0", "1", "0"),
            (4, "0", "0", "-"),
        ],
    );
    let b = shard(
        A,
        &[(1, 0), (2, 5), (3, 0), (4, 0)],
        &[
            (2, "0", "0", "5"),
            (2, "0", "1", "0"),
            (3, "0", "0", "0"),
            (3, "0", "1", "-"),
            (4, "0", "0", "-"),
        ],
    );
    let whole = shard(
        A,
        &[(1, 2), (2, 5), (3, 0), (4, 0)],
        &[
            (2, "0", "0", "5"),
            (2, "0", "1", "0"),
            (3, "0", "0", "1"),
            (3, "0", "1", "0"),
            (4, "0", "0", "-"),
        ],
    );
    let want = merged(&[&whole]).expect("whole");
    assert_eq!(merged(&[&a, &b]).expect("a b"), want);
    assert_eq!(merged(&[&b, &a]).expect("b a"), want);
    assert_eq!(
        want,
        format!(
            "TN:\nSF:{A}\nDA:1,2\nDA:2,5\nDA:3,0\nDA:4,0\n\
             BRDA:2,0,0,5\nBRDA:2,0,1,0\nBRDA:3,0,0,1\nBRDA:3,0,1,0\nBRDA:4,0,0,-\n\
             LF:4\nLH:2\nBRF:5\nBRH:2\nend_of_record\n"
        )
    );
}

/// Catches: sources of different shards run together or dropped, a source one shard
/// lacks, and counts that saturate wrongly: two records in each shard, merged by path.
#[test]
fn records_merge_by_source_and_counts_saturate() {
    let b = "/w/crates/b/src/lib.rs";
    let one = format!(
        "{}{}",
        shard(A, &[(1, u64::MAX)], &[]),
        shard(b, &[(7, 1)], &[])
    );
    let two = format!("{}{}", shard(b, &[(7, 0)], &[]), shard(A, &[(1, 3)], &[]));
    let got = merged(&[&one, &two]).expect("merged");
    assert!(
        got.contains(&format!("SF:{A}\nDA:1,{}\n", u64::MAX)),
        "{got}"
    );
    assert!(
        got.contains(&format!("SF:{b}\nDA:7,1\nLF:1\nLH:1\nBRF:0\nBRH:0\n")),
        "{got}"
    );
    assert_eq!(got.matches("end_of_record").count(), 2);
}

/// Catches: a checksum on a `DA` line, CRLF line ends, or the lines the merge does not
/// read (`TN`, `FN`, `FNDA`, a line with no colon) refused instead of ignored, a record
/// with no summary lines refused, and the summary lines not written for it.
#[test]
fn tolerates_what_llvm_cov_and_lcov_write() {
    let text = format!("TN:name\r\nSF:{A}\r\nnoise\r\nDA:1,4,abc\r\nend_of_record\r\n");
    assert_eq!(
        merged(&[&text]).expect("merged"),
        format!("TN:\nSF:{A}\nDA:1,4\nLF:1\nLH:1\nBRF:0\nBRH:0\nend_of_record\n")
    );
}

fn error(texts: &[&str]) -> MergeError {
    merged(texts).expect_err("refused")
}

fn malformed(text: &str, line: usize, what: &str) {
    assert_eq!(
        error(&[text]),
        MergeError::Malformed {
            input: "shard1".to_owned(),
            line,
            what: what.to_owned()
        },
        "{text}"
    );
}

/// Catches: each malformed shape the module docs list being merged, which would let a
/// damaged shard pass the gate with a total no run had.
#[test]
fn refuses_malformed_tracefiles() {
    malformed("end_of_record\n", 1, "end_of_record outside a record");
    malformed("DA:1,1\n", 1, "DA outside a record");
    malformed("BRDA:1,0,0,1\n", 1, "BRDA outside a record");
    malformed("LF:1\n", 1, "LF outside a record");
    malformed("SF:a\nSF:b\n", 2, "SF while a record is open");
    malformed("SF:a\nend_of_record\nSF:a\n", 3, "a appears in two records");
    malformed("SF:a\nDA:1\n", 2, "DA:1 is not a line and a count");
    malformed("SF:a\nDA:x,1\n", 2, "DA:x,1 is not a line and a count");
    malformed("SF:a\nDA:1,-1\n", 2, "DA:1,-1 is not a line and a count");
    malformed("SF:a\nBRDA:1,0,0\n", 2, "BRDA:1,0,0 is not a branch");
    malformed("SF:a\nBRDA:1,0,0,x\n", 2, "BRDA:1,0,0,x is not a branch");
    malformed("SF:a\nBRDA:y,0,0,1\n", 2, "BRDA:y,0,0,1 is not a branch");
    malformed("SF:a\nBRF:-1\n", 2, "BRF:-1 is not a count");
    malformed("SF:a\nLH:1\nLH:1\n", 3, "LH given twice in one record");
    malformed("SF:a\nDA:1,1\n", 2, "the record for a has no end_of_record");
}

/// Catches: a tracefile whose summary lines disagree with its detail lines being
/// merged, each of the four summaries; and one with no records at all passing as an
/// empty merge.
#[test]
fn refuses_summaries_that_disagree_with_the_detail() {
    let cases = [
        ("SF:a\nDA:1,1\nLF:2\nend_of_record\n", "LF", 2, 1),
        ("SF:a\nDA:1,1\nDA:2,0\nLH:2\nend_of_record\n", "LH", 2, 1),
        ("SF:a\nBRDA:1,0,0,-\nBRF:2\nend_of_record\n", "BRF", 2, 1),
        ("SF:a\nBRDA:1,0,0,0\nBRH:1\nend_of_record\n", "BRH", 1, 0),
    ];
    for (text, field, stated, listed) in cases {
        assert_eq!(
            error(&[text]),
            MergeError::Summary {
                input: "shard1".to_owned(),
                path: "a".to_owned(),
                field,
                stated,
                listed
            },
            "{text}"
        );
    }
    assert_eq!(
        error(&["TN:\n"]),
        MergeError::Empty {
            input: "shard1".to_owned()
        }
    );
    assert_eq!(merge(&[]), Err(MergeError::NoInputs));
}

/// Catches: shards of different builds merged by position: a shard with another set of
/// sources (fewer, or as many but others), other lines, or other branches (another
/// line, block or branch number), and an error that does not name the input, the first
/// shard and what differs.
#[test]
fn refuses_shards_that_do_not_list_the_same_things() {
    let base = shard(A, &[(1, 1), (2, 1)], &[(2, "0", "0", "1")]);
    let two_files = format!("{base}{}", shard("/w/b.rs", &[(1, 1)], &[]));
    let other_file = shard("/w/other.rs", &[(1, 1), (2, 1)], &[(2, "0", "0", "1")]);
    let other_lines = shard(A, &[(1, 1), (3, 1)], &[(2, "0", "0", "1")]);
    let fewer_lines = shard(A, &[(1, 1)], &[(2, "0", "0", "1")]);
    let other_line = shard(A, &[(1, 1), (2, 1)], &[(3, "0", "0", "1")]);
    let other_block = shard(A, &[(1, 1), (2, 1)], &[(2, "1", "0", "1")]);
    let other_branch = shard(A, &[(1, 1), (2, 1)], &[(2, "0", "1", "1")]);
    let fewer_branches = shard(A, &[(1, 1), (2, 1)], &[]);
    let cases = [
        (&two_files, "how many sources it lists (2 against 1)"),
        (
            &other_file,
            "the sources it lists (/w/other.rs is not in the first)",
        ),
        (
            &other_lines,
            "the lines it lists for /w/crates/a/src/lib.rs",
        ),
        (
            &fewer_lines,
            "the lines it lists for /w/crates/a/src/lib.rs",
        ),
        (
            &other_line,
            "the branches it lists for /w/crates/a/src/lib.rs",
        ),
        (
            &other_block,
            "the branches it lists for /w/crates/a/src/lib.rs",
        ),
        (
            &other_branch,
            "the branches it lists for /w/crates/a/src/lib.rs",
        ),
        (
            &fewer_branches,
            "the branches it lists for /w/crates/a/src/lib.rs",
        ),
    ];
    for (other, what) in cases {
        let e = error(&[&base, other]);
        assert_eq!(
            e,
            MergeError::Differs {
                input: "shard2".to_owned(),
                first: "shard1".to_owned(),
                what: what.to_owned()
            }
        );
        assert!(
            e.to_string().starts_with("shard2 differs from shard1 in "),
            "{e}"
        );
    }
}

/// Catches: an error message that drops the input, the line or the values a reader needs.
#[test]
fn errors_name_the_place() {
    let messages = [
        MergeError::NoInputs.to_string(),
        error(&["SF:a\nSF:b\n"]).to_string(),
        error(&["SF:a\nLF:2\nend_of_record\n"]).to_string(),
        error(&[""]).to_string(),
    ];
    assert_eq!(
        messages,
        [
            "no tracefiles to merge",
            "shard1: line 2: SF while a record is open",
            "shard1: a: LF says 2 but its detail lines give 0",
            "shard1 holds no records",
        ]
    );
}
