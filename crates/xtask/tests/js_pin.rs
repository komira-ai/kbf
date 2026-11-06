//! `fetch-pinned-js` without the network: its verifier run against `xtask-fake` as `gh` (whose
//! file `attested` is the store of attestations), and the verb's flags through the binary.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use xtask::fetch::Sha256;
use xtask::fetch_attested::{Verifier, verify_args};
use xtask::js_pin::{Pin, PinnedGh, install};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-js-pin"),
        name,
    )
}

/// The pin the repository holds.
fn pin_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kbf-js-version.toml")
}

/// Asserts what the verifier asks of `gh` and how it reads the answer: it runs `gh attestation
/// verify <file>` with the flags of the pinned row and `--format json`; when `gh` refuses the
/// file, `gh`'s reason is the refusal; when `gh` accepts it but prints no attestation that is
/// the pinned log entry (the fake prints none), the file is refused naming the entry. Catches a
/// verifier that drops `--format json` (it would read nothing to check) and one that takes
/// `gh`'s exit status alone.
#[test]
fn the_verifier_needs_gh_to_print_the_pinned_entry() {
    let own = scratch("verifier");
    let gh = own.join("gh");
    std::fs::hard_link(FAKE, &gh).expect("link the fake as gh");
    let archive = own.join("js-lease-server.tar.gz");
    std::fs::write(&archive, b"an archive").expect("archive");

    let pin = Pin::read(&pin_path()).expect("the pin");
    let row = pin
        .archive("js-lease-server", "x86_64-unknown-linux-musl")
        .expect("the server's row");
    let url = pin.url(row);
    let args = verify_args(&pin.request(row, &url, &own));
    let verifier = PinnedGh {
        gh: &gh,
        log_index: row.attestation_log_index,
    };

    let (accepted, said) = verifier.verify(&archive, &args).expect("gh runs");
    assert!(!accepted);
    assert!(said.starts_with("xtask-fake: no attestation of "), "{said}");

    let flags = format!("{} --format json", args.join(" "));
    let digest = Sha256::of(b"an archive");
    std::fs::write(own.join("attested"), format!("{digest} {flags}\n")).expect("attested");
    let (accepted, said) = verifier.verify(&archive, &args).expect("gh runs");
    assert!(!accepted);
    assert_eq!(
        said,
        format!(
            "none of the attestations gh verified is the pinned transparency log entry {}",
            row.attestation_log_index
        )
    );
    let asked = std::fs::read_to_string(own.join("gh-args")).expect("gh-args");
    let last = asked.lines().last().expect("a call");
    assert_eq!(
        last,
        format!("attestation verify {} {flags}", archive.display())
    );
}

/// Asserts the verb's flags reach it and that it refuses before any download: a program the pin
/// has no row for exits 1 naming the program and target, and a pin that is not there exits 1
/// naming the file; `--dir` is not made either time.
#[test]
fn the_verb_exits_1_for_a_program_or_pin_it_does_not_have() {
    let own = scratch("cli");
    let dir = own.join("js");
    let run = |pin: &Path, program: &str| {
        Command::new(XTASK)
            .args(["fetch-pinned-js", "--pin"])
            .arg(pin)
            .args([
                "--program",
                program,
                "--target",
                "x86_64-unknown-linux-musl",
            ])
            .args(["--gh", "/nonexistent/gh", "--dir"])
            .arg(&dir)
            .output()
            .expect("run xtask")
    };
    let out = run(&pin_path(), "js-other");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert_eq!(
        err,
        "xtask: kbf-js-version.toml: no [[archive]] row for js-other on x86_64-unknown-linux-musl\n"
    );
    let out = run(&own.join("missing.toml"), "js-agent");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.starts_with(&format!(
            "xtask: read the pin {}: ",
            own.join("missing.toml").display()
        )),
        "{err}"
    );
    assert!(!dir.exists());
}

/// A gzip tar holding the regular file `member` with `program` in it.
fn tarball(member: &str, program: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut header = tar::Header::new_ustar();
    header.set_path(member).expect("path");
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(program.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, program).expect("append");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&builder.into_inner().expect("tar"))
        .expect("gzip");
    gz.finish().expect("gzip")
}

/// Asserts the install the verb makes of downloaded bytes: bytes whose SHA-256 is pinned, whose
/// file `gh` verifies with the pinned row's flags and `--format json` and lists the pinned log
/// entry, are installed as `<dir>/<program>` from the member `<program>-<version>-<target>/
/// <program>`; the same bytes with the attestation listed under another entry are refused with
/// no program; bytes with one changed bit are refused before anything is written. Catches an
/// install that takes the program from bytes it did not check, and one that trusts `gh`'s exit
/// status without the pinned entry.
#[test]
fn the_install_takes_the_program_only_from_pinned_attested_bytes() {
    let own = scratch("install");
    let gh = own.join("gh");
    std::fs::hard_link(FAKE, &gh).expect("link the fake as gh");
    let mut pin = Pin::read(&pin_path()).expect("the pin");
    let row = pin
        .archive("js-lease-server", "x86_64-unknown-linux-musl")
        .expect("the server's row")
        .clone();
    let bytes = tarball(
        pin.member(&row).to_str().expect("utf-8"),
        b"the server program",
    );
    for archive in &mut pin.archives {
        archive.sha256 = Sha256::of(&bytes);
    }
    let row = pin.archives[0].clone();
    let dir = own.join("js");
    let url = pin.url(&row);
    let flags = format!(
        "{} --format json",
        verify_args(&pin.request(&row, &url, &dir)).join(" ")
    );
    let attested = format!("{} {flags}\n", Sha256::of(&bytes));
    std::fs::write(own.join("attested"), attested).expect("attested");
    let listing = |index: u64| {
        format!(
            r#"[{{"attestation":{{"bundle":{{"verificationMaterial":{{"tlogEntries":[{{"logIndex":"{index}"}}]}}}}}}}}]"#
        )
    };

    std::fs::write(
        own.join("gh-stdout"),
        listing(row.attestation_log_index + 1),
    )
    .expect("out");
    let error = install(&pin, &row, &bytes, &gh, &dir)
        .unwrap_err()
        .to_string();
    assert!(
        error.ends_with(&format!(
            "pinned transparency log entry {}",
            row.attestation_log_index
        )),
        "{error}"
    );
    assert!(!dir.join(&row.program).exists());
    std::fs::remove_file(dir.join(pin.file_name(&row))).expect("the checked archive");

    std::fs::write(own.join("gh-stdout"), listing(row.attestation_log_index)).expect("out");
    let line = install(&pin, &row, &bytes, &gh, &dir).expect("installed");
    assert!(
        line.starts_with(&format!("ok: {url} has SHA-256 ")),
        "{line}"
    );
    let program = dir.join(&row.program);
    assert_eq!(
        std::fs::read(&program).expect("program"),
        b"the server program"
    );

    let mut changed = bytes.clone();
    changed[0] ^= 1;
    let other = own.join("other");
    let error = install(&pin, &row, &changed, &gh, &other)
        .unwrap_err()
        .to_string();
    assert!(error.contains(", not the pinned "), "{error}");
    assert!(!other.exists());
}
