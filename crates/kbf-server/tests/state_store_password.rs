//! A frontend's start asks for the state store's password before it opens the store
//! (`--state-store-password-file`, `Args::with_state_store_password`).

use std::future::pending;
use std::os::unix::fs::PermissionsExt as _;

use kbf_server::frontend::{self, start_frontend};

/// The flags of `kbf frontend` for a start on loopback ports, and these more.
fn flags(more: &[&str]) -> frontend::FrontendFlags {
    let mut argv = vec![
        "frontend",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--storage=http://127.0.0.1:1",
    ];
    argv.extend_from_slice(more);
    frontend::parse(
        &frontend::command()
            .try_get_matches_from(argv)
            .expect("flags"),
    )
    .expect("parses")
}

/// Catches a frontend that opens its Postgres store before it has the password: a password
/// file others can read stops the start with the password's refusal (a store opened first would
/// fail on its missing socket instead), and a password file beside a SQLite store stops it
/// before the file is made. Both with their whole text, which holds no password.
#[test]
fn a_frontend_start_refuses_a_password_it_cannot_use_before_it_opens_the_store() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "state-store-password");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let pw = dir.join("pw");
    std::fs::write(&pw, "hunter2\n").expect("write");
    std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let file = format!("--state-store-password-file={}", pw.display());
    let socket = dir.join("no-socket");
    let postgres = format!(
        "--state-store=postgres://kbf@/kbf?host={}",
        socket.display()
    );
    let refused = runtime
        .block_on(async {
            start_frontend(
                &flags(&[&postgres, &file, "--state-store-wait-secs=0"]),
                || Ok(pending::<()>()),
            )
        })
        .err()
        .expect("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the state store's password: the password file {}: it has mode 0644; it must be \
             0600 or 0400",
            pw.display()
        )
    );
    let state = dir.join("state.db");
    let sqlite = format!("--state-store=sqlite://{}", state.display());
    let refused = runtime
        .block_on(async { start_frontend(&flags(&[&sqlite, &file]), || Ok(pending::<()>())) })
        .err()
        .expect("refused");
    assert_eq!(
        refused.to_string(),
        "--state-store-password-file is for a postgres:// store, and this one is SQLite"
    );
    assert!(!state.exists(), "the SQLite store is not opened");
}
