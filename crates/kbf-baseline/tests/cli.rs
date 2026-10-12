//! The `kbf-baseline` binary end to end, on the CI captures in `tests/data`.
//!
//! Catches: the table's rows or columns moved or misnamed, the JSON not written or not
//! the report the library builds, a failed build turned into a non-zero exit (it is
//! data), and an unreadable log or `--json` path not exiting 2 with the path named.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn data(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
        .display()
        .to_string()
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-baseline"))
        .args(args)
        .output()
        .expect("run kbf-baseline")
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

#[test]
fn prints_the_table_and_writes_the_json() {
    let json = scratch("table").join("report.json");
    let logs = ["m1-first", "m1-second", "m1-failing"].map(|n| data(&format!("{n}.json-lines.gz")));
    let mut args: Vec<&str> = logs.iter().map(String::as_str).collect();
    args.extend(["--json", json.to_str().expect("utf-8 path")]);
    let out = run(&args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let table = String::from_utf8(out.stdout).expect("utf-8");
    let expected = "\
build                            result    wall ms remote  cache  local  other    hits failed
m1-first.json-lines.gz               ok     1012.1      5      0      0      0    0.0%      0
m1-second.json-lines.gz              ok      275.2      0      5      0      0  100.0%      0
m1-failing.json-lines.gz         FAILED      390.6      1      1      0      0   50.0%      1
total                               2/3     1678.0      6      6      0      0   50.0%      1

ms          count        p50        p90        p99        max
action         12       87.2      422.9      427.9      427.9
queue           6        1.4        1.7        1.7        1.7
execution       6        3.4        3.5        3.5        3.5
FAILED root//:verdict (step) remote: Failure, exit 3, build 9fb3b55a-9715-46b4-b386-4ad8661d7d63
";
    assert_eq!(table, expected);

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json).expect("the JSON was written")).expect("JSON");
    assert_eq!(report["builds"].as_array().map(Vec::len), Some(3));
    assert_eq!(report["builds"][1]["ran"]["cache_hit"], 5);
    assert_eq!(report["builds"][2]["success"], false);
    assert_eq!(report["total"]["queue_us"]["p50"], 1487);
    assert_eq!(report["total"]["cache_hit_share"], 0.5);
    assert_eq!(report["total"]["failures"][0]["target"], "root//:verdict");
    assert_eq!(report["total"]["failures"][0]["exit_code"], 3);
}

#[test]
fn an_unreadable_input_or_output_exits_2_naming_it() {
    let out = run(&["missing.json-lines"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("kbf-baseline: missing.json-lines: "),
        "{err}"
    );

    // A stored log (protobuf, zstd) is refused with the conversion to run.
    let out = run(&["buck-out/v2/log/x_build_events.pb.zst"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("buck2 log show PATH"));

    let bad = scratch("bad").join("no-such-dir").join("r.json");
    let out = run(&[
        &data("m1-first.json-lines.gz"),
        "--json",
        bad.to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("write ") && err.contains("r.json"), "{err}");
    assert!(out.stdout.is_empty(), "no table after a failed write");
}

#[test]
fn a_plain_log_reads_as_its_gzipped_copy_does() {
    // Catches: the `.json-lines` reader (no gzip) broken or reading another encoding,
    // and a run without `--json` writing or printing anything else.
    let dir = scratch("plain");
    let plain = dir.join("m1-failing.json-lines");
    let mut text = Vec::new();
    let gz = std::fs::File::open(data("m1-failing.json-lines.gz")).expect("open");
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz), &mut text).expect("gunzip");
    std::fs::write(&plain, text).expect("write the plain copy");
    let from_plain = run(&[plain.to_str().expect("utf-8")]);
    let from_gz = run(&[&data("m1-failing.json-lines.gz")]);
    assert!(from_plain.status.success() && from_gz.status.success());
    // The same table but for the log's name (and the padding after it).
    let tables = [from_plain.stdout, from_gz.stdout].map(|t| {
        let t = String::from_utf8(t)
            .expect("utf-8")
            .replace(".json-lines.gz", ".json-lines");
        t.lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
    });
    assert_eq!(tables[0], tables[1]);
    assert!(
        tables[1]
            .iter()
            .any(|l| l.starts_with("FAILED root//:verdict")),
        "{tables:?}"
    );
    assert_eq!(
        std::fs::read_dir(&dir).expect("list").count(),
        1,
        "only the plain copy"
    );
}
