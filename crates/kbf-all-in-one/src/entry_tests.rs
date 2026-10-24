//! The crate's entry point called as a library, with the flags `kbf` would parse for it, so that
//! the start and the exit status are covered whatever `kbf` itself does with the mode.

use std::process::ExitCode;

use crate::cli::{command, parse};

/// The flags the daemon half requires, and a server half whose cold store cannot be made and
/// whose metadata directory is not given.
const REFUSED: &[&str] = &[
    "kbf-all-in-one",
    "--driver=container",
    "--server=https://127.0.0.1:1",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=n",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--cold-store=file:///proc/nonexistent/cold",
];

/// Asserts that `run` on parsed flags starts the server half, which refuses a start it cannot
/// make, and that the process then exits 2 as `kbf-server` does, before the daemon half starts:
/// the parse-and-start path and `exit`'s failure arm.
#[test]
fn a_start_the_server_half_refuses_exits_2() {
    let matches = command()
        .try_get_matches_from(REFUSED)
        .expect("the flags parse");
    let flags = parse(&matches).expect("flags");
    assert_eq!(
        format!("{:?}", crate::run(&flags)),
        format!("{:?}", ExitCode::from(2))
    );
}

/// Asserts that a run that ended well exits 0, and says nothing.
#[test]
fn a_run_that_ended_well_exits_0() {
    assert_eq!(
        format!("{:?}", crate::run::exit("kbf-all-in-one", "server", Ok(()))),
        format!("{:?}", ExitCode::SUCCESS)
    );
}
