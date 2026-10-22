//! The `kbf.toml` that `kbf-installer setup` writes for a node, read by `kbf` itself: the
//! node's update client is gone, but a host set up before that still carries the two keys,
//! and `kbf` must keep accepting them (`kbf-node`'s retired flags). The installer's side
//! (which modes get the keys, their values, the Keep rules) is its own tests'.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use kbf_cli::Invocation;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::setup::kbf_toml::{Ours, UpdateDirs};
use kbf_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("setup-update-{name}"))
}

/// The file `setup` writes for a `daemon` host with these directories (`None`: the file of a
/// setup that wrote none).
fn rendered(dirs: Option<UpdateDirs<'_>>) -> String {
    Ours {
        mode: "daemon",
        frontend_url: Some("https://farm.example:8443"),
        token_file: Some("/etc/kbf/enroll.token"),
        api_token_file: None,
        state_store: None,
        update: dirs,
        extra: &[],
    }
    .render()
}

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

/// Catches a file whose keys `kbf` refuses: the values `setup` writes for a Linux and a Mac
/// node are still taken as the daemon's retired `--update-dir` and `--orders-dir` (the Debug
/// of the parsed flags names them).
///
/// Not run as a mutant: the key written with an underscore, which `kbf` refuses.
#[test]
fn the_layouts_directories_reach_the_daemons_flags() {
    for platform in [Platform::Linux, Platform::MacOs] {
        let layout = Layout::new(kbf_host::Root::new("/").expect("root"), platform);
        let (update, orders) = (
            layout.update_dir().to_string(),
            layout.orders_dir().to_string(),
        );
        let dir = scratch(&format!("flags-{}", platform.name()));
        let cli = daemon(
            dir.path(),
            &rendered(Some(UpdateDirs {
                update_dir: &update,
                orders_dir: &orders,
            })),
        );
        let shown = format!("{cli:?}");
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
