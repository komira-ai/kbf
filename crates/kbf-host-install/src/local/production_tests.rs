//! The process around the socket: exit codes of the flags, the private state directory, and the
//! serving of a backend on a socket until it is told to stop.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use super::core_tests::{A, Fake, Fx};
use super::production::{Place, main_with, private_dir, serve_with, unix_now};
use super::serve::ServeConfig;
use crate::trust::Trust;

fn trust(dir: &Path) -> Trust {
    Trust::owned_by(fs::metadata(dir).unwrap().uid(), dir)
}

/// Bad flags are exit 2 with the usage, and the serving is not started; flags that parse start
/// it, and its failure is exit 1 while its end is exit 0.
#[test]
fn the_exit_status_says_flags_start_and_end() {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let started = std::cell::Cell::new(false);
    let code = main_with(&words(&["--socket"]), |_| {
        started.set(true);
        Ok(())
    });
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(2))
    );
    assert!(!started.get());

    let good = words(&["--socket", "/run/s", "--socket-user", "u"]);
    let code = main_with(&good, |config: &ServeConfig| {
        assert_eq!(config.user, "u");
        Ok(())
    });
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::SUCCESS)
    );
    let code = main_with(&good, |_| Err("no socket".to_owned()));
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(1))
    );
}

/// The clock is the wall clock in Unix seconds.
#[test]
fn the_clock_is_unix_seconds() {
    assert!(unix_now() > 1_700_000_000);
}

/// The state directory is made private (0700) when it is not there, kept when it is, and refused
/// when it is not the trusted user's or others can write above it.
#[test]
fn the_state_directory_is_private() {
    let outer = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "pr-private");
    fs::set_permissions(&*outer, fs::Permissions::from_mode(0o755)).unwrap();
    let dir = outer.join("state");
    private_dir(&trust(&outer), &dir).unwrap();
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o7777, 0o700);
    fs::write(dir.join("kept"), "x").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    private_dir(&trust(&outer), &dir).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().mode() & 0o7777,
        0o700,
        "made private again"
    );
    assert!(dir.join("kept").exists());

    // Not a directory, and not under a directory it may live in.
    assert!(private_dir(&trust(&outer), &dir.join("kept")).is_err());
    assert!(private_dir(&trust(&outer), &outer.join("no/such/dir")).is_err());
    fs::set_permissions(&*outer, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(private_dir(&trust(&outer), &dir).is_err());
    fs::set_permissions(&*outer, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The process serves a backend on the socket named by the flags, as the account given, writing
/// its state under the state directory, until told to stop; a socket that cannot be made is an
/// error with the path in it.
#[test]
fn a_backend_is_served_on_the_socket_until_told_to_stop() {
    let fx = Fx::new("pr-serve");
    let run = fx.dir.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = run.join("host.sock");
    let config = ServeConfig::parse(&[
        "--socket".to_owned(),
        socket.display().to_string(),
        "--socket-user".to_owned(),
        "u".to_owned(),
    ])
    .unwrap();
    let meta = fs::metadata(&*fx.dir).unwrap();
    let trust = trust(&fx.dir);
    let audit = fx.dir.join("audit.log");
    let place = Place {
        state_dir: &fx.dir,
        trust: &trust,
        ids: (meta.uid(), meta.gid()),
        audit: &audit,
    };
    let backend = Fake {
        world: std::rc::Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let client = {
        let (stop, socket) = (Arc::clone(&stop), socket.clone());
        std::thread::spawn(move || {
            // The server binds the socket meanwhile.
            let mut stream = loop {
                match UnixStream::connect(&socket) {
                    Ok(stream) => break stream,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            };
            stream.write_all(b"query\n").unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            stop.store(true, Ordering::Relaxed);
            text
        })
    };
    serve_with(&config, backend, &place, &stop).unwrap();
    let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
    assert_eq!(answer["commit"], A);
    assert_eq!(answer["node_id"], "n1");

    let nowhere = ServeConfig::parse(&[
        "--socket".to_owned(),
        run.join("gone/host.sock").display().to_string(),
        "--socket-user".to_owned(),
        "u".to_owned(),
    ])
    .unwrap();
    let backend = Fake {
        world: std::rc::Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let err = serve_with(&nowhere, backend, &place, &AtomicBool::new(true)).unwrap_err();
    assert!(err.contains("gone/host.sock"), "{err}");
}
