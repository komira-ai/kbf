//! A `kbf.toml` written before the handover between two frontends of one host was removed
//! still has its `[handover]` keys. `kbf frontend` must keep accepting them from its
//! `--config` file: they are the retired flags of `kbf_server::retired`, kept for one release.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;

use kbf_cli::Invocation;
use kbf_testkit::dir::TestDir;

/// A frontend's settings file as it was written before the removal.
const OLD_FILE: &str = concat!(
    "mode = \"frontend\"\n\n",
    "[handover]\n",
    "handover-socket = \"/run/x.sock\"\n",
    "instance = \"a\"\n",
    "envoy-check-interval-ms = 1000\n",
    "await-takeover = true\n",
);

/// Asserts that a `kbf.toml` with `[handover]` keys still loads for a frontend: the file
/// parses, and each of its four keys reaches the retired flags, so each one is warned of.
#[test]
fn the_retired_handover_keys_in_a_config_file_are_still_accepted() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "retired-handover-keys");
    let file = dir.path().join("kbf.toml");
    std::fs::write(&file, OLD_FILE).expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let mut args: Vec<OsString> = [
        "kbf",
        "frontend",
        "--storage=http://127.0.0.1:19000",
        "--state-store=sqlite:///x.db",
        "--config",
    ]
    .map(OsString::from)
    .into();
    args.push(file.into());
    match kbf_cli::parse_with(args, None).expect("parses").invocation {
        Invocation::Frontend(flags) => assert_eq!(flags.retired.warnings().len(), 4),
        other => panic!("not the frontend: {other:?}"),
    }
}
