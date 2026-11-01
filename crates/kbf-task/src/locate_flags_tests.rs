//! `--locate` through the flags: without it the listener alone is asked; with it `/locate` is
//! asked once and the store the answer names serves the blobs it points at.

use clap::Parser as _;
use kbf_segments::sha256;
use kbf_testkit::dir::TestDir;

use super::*;
use crate::fake_http::{Answer, Fake};

fn cli_for(dir: &TestDir, grant_url: &str, extra: &[&str]) -> Cli {
    let id = digest::from_reapi(&digest::to_reapi(&sha256(b"action"))).expect("digest");
    let work = dir.join("work");
    let id = id.to_string();
    let mut args = vec![
        "kbf-task",
        "--pack-url-file",
        "/nonexistent-url",
        "--grant-url",
        grant_url,
        "--grant-token-file",
        "/nonexistent",
        "--allow-plain-http-grant",
        "--lease",
        "lease-1",
        "--action",
        &id,
        "--work-dir",
        work.to_str().expect("utf-8"),
    ];
    args.extend(extra);
    let mut cli = Cli::try_parse_from(args).expect("flags");
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    std::fs::write(dir.join("url"), "https://store.example/pack?sig=s\n").expect("write");
    std::fs::write(dir.join("root.der"), [1, 2, 3]).expect("write");
    cli.grant_token_file = Some(dir.join("token"));
    cli.pack_url_file = Some(dir.join("url"));
    cli.root_der = vec![dir.join("root.der")];
    cli
}

fn blob() -> Vec<u8> {
    b"the one input blob".to_vec()
}

fn blob_label() -> String {
    let d = sha256(&blob());
    format!("{}/{}", d.hash_hex(), d.size_bytes)
}

/// Catches the flag being on by default, or not parsed.
#[test]
fn the_locate_flag_is_off_unless_given() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-locate-flag");
    assert!(!cli_for(&dir, "http://127.0.0.1:9", &[]).locate);
    assert!(cli_for(&dir, "http://127.0.0.1:9", &["--locate"]).locate);
}

/// Catches `/locate` being asked without the flag: the listener sees one GET for the blob and
/// nothing else, and the source has no report of its own.
#[test]
fn without_the_flag_locate_is_never_asked() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-locate-off");
    let body = blob();
    let listener = Fake::start(move |seen| {
        if seen.path == format!("/{}", blob_label()) {
            Answer::new(200, body.clone())
        } else {
            Answer::new(404, "")
        }
    });
    let cli = cli_for(&dir, &listener.base, &[]);
    let (source, _) = connect_located(&cli).expect("connect");
    let wanted = digest::to_reapi(&sha256(&blob()));
    assert!(source.get(&wanted).expect("get") == blob());
    assert_eq!(listener.paths(), vec![format!("/{}", blob_label())]);
    assert!(source.report().is_none());
}

/// Catches the flag not reaching the source: with it the listener is asked for `/locate` once and
/// never for the blob, the store the answer names is read with the grant the answer carries, and
/// the report counts the read.
#[test]
fn with_the_flag_the_store_serves_what_locate_points_at() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-locate-on");
    let bytes = blob();
    let mut pack = b"HEAD".to_vec();
    pack.extend_from_slice(&bytes);
    let store = Fake::start(move |seen| {
        assert_eq!(seen.path, "/bkt/cas/f/w/p0");
        assert_eq!(seen.header("x-tierstore-grant"), Some("the-grant"));
        Answer::new(206, pack[4..].to_vec())
    });
    let head = format!(
        r#"{{"store":"{}","bucket":"bkt","grants":["the-grant"]}}"#,
        store.base
    );
    let line = format!(
        r#"{{"digest":"{}","pack":{{"key":"cas/f/w/p0","offset":4,"length":{}}},"g":0}}"#,
        blob_label(),
        bytes.len()
    );
    let table = format!("{head}\n{line}");
    let listener = Fake::start(move |seen| {
        if seen.path == "/locate" {
            Answer::new(200, table.clone())
        } else {
            Answer::new(404, "")
        }
    });
    let cli = cli_for(&dir, &listener.base, &["--locate"]);
    let (source, _) = connect_located(&cli).expect("connect");
    let wanted = digest::to_reapi(&sha256(&bytes));
    assert!(source.get(&wanted).expect("get") == bytes);
    assert_eq!(listener.paths(), vec!["/locate".to_owned()]);
    assert_eq!(store.requests().len(), 1);
    let report = source.report().expect("report");
    assert!(report.contains("1 by range"), "{report}");
    assert!(!report.contains("the-grant"), "{report}");
}

/// Catches a bad root file being ignored under the flag: the setup failure of `connect` is the
/// same (exit 14).
#[test]
fn an_unusable_root_file_is_still_a_setup_failure_with_the_flag() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-locate-roots");
    let mut cli = cli_for(&dir, "http://127.0.0.1:9", &["--locate"]);
    cli.root_der = vec![dir.join("missing.der")];
    let failure = connect_located(&cli).err().expect("refused");
    assert_eq!(failure.exit_code(), EXIT_SETUP);
}
