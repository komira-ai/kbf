use super::*;

#[test]
fn the_properties_are_set_only_by_a_value_that_is_not_blank() {
    // Catches: the key's absence (`{}`, what buck2 prints for an unset key), an empty or blank
    // value, another key, a value that is not text, or output that is not JSON read as set.
    assert!(has_value(
        r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#
    ));
    for json in [
        "{}",
        r#"{"buildrules_re.linux_x86_64_properties":""}"#,
        r#"{"buildrules_re.linux_x86_64_properties":"  "}"#,
        r#"{"buildrules_re.linux_aarch64_properties":"OSFamily=Linux"}"#,
        r#"{"buildrules_re.linux_x86_64_properties":["OSFamily=Linux"]}"#,
        "not json",
        "",
    ] {
        assert!(!has_value(json), "{json}");
    }
}

#[test]
fn the_summary_is_the_last_commands_line_and_its_local_count() {
    // Catches: a summary read from an earlier line, a timestamp prefix kept in the line, or the
    // count of another field (cached, remote) read as the local one.
    let log = "\
[t] Commands: 9 (cached: 0, remote: 0, local: 9)
[t] Cache hits: 100%
[t] Commands: 3356 (cached: 3356, remote: 0, local: 0)
BUILD SUCCEEDED
";
    assert_eq!(
        summary(log),
        Some(("Commands: 3356 (cached: 3356, remote: 0, local: 0)", 0))
    );
    assert_eq!(
        summary("Commands: 2 (cached: 0, remote: 1, local: 12)"),
        Some(("Commands: 2 (cached: 0, remote: 1, local: 12)", 12))
    );
}

#[test]
fn a_log_with_no_readable_summary_has_none() {
    // Catches: a build log with no summary, or one whose local count is missing or not a number,
    // read as `local: 0`.
    for log in [
        "",
        "BUILD SUCCEEDED\n",
        "Commands: 2 (cached: 2, remote: 0)\n",
        "Commands: 2 (cached: 2, remote: 0, local: none)\n",
    ] {
        assert_eq!(summary(log), None, "{log}");
    }
}

#[test]
fn every_error_says_what_is_wrong() {
    // Catches: an error whose words do not name the key, the target, the status or the line.
    let cases = [
        (
            Error::Start {
                program: PathBuf::from("/x/buck2"),
                why: "No such file or directory (os error 2)".to_owned(),
            },
            "cannot run \"/x/buck2\": No such file or directory (os error 2)",
        ),
        (
            Error::Audit {
                status: "exit status: 3".to_owned(),
                stderr: "no .buckconfig".to_owned(),
            },
            "`buck2 audit config` failed (exit status: 3): no .buckconfig",
        ),
        (
            Error::NoWorkers,
            "buildrules_re.linux_x86_64_properties is empty: `farm-config` wrote no worker \
             properties, so buck2 has no farm to build on",
        ),
        (
            Error::Build {
                targets: "//:ci //:sync_check".to_owned(),
                status: "exit status: 3".to_owned(),
            },
            "`buck2 build //:ci //:sync_check` failed (exit status: 3)",
        ),
        (
            Error::NoSummary,
            "the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)",
        ),
        (
            Error::Local("Commands: 2 (cached: 0, remote: 0, local: 2)".to_owned()),
            "`Commands: 2 (cached: 0, remote: 0, local: 2)`: actions ran on this machine, not on \
             the farm (local must be 0)",
        ),
    ];
    for (error, words) in cases {
        assert_eq!(error.to_string(), words);
    }
}

/// A log that refuses every write.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the log is closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn every_line_goes_to_the_log_and_is_kept_even_when_the_log_refuses_it() {
    // Catches: a line dropped (the last one, with no newline, included), lines written out of
    // order, or a log that cannot be written losing the text the summary is read from.
    let from = "[t] Waiting\n[t] Commands: 1 (cached: 1, remote: 0, local: 0)\nBUILD SUCCEEDED";
    let mut log = Vec::new();
    assert_eq!(tee(from.as_bytes(), &mut log), from.as_bytes());
    assert_eq!(log, from.as_bytes());
    assert_eq!(tee(from.as_bytes(), &mut Refusing), from.as_bytes());
    assert_eq!(tee(&b""[..], &mut Vec::new()), b"");
}

/// A stream that gives one line and then fails.
struct Broken(bool);

impl std::io::Read for Broken {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if std::mem::replace(&mut self.0, true) {
            return Err(std::io::Error::other("the pipe broke"));
        }
        let line = b"[t] Waiting\n";
        buf[..line.len()].copy_from_slice(line);
        Ok(line.len())
    }
}

#[test]
fn a_read_that_fails_ends_the_log_with_what_came_before() {
    // Catches: a failed read of buck2's stderr that loses the lines already read, or that loops.
    let mut log = Vec::new();
    assert_eq!(tee(Broken(false), &mut log), b"[t] Waiting\n");
    assert_eq!(log, b"[t] Waiting\n");
}
