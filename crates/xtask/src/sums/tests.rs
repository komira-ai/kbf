use std::path::Path;

use super::*;
use kbf_testkit::dir::TestDir;

const A: &str = "kbf-daemon-c0ffee012345-linux-x86_64.tar.gz";
const A_SBOM: &str = "kbf-daemon-c0ffee012345-linux-x86_64.cdx.json";
const B: &str = "kbf-daemon-c0ffee012345-darwin-arm64.tar.gz";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-sums-unit"), name)
}

/// One row of the pairing test: what is listed, then the problem wanted (none when it passes).
type PairingRow<'a> = (&'a str, &'a [&'a str], Option<&'a str>);

fn line(name: &str, bytes: &[u8]) -> String {
    format!("{}  {name}\n", Sha256::of(bytes))
}

/// `dir` holding `files` and a SHA256SUMS of `sums` lines.
fn assets(dir: &Path, files: &[(&str, &[u8])], sums: &str) {
    for (name, bytes) in files {
        std::fs::write(dir.join(name), bytes).expect("file");
    }
    std::fs::write(dir.join(SUMS), sums).expect("sums");
}

fn refused(r: Result<String, Error>) -> Vec<String> {
    match r {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn a_sums_file_is_sha256sum_lines_of_bare_file_names() {
    // Catches: a parser that accepts a line `sha256sum --check --strict` refuses (one
    // space, a short or upper-case digest, no final newline, an empty line), or a name
    // with a `/` (a line that checks a file outside the directory); and a render that
    // does not round-trip.
    let hex = Sha256::of(b"a").to_string();
    let good = format!("{hex}  {A}\n{hex}  {A_SBOM}\n");
    let lines = parse(&good).expect("parse");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].name, A_SBOM);
    assert_eq!(render(&lines), good);
    assert_eq!(parse(""), Ok(vec![]));
    let rows = [
        format!("{hex} {A}\n"),
        format!("{}  {A}\n", &hex[..63]),
        format!("{}  {A}\n", hex.to_uppercase()),
        format!("{hex}  {A}"),
        format!("{hex}  \n"),
        format!("{hex}  ../{A}\n"),
        "\n".to_owned(),
    ];
    for bad in rows {
        let text = format!("{good}{bad}");
        let first = bad.trim_end_matches('\n');
        assert_eq!(
            parse(&text),
            Err(vec![format!(
                "line 3: {first:?} is not `<sha256>  <file name>` and a newline"
            )]),
            "{bad:?}"
        );
    }
}

#[test]
fn duplicates_lists_each_repeated_name_once() {
    // Catches: a repeated line taken as one, or reported once per copy.
    let l = |name: &str| Line {
        sha256: Sha256::of(name.as_bytes()),
        name: name.to_owned(),
    };
    assert_eq!(duplicates(&[l(A), l(B), l(A), l(A), l(B)]), [B, A]);
    assert!(duplicates(&[l(A), l(B)]).is_empty());
}

#[test]
fn verify_sums_passes_when_listed_is_present_and_each_matches() {
    // Control: catches a verify that refuses a correct directory, or counts a
    // subdirectory as a file.
    let dir = scratch("verify-good");
    let sums = format!("{}{}", line(A, b"tarball"), line(A_SBOM, b"sbom"));
    assets(&dir, &[(A, b"tarball"), (A_SBOM, b"sbom")], &sums);
    std::fs::create_dir(dir.join("subdirectory")).expect("dir");
    assert_eq!(
        verify_sums(&dir, Sha256::of),
        Ok("sums: 2 files match; the flipped copy does not".to_owned())
    );
}

#[test]
fn a_kbf_tarball_and_its_version_facts_are_listed_together() {
    // Catches: the pairing in one direction only. Rows: a `kbf` tarball without facts is
    // refused; facts without their tarball are refused; both listed pass. Catches: either `if`
    // dropped.
    let tarball = "kbf-c0ffee012345-linux-x86_64.tar.gz";
    let facts = "kbf-c0ffee012345-linux-x86_64.version.json";
    let no_facts = format!("{tarball}: its version facts {facts} are not listed in {SUMS}");
    let no_tarball =
        format!("{facts}: the tarball it states the facts of, {tarball}, is not listed in {SUMS}");
    let (no_facts, no_tarball) = (no_facts.as_str(), no_tarball.as_str());
    let rows: [PairingRow; 3] = [
        ("tarball", &[tarball], Some(no_facts)),
        ("facts", &[facts], Some(no_tarball)),
        ("both", &[tarball, facts], None),
    ];
    let bytes: &[u8] = b"bytes";
    for (what, listed, wanted) in rows {
        let dir = scratch(&format!("verify-pairing-{what}"));
        let files: Vec<(&str, &[u8])> = listed.iter().map(|n| (*n, bytes)).collect();
        let sums: String = listed.iter().map(|n| line(n, bytes)).collect();
        assets(&dir, &files, &sums);
        let got = verify_sums(&dir, Sha256::of);
        match wanted {
            Some(problem) => assert_eq!(refused(got), [problem], "{what}"),
            None => assert!(got.is_ok(), "{what}: {got:?}"),
        }
    }
}

#[test]
fn a_task_image_and_its_sbom_are_listed_together() {
    // Catches: the pairing in one direction only, and the SBOM of the `kbf-task` binary (a tarball's, named
    // `kbf-task-<commit>-<platform>.cdx.json`) or an image that has no SBOM (the frontend's)
    // taken for the pair. Rows: a task image with its facts and no SBOM is refused; one with its SBOM
    // and no facts is refused; an SBOM without its image is refused; the set with both passes.
    // Catches: either `if` dropped. (A tarball is listed in every row: the
    // control that flips its last byte needs one.)
    let image = "kbf-task-image-c0ffee012345-linux-x86_64.oci.tar";
    let facts = "kbf-task-image-c0ffee012345-linux-x86_64.facts.json";
    let sbom = "kbf-task-image-c0ffee012345-linux-x86_64.cdx.json";
    let binary_sbom = "kbf-task-c0ffee012345-linux-x86_64.cdx.json";
    let frontend = "kbf-frontend-image-c0ffee012345-linux-x86_64.oci.tar";
    let no_sbom = format!("{image}: its image SBOM {sbom} is not listed in {SUMS}");
    let no_image = format!("{sbom}: the image it describes, {image}, is not listed in {SUMS}");
    let (no_sbom, no_image) = (no_sbom.as_str(), no_image.as_str());
    let no_facts = format!("{image}: its image facts {facts} are not listed in {SUMS}");
    let no_facts = no_facts.as_str();
    let rows: [PairingRow; 6] = [
        ("image", &[A, image, facts], Some(no_sbom)),
        ("image-no-facts", &[A, image, sbom], Some(no_facts)),
        ("sbom", &[A, sbom], Some(no_image)),
        ("both", &[A, image, facts, sbom], None),
        ("binary-sbom", &[A, binary_sbom], None),
        ("frontend", &[A, frontend], None),
    ];
    let bytes: &[u8] = b"bytes";
    for (what, listed, wanted) in rows {
        let dir = scratch(&format!("verify-image-sbom-{what}"));
        let files: Vec<(&str, &[u8])> = listed.iter().map(|n| (*n, bytes)).collect();
        let sums: String = listed.iter().map(|n| line(n, bytes)).collect();
        assets(&dir, &files, &sums);
        let got = verify_sums(&dir, Sha256::of);
        match wanted {
            Some(problem) => assert_eq!(refused(got), [problem], "{what}"),
            None => assert!(got.is_ok(), "{what}: {got:?}"),
        }
    }
}

#[test]
fn verify_sums_refuses_listed_not_equal_to_present() {
    // Catches: a file in the directory that SHA256SUMS does not list (it would be
    // shipped unsigned; a leftover SHA256SUMS.<platform> part included), a listed file that is missing, a line listed twice, and a
    // file whose bytes are not its line's. Each row plants one.
    struct Row {
        what: &'static str,
        files: &'static [(&'static str, &'static [u8])],
        sums: String,
        want: fn(&Path) -> Vec<String>,
    }
    let rows = [
        Row {
            what: "unlisted",
            files: &[(A, b"tarball"), (A_SBOM, b"sbom")],
            sums: line(A, b"tarball"),
            want: |_| vec![format!("not listed in {SUMS}: {A_SBOM}")],
        },
        Row {
            // A part file left over from assembly: named like SHA256SUMS but not it.
            what: "unlisted-part",
            files: &[(A, b"tarball"), ("SHA256SUMS.linux-arm64", b"part")],
            sums: line(A, b"tarball"),
            want: |_| vec![format!("not listed in {SUMS}: SHA256SUMS.linux-arm64")],
        },
        Row {
            what: "missing",
            files: &[(A, b"tarball")],
            sums: format!("{}{}", line(A, b"tarball"), line(A_SBOM, b"sbom")),
            want: |dir| vec![format!("listed, but not in {}: {A_SBOM}", dir.display())],
        },
        Row {
            what: "twice",
            files: &[(A, b"tarball")],
            sums: format!("{}{}", line(A, b"tarball"), line(A, b"tarball")),
            want: |_| vec![format!("listed twice: {A}")],
        },
        Row {
            what: "mismatch",
            files: &[(A, b"tarball"), (A_SBOM, b"SBOM")],
            sums: format!("{}{}", line(A, b"tarball"), line(A_SBOM, b"sbom")),
            want: |_| vec![format!("{A_SBOM}: does not match its line")],
        },
    ];
    for r in rows {
        let dir = scratch(&format!("verify-{}", r.what));
        assets(&dir, r.files, &r.sums);
        assert_eq!(
            refused(verify_sums(&dir, Sha256::of)),
            (r.want)(&dir),
            "{}",
            r.what
        );
    }
}

#[test]
fn a_digest_that_accepts_anything_is_caught_by_the_flipped_byte_control() {
    // Catches: the byte-flip control skipped. With a digest that gives every input the
    // same value, every line "matches"; only the control (a flipped copy of the first
    // tarball must not match) stops the run.
    fn constant(_: &[u8]) -> Sha256 {
        Sha256([7; 32])
    }
    let dir = scratch("verify-constant");
    let sums = format!("{}  {A}\n", Sha256([7; 32]));
    assets(&dir, &[(A, b"tarball")], &sums);
    assert_eq!(
        verify_sums(&dir, constant),
        Err(Error::Failed(format!(
            "control failed: a flipped byte in {A} still matched {SUMS}"
        )))
    );
}

#[test]
fn verify_sums_needs_a_sums_file_and_a_nonempty_tarball() {
    // Catches: a run with no SHA256SUMS, an empty one, one with no tarball (the
    // control would have nothing to flip) or an empty first tarball passing.
    let dir = scratch("verify-none");
    assert!(
        verify_sums(&dir, Sha256::of).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("read {}: ", dir.join(SUMS).display()))),
        "no sums file"
    );
    assets(&dir, &[], "");
    assert_eq!(
        verify_sums(&dir, Sha256::of),
        Err(Error::Failed(format!(
            "{} lists no file",
            dir.join(SUMS).display()
        )))
    );
    assets(&dir, &[(A_SBOM, b"sbom")], &line(A_SBOM, b"sbom"));
    assert_eq!(
        verify_sums(&dir, Sha256::of),
        Err(Error::Failed(format!("{SUMS} lists no tarball")))
    );
    let dir = scratch("verify-empty-tarball");
    assets(&dir, &[(A, b"")], &line(A, b""));
    assert_eq!(
        verify_sums(&dir, Sha256::of),
        Err(Error::Failed(format!("{A} is empty")))
    );
    let dir = scratch("verify-malformed");
    assets(&dir, &[], "x\n");
    assert_eq!(
        refused(verify_sums(&dir, Sha256::of)),
        [format!(
            "{}: line 1: \"x\" is not `<sha256>  <file name>` and a newline",
            dir.join(SUMS).display()
        )]
    );
}

/// `dir/SHA256SUMS.<platform>` holding `text` for each `(platform, text)`.
fn parts(dir: &Path, parts: &[(&str, &str)]) {
    for (platform, text) in parts {
        std::fs::write(dir.join(format!("{SUMS}.{platform}")), text).expect("part");
    }
}

#[test]
fn assemble_merges_the_platform_files_sorted_by_name_and_removes_them() {
    // Catches: lines lost or kept in file order (the old step sorted by name), a
    // platform file left in the directory (verify-assets would refuse it as
    // unlisted), and a summary not written or overwritten instead of appended.
    let dir = scratch("assemble");
    let (a, sbom, b) = (line(A, b"a"), line(A_SBOM, b"s"), line(B, b"b"));
    parts(
        &dir,
        &[
            ("linux-x86_64", &format!("{a}{sbom}")),
            ("darwin-arm64", &b),
        ],
    );
    std::fs::write(dir.join(A), b"a").expect("asset");
    let summary = dir.join("summary.md");
    std::fs::write(&summary, "# earlier\n").expect("summary");
    let printed = assemble(&dir, None, Some(&summary), None).expect("assemble");
    let want = format!("{b}{sbom}{a}");
    assert_eq!(printed, want.lines().collect::<Vec<_>>());
    assert_eq!(std::fs::read_to_string(dir.join(SUMS)).expect("sums"), want);
    assert_eq!(
        std::fs::read_to_string(&summary).expect("summary"),
        format!("# earlier\n```text\n{want}```\n")
    );
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .expect("ls")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [SUMS, A, "summary.md"]);
}

#[test]
fn assemble_checks_the_result_against_the_published_file() {
    // Catches: --expect ignored, so the attest job could sign sums other than the ones
    // the sums job checked and published; and a matching file refused.
    let dir = scratch("assemble-expect");
    let (a, b) = (line(A, b"a"), line(B, b"b"));
    let published = dir.join("published");
    std::fs::write(&published, format!("{b}{a}")).expect("published");
    parts(&dir, &[("linux-x86_64", &a), ("darwin-arm64", &b)]);
    assert_eq!(
        assemble(&dir, Some(&published), None, None),
        Ok(vec![b.trim_end().to_owned(), a.trim_end().to_owned()])
    );
    let dir = scratch("assemble-expect-differs");
    parts(&dir, &[("linux-x86_64", &a)]);
    assert_eq!(
        assemble(&dir, Some(&published), None, None),
        Err(Error::Failed(format!(
            "the assembled {SUMS} is not {}:\n{a}",
            published.display()
        )))
    );
    assert!(!dir.join(SUMS).exists(), "written although it differs");
}

#[test]
fn assemble_refuses_a_name_twice_an_unknown_platform_and_no_files() {
    // Catches: one asset listed by two platform files (two digests for one name), a
    // stray `SHA256SUMS.<suffix>` merged or skipped, an empty or malformed platform
    // file merged, no platform file at all, an existing SHA256SUMS overwritten, and a
    // summary that cannot be written passing.
    let a = line(A, b"a");
    /// The linux-arm64 part file in `dir`, as messages show it.
    fn arm(dir: &Path) -> String {
        dir.join(format!("{SUMS}.linux-arm64"))
            .display()
            .to_string()
    }
    type Parts<'a> = Vec<(&'a str, &'a str)>;
    type Want = fn(&Path) -> Error;
    let cases: [(&str, Parts, Want); 5] = [
        (
            "twice",
            vec![("linux-x86_64", &a), ("linux-arm64", &a)],
            |_| Error::Refused(vec![format!("listed twice: {A}")]),
        ),
        ("unknown", vec![("linux-x86_64", &a), ("riscv", &a)], |_| {
            Error::Failed(format!("{SUMS}.riscv: \"riscv\" names no platform"))
        }),
        ("none", vec![], |dir| {
            Error::Failed(format!("{} holds no {SUMS}.<platform> file", dir.display()))
        }),
        ("empty", vec![("linux-arm64", "")], |dir| {
            Error::Failed(format!("{} lists no file", arm(dir)))
        }),
        ("malformed", vec![("linux-arm64", "x\n")], |dir| {
            Error::Refused(vec![format!(
                "{}: line 1: \"x\" is not `<sha256>  <file name>` and a newline",
                arm(dir)
            )])
        }),
    ];
    for (what, files, want) in cases {
        let dir = scratch(&format!("assemble-{what}"));
        parts(&dir, &files);
        assert_eq!(assemble(&dir, None, None, None), Err(want(&dir)), "{what}");
    }
    let dir = scratch("assemble-exists");
    parts(&dir, &[("linux-x86_64", &a)]);
    std::fs::write(dir.join(SUMS), "old").expect("old");
    assert!(
        assemble(&dir, None, None, None).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("write {}: ", dir.join(SUMS).display()))),
        "overwrote"
    );
    let dir = scratch("assemble-summary");
    parts(&dir, &[("linux-x86_64", &a)]);
    let summary = dir.join("no-such-dir/summary.md");
    assert!(
        assemble(&dir, None, Some(&summary), None).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("append to {}: ", summary.display()))),
        "summary"
    );
    let _missing_own = scratch("assemble-missing");
    let missing = _missing_own.join("dist");
    assert!(
        assemble(&missing, None, None, None).is_err_and(|e| e
            .to_string()
            .starts_with(&format!("list {}: ", missing.display()))),
        "no directory"
    );
}

#[test]
fn assemble_appends_the_digest_of_the_sums_it_wrote_to_the_output_file() {
    // Catches: a digest of anything but the bytes written to SHA256SUMS (the attest job compares
    // the sums it downloads with this value, so a wrong one signs nothing or signs anything), an
    // output file overwritten instead of appended to (GITHUB_OUTPUT holds other steps' lines), and
    // an output written when none was asked for.
    let dir = scratch("assemble-digest");
    let (a, b) = (line(A, b"a"), line(B, b"b"));
    parts(&dir, &[("linux-x86_64", &a), ("darwin-arm64", &b)]);
    let output = dir.join("output");
    std::fs::write(&output, "earlier=1\n").expect("output");
    assemble(&dir, None, None, Some(&output)).expect("assemble");
    let written = std::fs::read(dir.join(SUMS)).expect("sums");
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        format!("earlier=1\nsha256={}\n", Sha256::of(&written))
    );
    assert_ne!(
        Sha256::of(&written),
        Sha256::of(b"something else"),
        "the digest depends on the sums"
    );
    let dir = scratch("assemble-no-digest");
    parts(&dir, &[("linux-x86_64", &a)]);
    assemble(&dir, None, None, None).expect("assemble");
    assert!(!dir.join("output").exists());
}
