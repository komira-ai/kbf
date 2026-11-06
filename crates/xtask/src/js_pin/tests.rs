use super::*;

/// A pin with every key, one row for `js-lease-server` on x86_64.
const GOOD: &str = r#"
[release]
repository = "komira-ai/jobscheduler"
version = "0.1.179"
commit = "9cf5b5a987e77aa39cac7c15a5dd4e635af4742c"
signer_workflow = "komira-ai/jobscheduler/.github/workflows/release.yml"
source_ref = "refs/heads/main"

[[archive]]
program = "js-lease-server"
target = "x86_64-unknown-linux-musl"
sha256 = "cd8759affe0f109b797695490d5a9e644f268659c7e70a8a422868ac1f57d4d5"
attestation_log_index = 3722910151
"#;

/// `GOOD` with `from` replaced by `to` (which must change it).
fn changed(from: &str, to: &str) -> String {
    assert!(GOOD.contains(from), "{from:?} is not in the pin");
    GOOD.replacen(from, to, 1)
}

/// The message of the refusal of `text`.
fn refusal(text: &str) -> String {
    Pin::parse(text).expect_err("refused").to_string()
}

/// Asserts that the pin the repository holds parses, names the server and the agent on x86_64,
/// and that the server's row gives the URL, archive name, member, files and flags the release
/// has. Catches a checked-in pin that the verb would refuse, and a URL or member built from the
/// wrong fields.
#[test]
fn the_checked_in_pin_names_the_server_and_the_agent() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kbf-js-version.toml");
    let pin = Pin::read(&path).expect("the checked-in pin");
    assert_eq!(
        pin,
        Pin::parse(&std::fs::read_to_string(&path).unwrap()).unwrap()
    );
    assert_eq!(pin.repository, "komira-ai/jobscheduler");
    let server = pin
        .archive("js-lease-server", "x86_64-unknown-linux-musl")
        .unwrap();
    pin.archive("js-agent", "x86_64-unknown-linux-musl")
        .unwrap();
    let url = pin.url(server);
    assert_eq!(
        url,
        format!(
            "https://github.com/komira-ai/jobscheduler/releases/download/v{0}/js-lease-server-{0}-x86_64-unknown-linux-musl.tar.gz",
            pin.version
        )
    );
    let request = pin.request(server, &url, Path::new("/d"));
    assert_eq!(request.archive, Path::new("/d").join(pin.file_name(server)));
    assert_eq!(request.out, Path::new("/d/js-lease-server"));
    assert_eq!(
        request.member,
        Path::new(&format!(
            "js-lease-server-{}-x86_64-unknown-linux-musl",
            pin.version
        ))
        .join("js-lease-server")
    );
    assert_eq!(request.sha256, server.sha256);
    assert_eq!(
        (
            request.repo,
            request.signer_workflow,
            request.source_ref,
            request.source_digest
        ),
        (
            pin.repository.as_str(),
            pin.signer_workflow.as_str(),
            "refs/heads/main",
            pin.commit.as_str()
        )
    );
}

/// Asserts the values of a good pin, field by field.
#[test]
fn a_good_pin_reads_every_value() {
    let pin = Pin::parse(GOOD).unwrap();
    assert_eq!(
        pin,
        Pin {
            repository: "komira-ai/jobscheduler".to_owned(),
            version: "0.1.179".to_owned(),
            commit: "9cf5b5a987e77aa39cac7c15a5dd4e635af4742c".to_owned(),
            signer_workflow: "komira-ai/jobscheduler/.github/workflows/release.yml".to_owned(),
            source_ref: "refs/heads/main".to_owned(),
            archives: vec![Archive {
                program: "js-lease-server".to_owned(),
                target: "x86_64-unknown-linux-musl".to_owned(),
                sha256: "cd8759affe0f109b797695490d5a9e644f268659c7e70a8a422868ac1f57d4d5"
                    .parse()
                    .unwrap(),
                attestation_log_index: 3_722_910_151,
            }],
        }
    );
}

/// Asserts that a program or target with no row is a refusal naming both, so nothing unpinned
/// is fetched. Catches a lookup that falls back to another row or matches on the program alone.
#[test]
fn a_program_or_target_without_a_row_is_refused() {
    let pin = Pin::parse(GOOD).unwrap();
    for (program, target) in [
        ("js-agent", "x86_64-unknown-linux-musl"),
        ("js-lease-server", "aarch64-unknown-linux-musl"),
    ] {
        let error = pin.archive(program, target).unwrap_err().to_string();
        assert_eq!(
            error,
            format!("kbf-js-version.toml: no [[archive]] row for {program} on {target}")
        );
    }
}

/// Asserts that a pin file that cannot be read is an error naming its path.
#[test]
fn a_missing_pin_file_is_refused_naming_it() {
    let error = Pin::read(Path::new("/nonexistent/kbf-js-version.toml"))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("read the pin /nonexistent/kbf-js-version.toml: "),
        "{error}"
    );
}

/// Asserts each refusal of the release table, with its exact message. Catches a check that
/// lets a value of the wrong shape through to a URL or to `gh`'s flags.
#[test]
fn each_bad_release_value_is_refused_by_name() {
    let cases: [(String, &str); 16] = [
        ("not = [toml".to_owned(), "kbf-js-version.toml: not TOML: "),
        (
            format!("{GOOD}\n[extra]\n"),
            "kbf-js-version.toml: `the file` has the unknown key `extra`",
        ),
        (
            GOOD.replacen("[release]", "[released]", 1),
            "kbf-js-version.toml: `the file` has the unknown key `released`",
        ),
        (
            changed("[[archive]]", "[[archives]]"),
            "kbf-js-version.toml: `the file` has the unknown key `archives`",
        ),
        (
            changed("source_ref = ", "extra = 1\nsource_ref = "),
            "kbf-js-version.toml: `[release]` has the unknown key `extra`",
        ),
        (
            changed("repository = \"komira-ai/jobscheduler\"\n", ""),
            "kbf-js-version.toml: `[release]` has no string `repository`",
        ),
        (
            changed("version = \"0.1.179\"", "version = 179"),
            "kbf-js-version.toml: `[release]` has no string `version`",
        ),
        (
            changed("\"komira-ai/jobscheduler\"", "\"komira-ai/jobscheduler/x\""),
            "kbf-js-version.toml: repository \"komira-ai/jobscheduler/x\" is not <owner>/<repo>",
        ),
        (
            changed("\"komira-ai/jobscheduler\"", "\"komira-ai/\""),
            "kbf-js-version.toml: repository \"komira-ai/\" is not <owner>/<repo>",
        ),
        (
            changed("\"0.1.179\"", "\"v0.1.179\""),
            "kbf-js-version.toml: version \"v0.1.179\" is not a plain version (no `v`)",
        ),
        (
            changed("\"0.1.179\"", "\"0.1/179\""),
            "kbf-js-version.toml: version \"0.1/179\" is not a plain version (no `v`)",
        ),
        (
            changed("9cf5b5a987e7", "9CF5B5A987E7"),
            "kbf-js-version.toml: commit \"9CF5B5A987E77aa39cac7c15a5dd4e635af4742c\" is not 40 lowercase hex digits",
        ),
        (
            changed("4742c\"", "4742\""),
            "kbf-js-version.toml: commit \"9cf5b5a987e77aa39cac7c15a5dd4e635af4742\" is not 40 lowercase hex digits",
        ),
        (
            changed(
                "komira-ai/jobscheduler/.github/workflows/release.yml",
                "komira-ai/other/.github/workflows/release.yml",
            ),
            "kbf-js-version.toml: signer_workflow \"komira-ai/other/.github/workflows/release.yml\" is not a file of komira-ai/jobscheduler/.github/workflows/",
        ),
        (
            changed("\"refs/heads/main\"", "\"main\""),
            "kbf-js-version.toml: source_ref \"main\" does not name a ref (refs/...)",
        ),
        (
            changed("\"refs/heads/main\"", "\"refs/\""),
            "kbf-js-version.toml: source_ref \"refs/\" does not name a ref (refs/...)",
        ),
    ];
    for (text, message) in cases {
        let error = refusal(&text);
        assert!(error.starts_with(message), "{message}: {error}");
    }
    assert_eq!(
        refusal("archive = []\n"),
        "kbf-js-version.toml: no `[release]` table"
    );
    assert_eq!(
        refusal("release = 1\n"),
        "kbf-js-version.toml: no `[release]` table"
    );
}

/// Asserts each refusal of an archive row, with its exact message, the repeated row included.
/// Catches a digest of the wrong shape passed on, a log index that wraps, and two rows of which
/// the lookup would take the first.
#[test]
fn each_bad_archive_row_is_refused_by_name() {
    let head = GOOD.split("[[archive]]").next().unwrap();
    let row = &GOOD[head.len()..];
    let cases: [(String, &str); 12] = [
        (head.to_owned(), "kbf-js-version.toml: no `[[archive]]` row"),
        (
            format!("archive = []\n{head}"),
            "kbf-js-version.toml: no `[[archive]]` row",
        ),
        (
            format!("archive = [1]\n{head}"),
            "kbf-js-version.toml: [[archive]] 1 is not a table",
        ),
        (
            changed(
                "attestation_log_index = ",
                "size = 1\nattestation_log_index = ",
            ),
            "kbf-js-version.toml: `[[archive]] 1` has the unknown key `size`",
        ),
        (
            changed("program = \"js-lease-server\"\n", ""),
            "kbf-js-version.toml: `[[archive]] 1` has no string `program`",
        ),
        (
            changed("program = \"js-lease-server\"", "program = \"..\""),
            "kbf-js-version.toml: [[archive]] 1: program \"..\" is not a plain name",
        ),
        (
            changed("target = \"x86_64-unknown-linux-musl\"", "target = \"x/y\""),
            "kbf-js-version.toml: [[archive]] 1: target \"x/y\" is not a plain name",
        ),
        (
            changed("\"cd8759", "\"Cd8759"),
            "kbf-js-version.toml: [[archive]] 1: sha256: not a SHA-256: ",
        ),
        (
            changed("4d5\"", "4d\""),
            "kbf-js-version.toml: [[archive]] 1: sha256: not a SHA-256: ",
        ),
        (
            changed("= 3722910151", "= -1"),
            "kbf-js-version.toml: [[archive]] 1 has no attestation_log_index of zero or more",
        ),
        (
            changed("= 3722910151", "= \"3722910151\""),
            "kbf-js-version.toml: [[archive]] 1 has no attestation_log_index of zero or more",
        ),
        (
            format!("{GOOD}{row}"),
            "kbf-js-version.toml: [[archive]] 2 repeats js-lease-server for x86_64-unknown-linux-musl",
        ),
    ];
    for (text, message) in cases {
        let error = refusal(&text);
        assert!(error.starts_with(message), "{message}: {error}");
    }
    let other_target = row.replace("x86_64", "aarch64");
    assert_eq!(
        Pin::parse(&format!("{GOOD}{other_target}"))
            .unwrap()
            .archives
            .len(),
        2
    );
}

/// Asserts the plain-name rule at its edges.
#[test]
fn a_plain_name_is_letters_digits_dot_underscore_dash() {
    for good in ["js-agent", "a.b_c-1", "..."] {
        assert!(plain(good), "{good}");
    }
    for bad in ["", ".", "..", "a/b", "a b", "a\u{e9}"] {
        assert!(!plain(bad), "{bad:?}");
    }
}

/// The JSON `gh attestation verify --format json` prints, one attestation per index (each
/// a string, as Sigstore writes it).
fn verified(indexes: &[&str]) -> Vec<u8> {
    let all: Vec<serde_json::Value> = indexes
        .iter()
        .map(|index| {
            serde_json::json!({"attestation": {"bundle": {"verificationMaterial": {
                "tlogEntries": [{"logIndex": index}]}}}})
        })
        .collect();
    serde_json::to_vec(&all).unwrap()
}

/// Asserts that the pinned log entry is found among the verified attestations, as a string or
/// a number, and that a different entry, no entry, and output that is not the list `gh` prints
/// are each not a match. Catches a check that takes any verified attestation, or that matches a
/// prefix of the index.
#[test]
fn logged_finds_the_pinned_entry_and_nothing_else() {
    assert!(logged(&verified(&["1", "3722910151"]), 3_722_910_151));
    assert!(logged(
        br#"[{"attestation":{"bundle":{"verificationMaterial":{"tlogEntries":[{"logIndex":7}]}}}}]"#,
        7
    ));
    assert!(!logged(&verified(&["372291015"]), 3_722_910_151));
    assert!(!logged(&verified(&["37229101510"]), 3_722_910_151));
    assert!(!logged(&verified(&[]), 1));
    for other in [
        &b"not json"[..],
        b"{}",
        br#"[{"attestation":{}}]"#,
        br#"[{"attestation":{"bundle":{"verificationMaterial":{"tlogEntries":{}}}}}]"#,
        br#"[{"attestation":{"bundle":{"verificationMaterial":{"tlogEntries":[{"logIndex":true}]}}}}]"#,
        br#"[{"attestation":{"bundle":{"verificationMaterial":{"tlogEntries":[{}]}}}}]"#,
    ] {
        assert!(!logged(other, 1), "{}", String::from_utf8_lossy(other));
    }
}

/// Asserts the verdict on `gh`'s answer: a failed verification passes `gh`'s reason on; a
/// success that lists the pinned entry is accepted; a success that does not is refused with a
/// message naming the entry. Catches a verifier that trusts `gh`'s exit status alone.
#[test]
fn the_verdict_needs_gh_and_the_pinned_entry() {
    let gh = PinnedGh {
        gh: Path::new("gh"),
        log_index: 42,
    };
    assert_eq!(
        gh.judge(false, &verified(&["42"]), "no attestation"),
        (false, "no attestation".to_owned())
    );
    assert_eq!(
        gh.judge(true, &verified(&["42"]), ""),
        (true, String::new())
    );
    assert_eq!(
        gh.judge(true, &verified(&["41"]), ""),
        (
            false,
            "none of the attestations gh verified is the pinned transparency log entry 42"
                .to_owned()
        )
    );
}

/// Asserts that a `gh` that cannot be started is an error naming it, not a refusal of the file.
#[test]
fn a_gh_that_cannot_start_is_an_error_naming_it() {
    let gh = PinnedGh {
        gh: Path::new("/nonexistent/gh"),
        log_index: 1,
    };
    let error = gh
        .verify(Path::new("/nonexistent/archive"), &[])
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("/nonexistent/gh: "), "{error}");
}

/// Asserts that the verb reads the pin before anything else: a missing pin, and a program the
/// pin has no row for, are refused with nothing written to `--dir`.
#[test]
fn the_verb_refuses_before_any_download() {
    let dir = Path::new("/nonexistent/js-dir");
    let pin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kbf-js-version.toml");
    let args = |pin: &Path, program: &str| Args {
        pin: pin.to_path_buf(),
        program: program.to_owned(),
        target: "x86_64-unknown-linux-musl".to_owned(),
        gh: PathBuf::from("gh"),
        dir: dir.to_path_buf(),
    };
    let error = fetch_pinned_js(&args(&pin, "js-other"))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "kbf-js-version.toml: no [[archive]] row for js-other on x86_64-unknown-linux-musl"
    );
    let missing = Path::new("/nonexistent/pin.toml");
    let error = fetch_pinned_js(&args(missing, "js-agent"))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("read the pin /nonexistent/pin.toml: "),
        "{error}"
    );
    assert!(!dir.exists());
}
