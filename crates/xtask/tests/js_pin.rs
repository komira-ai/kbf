//! `fetch-pinned-js` without the network: its verifier run against `xtask-fake` as `gh` (whose
//! file `attested` is the store of attestations), and the verb's flags through the binary.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use xtask::fetch::Sha256;
use xtask::fetch_attested::{Verifier, verify_args};
use xtask::js_pin::{Pin, PinnedGh};

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
            .args(["--program", program, "--target", "x86_64-unknown-linux-musl"])
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
