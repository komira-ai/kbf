//! The daemon's exit status for a start it refuses, and what its flags ask of the host.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;

use super::{Cli, ContainerNeeds, Error, HostRefused, ImageUnusable, exit_code};

fn code(e: Error) -> String {
    format!("{:?}", exit_code(&e))
}

/// Catches a start the host refuses that exits 1: the units' `RestartPreventExitStatus=2`
/// leaves a daemon that exits 2 down, and one that exits 1 restarts every `RestartSec`
/// forever on a host that will not change by itself. Each of the four refusals the
/// container driver makes of its host exits 2, and a start that failed some other way
/// still exits 1.
#[test]
fn a_start_the_host_refuses_exits_2_and_any_other_failure_exits_1() {
    let two = format!("{:?}", ExitCode::from(2));
    let one = format!("{:?}", ExitCode::FAILURE);
    // The container driver is Linux's: macOS has none of these errors.
    #[cfg(target_os = "linux")]
    {
        use kbf_driver_container::{DelegateError, IdFiles, OciError, check_daemon_user};
        // No subordinate ids to read: the daemon user's range is missing.
        let subids = check_daemon_user(&IdFiles::in_dir(Path::new("/nonexistent/ids")))
            .expect_err("no id files");
        assert_eq!(code(Box::new(subids)), two, "subuid/subgid");
        assert_eq!(
            code(Box::new(DelegateError::RootCgroup)),
            two,
            "cgroup delegation"
        );
        assert_eq!(
            code(Box::new(OciError::NoRuntime(PathBuf::from(
                "/usr/bin/crun"
            )))),
            two,
            "crun"
        );
    }
    assert_eq!(
        code(Box::new(HostRefused(
            "read /proc/self/cgroup: gone".to_owned()
        ))),
        two,
        "an unreadable own cgroup"
    );
    let image: Error = Box::new(ImageUnusable("x".to_owned()));
    assert_eq!(code(image), two, "image");
    assert_eq!(code("the data directory cannot be made".into()), one);
}

fn cli(extra: &[&str]) -> Cli {
    let base = [
        "kbf-daemon",
        "--server=https://front:7070",
        "--ca-cert=/c/ca.pem",
        "--cert=/c/node.pem",
        "--key=/c/node.key",
        "--node-id=n1",
    ];
    Cli::try_parse_from(base.iter().chain(extra)).expect("flags")
}

/// Catches the host needs reading the wrong flag: the files, the node id, the data
/// directory (the default when not given) and, for the container driver alone, the
/// OCI runtime.
#[test]
fn the_flags_say_what_the_daemon_asks_of_its_host() {
    let n = cli(&["--driver=fake"]).host_needs();
    assert_eq!(n.ca_cert, Path::new("/c/ca.pem"));
    assert_eq!(n.cert, Path::new("/c/node.pem"));
    assert_eq!(n.key, Path::new("/c/node.key"));
    assert_eq!(n.node_id, "n1");
    assert_eq!(n.data_dir, Path::new(super::DEFAULT_DATA_DIR));
    assert_eq!(n.container, None);

    let n = cli(&["--driver=native", "--data-dir=/d"]).host_needs();
    assert_eq!((n.container, n.data_dir.as_path()), (None, Path::new("/d")));

    let n = cli(&["--driver=container"]).host_needs();
    assert_eq!(
        n.container,
        Some(ContainerNeeds {
            oci_runtime: PathBuf::from("/usr/bin/crun")
        })
    );

    let n = cli(&["--driver=container", "--oci-runtime=/opt/crun"]).host_needs();
    assert_eq!(
        n.container,
        Some(ContainerNeeds {
            oci_runtime: PathBuf::from("/opt/crun")
        })
    );
}
