//! `kbf-updater enroll` without a frontend: the flags, and every way the command refuses before
//! or while it calls one. An enrollment that succeeds, and the renewals after it, are run
//! against the frontend's real enroll listener in `kbf-server`'s tests.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kbf_testkit::dir::TestDir;
use kbf_types::{CaPin, Token, TokenId, TokenSecret};

use super::enroll::{EnrollFlags, USAGE, parse_flags, run_enroll};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

const ALL: [&str; 8] = [
    "--frontend",
    "frontend.farm.example:443",
    "--node-id",
    "host-1",
    "--token-file",
    "/var/lib/kbf-updater/enroll.token",
    "--state-dir",
    "/var/lib/kbf-updater",
];

/// The four flags make the command's settings, each given once as `--flag value` or
/// `--flag=value`; the node id is the farm's grammar, and the frontend a DNS name and a port.
#[test]
fn the_four_flags_are_read() {
    let flags = parse_flags(&args(&ALL)).unwrap();
    assert_eq!(flags.frontend.host, "frontend.farm.example");
    assert_eq!(flags.frontend.port, 443);
    assert_eq!(flags.node_id.as_str(), "host-1");
    assert_eq!(
        flags.token_file,
        Path::new("/var/lib/kbf-updater/enroll.token")
    );
    assert_eq!(flags.state_dir, Path::new("/var/lib/kbf-updater"));
    let inline: Vec<String> = ALL
        .chunks(2)
        .map(|pair| format!("{}={}", pair[0], pair[1]))
        .collect();
    assert_eq!(parse_flags(&inline).unwrap(), flags);
}

/// Each flag is required, and a frontend that is an address, a node id that is not a name, an
/// unknown flag and a flag with no value are refused with the flag in the message.
#[test]
fn flags_that_are_missing_or_wrong_are_refused_by_name() {
    for position in (0..ALL.len()).step_by(2) {
        let mut words = args(&ALL);
        words.drain(position..position + 2);
        assert_eq!(
            parse_flags(&words).unwrap_err(),
            format!("{} is required", ALL[position])
        );
    }
    let replace = |flag: &str, value: &str| {
        let mut words = args(&ALL);
        let at = words.iter().position(|w| w == flag).unwrap();
        words[at + 1] = value.to_owned();
        parse_flags(&words).unwrap_err()
    };
    assert!(replace("--frontend", "203.0.113.5:443").starts_with("--frontend:"));
    assert!(replace("--node-id", "Host_1").starts_with("--node-id:"));
    assert!(
        parse_flags(&args(&["--nope", "1"]))
            .unwrap_err()
            .contains("unknown argument")
    );
    assert!(
        parse_flags(&args(&["--node-id"]))
            .unwrap_err()
            .contains("needs a value")
    );
}

/// Wrong flags are a usage error (exit 2, the usage in the text) and nothing is created.
#[test]
fn wrong_flags_are_a_usage_error() {
    let (code, text) = run_enroll(&args(&["--frontend", "x"]), 1000, 0);
    assert_eq!(code, 2);
    assert!(text.ends_with(USAGE), "{text}");
}

fn flags_in(dir: &TestDir, port: u16) -> EnrollFlags {
    parse_flags(&args(&[
        "--frontend",
        &format!("localhost:{port}"),
        "--node-id",
        "host-1",
        "--token-file",
        dir.join("enroll.token").to_str().unwrap(),
        "--state-dir",
        dir.join("state").to_str().unwrap(),
    ]))
    .unwrap()
}

fn token() -> Token {
    Token::new(
        TokenId::from_bits(1),
        TokenSecret::from_bytes([1; 32]),
        CaPin::from_bytes([2; 32]),
    )
}

fn write_token(dir: &TestDir, mode: u32) {
    let path = dir.join("enroll.token");
    std::fs::write(&path, token().to_line()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A closed port, for a frontend that is not there.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The command stops, with a reason in a line (exit 1), when the token file is not the user's or
/// is not private, is not there, or when the frontend cannot be reached; and it has made no
/// certificate and no key a host could use, and left the token file where it was.
#[test]
fn an_enrollment_that_cannot_start_or_reach_the_frontend_says_why_and_changes_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "en-refused");
    let euid = crate::secret::euid();
    let port = closed_port();
    let flags = flags_in(&dir, port);

    // No token file.
    let err = super::enroll::enroll_host(&flags, euid, 0).unwrap_err();
    assert!(err.contains("enroll.token"), "{err}");

    // A token file others can read.
    write_token(&dir, 0o644);
    let err = super::enroll::enroll_host(&flags, euid, 0).unwrap_err();
    assert!(err.contains("mode"), "{err}");

    // Another user's.
    write_token(&dir, 0o600);
    let err = super::enroll::enroll_host(&flags, euid + 1, 0).unwrap_err();
    assert!(
        err.contains("owner") || err.contains("owned") || err.contains("uid"),
        "{err}"
    );

    // A frontend that is not there.
    let (code, text) = run_enroll(
        &args(&[
            "--frontend",
            &format!("localhost:{port}"),
            "--node-id",
            "host-1",
            "--token-file",
            dir.join("enroll.token").to_str().unwrap(),
            "--state-dir",
            dir.join("state").to_str().unwrap(),
        ]),
        euid,
        0,
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.starts_with("kbf-updater: "), "{text}");
    assert!(
        dir.join("enroll.token").exists(),
        "the token is kept for a retry"
    );
    assert!(!dir.join("state/pki/updater.pem").exists());
}

/// The state directory must be private and the user's: one anyone can enter is refused before the
/// token is read, so a key is never made where another user could take it.
#[test]
fn a_state_directory_others_can_enter_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "en-insecure");
    let state = dir.join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_token(&dir, 0o600);
    let flags = flags_in(&dir, closed_port());
    let err = super::enroll::enroll_host(&flags, crate::secret::euid(), 0).unwrap_err();
    assert!(err.contains("state"), "{err}");
    assert!(dir.join("enroll.token").exists());
}
