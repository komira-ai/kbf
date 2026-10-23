//! A `kbf.toml` written before the node's update client was removed still has `[daemon]
//! update-dir` and `orders-dir` (on Linux the installer still writes them, and Macs
//! provisioned earlier carry them). `kbf daemon` must keep accepting both keys from its
//! `--config` file: they are the retired `--update-dir` and `--orders-dir` flags.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use kbf_cli::Invocation;
use kbf_testkit::dir::TestDir;

/// `kbf daemon` on `text` as its `--config`, with the daemon's required flags beside it.
fn daemon(dir: &Path, text: &str) -> Box<kbf_node::Cli> {
    let file = dir.join("kbf.toml");
    std::fs::write(&file, text).expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let mut args: Vec<OsString> = [
        "kbf",
        "daemon",
        "--server=https://front:7070",
        "--node-id=n1",
        "--driver=fake",
    ]
    .map(OsString::from)
    .into();
    for (flag, name) in [
        ("ca-cert", "ca.pem"),
        ("cert", "node.pem"),
        ("key", "node.key"),
    ] {
        args.push(format!("--{flag}={}", dir.join(name).display()).into());
    }
    args.push("--config".into());
    args.push(file.into());
    match kbf_cli::parse_with(args, None).expect("parses").invocation {
        Invocation::Daemon(cli) => cli,
        other => panic!("not the daemon: {other:?}"),
    }
}

/// The two keys, in the values a Linux and a Mac host carry, reach the daemon's retired flags.
#[test]
fn the_retired_update_keys_in_a_config_file_are_still_accepted() {
    for (name, update, orders) in [
        (
            "linux",
            "/var/lib/kbf-installer",
            "/var/lib/kbf-installer/orders",
        ),
        (
            "mac",
            "/var/db/kbf-installer",
            "/var/db/kbf-installer/orders",
        ),
    ] {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("retired-keys-{name}"));
        let text = format!(
            "mode = \"daemon\"\n\n[daemon]\nupdate-dir = \"{update}\"\norders-dir = \"{orders}\"\n"
        );
        let shown = format!("{:?}", daemon(dir.path(), &text));
        assert!(
            shown.contains(&format!("update_dir: Some(\"{update}\")")),
            "{shown}"
        );
        assert!(
            shown.contains(&format!("orders_dir: Some(\"{orders}\")")),
            "{shown}"
        );
    }
}
