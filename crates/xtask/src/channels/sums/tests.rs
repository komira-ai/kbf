use super::*;
use kbf_testkit::dir::TestDir;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-sums-{name}"))
}

#[test]
fn a_sums_file_is_read_as_one_digest_and_name_per_line() {
    // Catches: a name or digest taken from the wrong side of the two spaces, and a line that
    // drops out of the result.
    let lines = parse(&format!("{A}  kbf_1.2.3_amd64.deb\n{B}  SHA256SUMS.txt\n")).expect("ok");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].sha256.to_string(), A);
    assert_eq!(lines[0].name, "kbf_1.2.3_amd64.deb");
    assert_eq!(lines[1].sha256.to_string(), B);
    assert_eq!(lines[1].name, "SHA256SUMS.txt");
    assert_eq!(parse("").expect("empty"), []);
}

#[test]
fn every_line_off_the_form_is_a_problem_of_its_own() {
    // Catches: a malformed line skipped, so that a digest is trusted from a file that is not a
    // sums file; and a line with no newline (a cut file) accepted.
    let text = format!(
        "{A}  ok\nnot a line\n{A} one-space\n{}  short\n{A}  dir/name\n{A}  \n{A}  no-newline",
        &A[1..]
    );
    let problems = parse(&text).expect_err("problems");
    assert_eq!(
        problems,
        [
            "line 2: \"not a line\" is not `<sha256>  <file name>` and a newline",
            format!("line 3: \"{A} one-space\" is not `<sha256>  <file name>` and a newline")
                .as_str(),
            format!(
                "line 4: \"{}  short\" is not `<sha256>  <file name>` and a newline",
                &A[1..]
            )
            .as_str(),
            format!("line 5: \"{A}  dir/name\" is not `<sha256>  <file name>` and a newline")
                .as_str(),
            format!("line 6: \"{A}  \" is not `<sha256>  <file name>` and a newline").as_str(),
            format!("line 7: \"{A}  no-newline\" is not `<sha256>  <file name>` and a newline")
                .as_str(),
        ]
    );
}

#[test]
fn a_sums_file_is_read_from_a_path_and_a_missing_malformed_or_empty_one_is_refused() {
    // Catches: a release checked against no digests at all, because its sums file was empty,
    // missing or unreadable.
    let dir = scratch("read");
    let good = dir.join("good");
    std::fs::write(&good, format!("{A}  one\n")).expect("write");
    assert_eq!(read_sums(&good).expect("read")[0].name, "one");

    let missing = read_sums(&dir.join("missing")).unwrap_err();
    assert!(
        matches!(&missing, Error::Spawn { what, .. } if what.starts_with("read ") && what.ends_with("missing")),
        "{missing}"
    );

    let bad = dir.join("bad");
    std::fs::write(&bad, "junk\n").expect("write");
    let Error::Refused(problems) = read_sums(&bad).unwrap_err() else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        [format!(
            "{}: line 1: \"junk\" is not `<sha256>  <file name>` and a newline",
            bad.display()
        )]
    );

    let empty = dir.join("empty");
    std::fs::write(&empty, "").expect("write");
    assert_eq!(
        read_sums(&empty),
        Err(Error::Failed(format!("{} lists no file", empty.display())))
    );
}
