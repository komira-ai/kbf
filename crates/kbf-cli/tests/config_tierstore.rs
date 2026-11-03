//! The `--tierstore-*` settings of `kbf daemon` read from `kbf.toml`'s `[packs]` table: what a
//! file stands for, what the command line overrides, and what is refused. The starts that refuse
//! the settings (`kbf storage-and-daemon`, `kbf all-in-one`) are kbf-node's tests.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use kbf_cli::Invocation;
use kbf_testkit::dir::TestDir;

/// A fresh, empty scratch directory under the workspace's `target/`, removed when it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("config-tierstore-{name}"),
    )
}

/// A `kbf.toml` in `dir` holding `text`, readable by its owner alone.
fn file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("kbf.toml");
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

/// `kbf daemon` with its identity and driver on the command line, these more words, and the
/// file at `config`.
fn daemon(more: &[&str], config: &Path) -> Result<Invocation, clap::Error> {
    let mut argv: Vec<OsString> = [
        "kbf",
        "daemon",
        "--server=https://front:7070",
        "--ca-cert=ca.pem",
        "--cert=c.pem",
        "--key=c.key",
        "--node-id=mac-1",
        "--driver=native",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    argv.extend(more.iter().map(OsString::from));
    argv.push("--config".into());
    argv.push(config.into());
    kbf_cli::parse_with(argv, None).map(|parsed| parsed.invocation)
}

/// The daemon's settings as `Debug` spells them, from which the three tierstore fields are read.
fn settings(invocation: &Invocation) -> String {
    match invocation {
        Invocation::Daemon(cli) => format!("{cli:?}"),
        other => panic!("not the daemon: {other:?}"),
    }
}

/// The three settings, in a `[packs]` table.
const PACKS: &str = "\
[packs]
tierstore-endpoint = \"http://127.0.0.1:9300\"
tierstore-bucket = \"tierstore\"
tierstore-credentials-file = \"/usr/local/etc/kbf/tierstore.json\"
";

/// Asserts that the three settings given in `[packs]` alone reach the daemon as given.
/// Catches a key of the table that does not become its flag.
#[test]
fn the_settings_come_from_the_packs_table_alone() {
    let dir = scratch("alone");
    let path = file(&dir, PACKS);
    let said = settings(&daemon(&[], &path).expect("parses"));
    for field in [
        "tierstore_endpoint: Some(\"http://127.0.0.1:9300\")",
        "tierstore_bucket: Some(\"tierstore\")",
        "tierstore_credentials_file: Some(\"/usr/local/etc/kbf/tierstore.json\")",
    ] {
        assert!(said.contains(field), "{field} in {said}");
    }
}

/// Asserts that a setting on the command line wins over its key, and that the keys it does not
/// give still come from the file.
#[test]
fn the_command_line_overrides_a_key() {
    let dir = scratch("override");
    let path = file(&dir, PACKS);
    let parsed = daemon(&["--tierstore-endpoint=http://[::1]:9301"], &path).expect("parses");
    let said = settings(&parsed);
    assert!(
        said.contains("tierstore_endpoint: Some(\"http://[::1]:9301\")"),
        "{said}"
    );
    assert!(
        said.contains("tierstore_bucket: Some(\"tierstore\")"),
        "{said}"
    );
}

/// Asserts that a file that gives the endpoint without the bucket or the credentials file, or
/// either of those without the endpoint, is refused naming what is missing.
/// Catches a daemon that starts half-configured from its file.
#[test]
fn a_file_without_a_companion_is_refused_naming_it() {
    let dir = scratch("companion");
    let cases = [
        (
            "[packs]\ntierstore-endpoint = \"http://127.0.0.1:9300\"\ntierstore-bucket = \"t-1\"\n",
            "--tierstore-credentials-file",
        ),
        (
            "[packs]\ntierstore-endpoint = \"http://127.0.0.1:9300\"\ntierstore-credentials-file = \"/k.json\"\n",
            "--tierstore-bucket",
        ),
        (
            "[packs]\ntierstore-bucket = \"t-1\"\n",
            "--tierstore-endpoint",
        ),
        (
            "[packs]\ntierstore-credentials-file = \"/k.json\"\n",
            "--tierstore-endpoint",
        ),
    ];
    for (text, missing) in cases {
        let path = file(&dir, text);
        let refused = daemon(&[], &path).expect_err("refused");
        assert_eq!(
            refused.kind(),
            ErrorKind::MissingRequiredArgument,
            "{refused}"
        );
        assert!(
            refused.to_string().contains(missing),
            "{missing}: {refused}"
        );
    }
}

/// Asserts that a file turning on `pack-read` beside the tierstore is refused as a conflict naming
/// both, and that a template's `pack-read = false` beside it is not.
#[test]
fn pack_read_and_the_tierstore_are_refused_together() {
    let dir = scratch("conflict");
    let both = format!(
        "{PACKS}pack-read = true\npack-store-endpoint = \"http://127.0.0.1:9\"\npack-store-bucket = \"packs\"\n\
         pack-store-credentials-file = \"/k.json\"\n"
    );
    let refused = daemon(&[], &file(&dir, &both)).expect_err("refused");
    assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{refused}");
    let said = refused.to_string();
    assert!(
        said.contains("--tierstore-endpoint") && said.contains("--pack-read"),
        "{said}"
    );
    let off = format!("{PACKS}pack-read = false\n");
    assert!(daemon(&[], &file(&dir, &off)).is_ok());
}
