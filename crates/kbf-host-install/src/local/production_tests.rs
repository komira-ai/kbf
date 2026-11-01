//! The process around the socket: exit codes of the flags, the private state directory, and the
//! serving of a backend on a socket until it is told to stop, on each platform.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_testkit::dir::TestDir;
use serde_json::Value;

use super::core_tests::{A, Fake, Fx};
use super::production::{
    LinuxEnv, Place, bound_container, log, main_with, private_dir, run, serve_linux,
    serve_linux_with, serve_mac, serve_mac_with, serve_with, unix_now,
};
use super::serve::ServeConfig;
use crate::mac::tests::fx::{Fx as MacFx, Sim, mkdir};
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

fn trust(dir: &Path) -> Trust {
    Trust::owned_by(fs::metadata(dir).unwrap().uid(), dir)
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| (*s).to_owned()).collect()
}

fn config(socket: &Path, user: &str) -> ServeConfig {
    ServeConfig::parse(&words(&[
        "--socket",
        &socket.display().to_string(),
        "--socket-user",
        user,
    ]))
    .unwrap()
}

/// The same for a Linux host, which needs a unit to restart.
fn linux_config(socket: &Path, user: &str) -> ServeConfig {
    ServeConfig::parse(&words(&[
        "--socket",
        &socket.display().to_string(),
        "--socket-user",
        user,
        "--restart-unit",
        "kbf-daemon.service",
    ]))
    .unwrap()
}

/// A directory the trust of a test reaches, with a `run` directory in it for the socket.
fn scratch(name: &str) -> (TestDir, std::path::PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let run = dir.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    (dir, run)
}

/// Query through `socket` once it is there, then stop the serving.
fn ask_then_stop(
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<String> {
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
}

/// Bad flags are exit 2 with the usage, and the serving is not started; flags that parse start
/// it, and its failure is exit 1 while its end is exit 0.
#[test]
fn the_exit_status_says_flags_start_and_end() {
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
    log("a line for the journal");
}

/// The clock is the wall clock in Unix seconds.
#[test]
fn the_clock_is_unix_seconds() {
    assert!(unix_now() > 1_700_000_000);
}

/// A Linux host needs a unit to restart, a Mac takes none; both say so before anything is
/// bound or written, and the program runs the side of the platform it is built for.
#[test]
fn each_platform_names_what_it_needs_before_it_touches_anything() {
    let no_units = config(Path::new("/run/kbf/s"), "u");
    assert!(
        serve_linux(&no_units)
            .unwrap_err()
            .contains("--restart-unit is required")
    );
    let with_unit = ServeConfig::parse(&words(&[
        "--socket",
        "/run/kbf/s",
        "--socket-user",
        "u",
        "--restart-unit",
        "kbf-daemon.service",
    ]))
    .unwrap();
    assert!(
        serve_mac(&with_unit)
            .unwrap_err()
            .contains("a Mac restarts the daemon's job")
    );
    if cfg!(target_os = "linux") {
        assert!(
            run(&no_units)
                .unwrap_err()
                .contains("--restart-unit is required")
        );
    }
}

/// The state directory is made private (0700) when it is not there, kept when it is, and refused
/// when it is not the trusted user's or others can write above it.
#[test]
fn the_state_directory_is_private() {
    let outer = TestDir::under(std::env::temp_dir(), "pr-private");
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
    let meta = fs::metadata(&*fx.dir).unwrap();
    let trust = trust(&fx.dir);
    let audit = fx.dir.join("audit.log");
    let place = Place {
        state_dir: &fx.dir,
        state_name: "state.json",
        trust: &trust,
        ids: (meta.uid(), meta.gid()),
        audit: &audit,
    };
    let backend = Fake {
        world: Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let client = ask_then_stop(socket.clone(), Arc::clone(&stop));
    serve_with(&config(&socket, "u"), backend, &place, &stop).unwrap();
    let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
    assert_eq!(answer["commit"], A);
    assert_eq!(answer["node_id"], "n1");

    let nowhere = config(&run.join("gone/host.sock"), "u");
    let backend = Fake {
        world: Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let err = serve_with(&nowhere, backend, &place, &AtomicBool::new(true)).unwrap_err();
    assert!(err.contains("gone/host.sock"), "{err}");
}

/// A Linux host: the account of the socket is looked up in the passwd file and is never root's,
/// the state directory is made, and the backend is served; an account that is not there, a
/// passwd file that cannot be read and a state directory that cannot be made are each an error
/// that says which.
#[test]
fn a_linux_host_looks_up_the_account_and_makes_its_state_directory() {
    let fx = Fx::new("pr-linux");
    let (dir, run) = scratch("pr-linux-env");
    let meta = fs::metadata(&*dir).unwrap();
    let passwd = dir.join("passwd");
    fs::write(
        &passwd,
        format!(
            "root:x:0:0::/root:/bin/sh\nkbf-updater:x:{}:{}::/:/bin/false\n",
            meta.uid(),
            meta.gid()
        ),
    )
    .unwrap();
    fs::set_permissions(&passwd, fs::Permissions::from_mode(0o644)).unwrap();
    let env = LinuxEnv {
        passwd,
        state_dir: dir.join("state"),
        state_name: "state.json".to_owned(),
        audit: dir.join("audit.log"),
    };
    let trust = trust(&dir);
    let backend = || Fake {
        world: Rc::clone(&fx.world),
        lock: dir.join("lock"),
    };
    let socket = run.join("host.sock");
    let stop = Arc::new(AtomicBool::new(false));
    let client = ask_then_stop(socket.clone(), Arc::clone(&stop));
    serve_linux_with(
        &linux_config(&socket, "kbf-updater"),
        &env,
        &trust,
        backend(),
        &stop,
    )
    .unwrap();
    client.join().unwrap();
    assert_eq!(fs::metadata(&env.state_dir).unwrap().mode() & 0o7777, 0o700);

    let stopped = AtomicBool::new(true);
    for (user, why) in [
        ("nobody", "has no usable ids"),
        ("root", "has no usable ids"),
    ] {
        let err = serve_linux_with(
            &linux_config(&socket, user),
            &env,
            &trust,
            backend(),
            &stopped,
        )
        .unwrap_err();
        assert!(err.contains(why), "{user}: {err}");
    }
    let missing = LinuxEnv {
        passwd: dir.join("no-passwd"),
        state_dir: dir.join("state"),
        state_name: "state.json".to_owned(),
        audit: dir.join("audit.log"),
    };
    let err = serve_linux_with(
        &linux_config(&socket, "kbf-updater"),
        &missing,
        &trust,
        backend(),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("no-passwd"), "{err}");
    let blocked = LinuxEnv {
        passwd: env.passwd.clone(),
        state_dir: dir.join("passwd/state"),
        state_name: "state.json".to_owned(),
        audit: dir.join("audit.log"),
    };
    let err = serve_linux_with(
        &linux_config(&socket, "kbf-updater"),
        &blocked,
        &trust,
        backend(),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("passwd/state"), "{err}");
    // Without a unit there is nothing to restart: said before the account is looked up.
    let err = serve_linux_with(
        &config(&socket, "kbf-updater"),
        &env,
        &trust,
        backend(),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("--restart-unit is required"), "{err}");
    // A host that serves the frontend container needs no unit: it is on to the account.
    let container = ServeConfig::parse(&words(&[
        "--socket",
        &socket.display().to_string(),
        "--socket-user",
        "nobody-here",
        "--frontend-container",
    ]))
    .unwrap();
    let err = serve_linux_with(&container, &env, &trust, backend(), &stopped).unwrap_err();
    assert!(err.contains("nobody-here has no usable ids"), "{err}");
    // The program's own start serves the container the same way, up to the account.
    if cfg!(target_os = "linux") {
        let err = serve_linux(&container).unwrap_err();
        assert!(err.contains("nobody-here has no usable ids"), "{err}");
    }
    // And a Mac has no container to serve.
    let err = serve_mac(&container).unwrap_err();
    assert!(err.contains("--frontend-container"), "{err}");
    let production = LinuxEnv::production();
    assert_eq!(production.passwd, Path::new("/etc/passwd"));
    assert_eq!(production.state_dir, Path::new("/var/lib/kbf-host-install"));
}

/// A Runner for a tool that cannot be started.
struct Broken;

impl Runner for Broken {
    fn run(&self, _cmd: &Cmd) -> std::io::Result<Exec> {
        Err(std::io::Error::other("cannot start"))
    }
}

/// A Mac: the account of the socket is asked of the directory service, the work directory is
/// made, and the kit is served; a directory service that is not there, an account without
/// usable ids and a work directory that cannot be made are each an error that says which.
#[test]
fn a_mac_asks_the_directory_service_and_makes_its_work_directory() {
    let mac = MacFx::new("pr-mac");
    let sim = Rc::new(mac.sim());
    let run = mac.dir.join("run");
    mkdir(&run, 0o755);
    let socket = run.join("host.sock");
    let stop = Arc::new(AtomicBool::new(false));
    let client = ask_then_stop(socket.clone(), Arc::clone(&stop));
    serve_mac_with(
        &config(&socket, "_kbf"),
        mac.host.clone(),
        &*sim,
        Box::new(Shared(Rc::clone(&sim))),
        &stop,
    )
    .unwrap();
    let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
    assert_eq!(answer["node_id"], "mac-a");

    let stopped = AtomicBool::new(true);
    let err = serve_mac_with(
        &config(&socket, "_kbf"),
        mac.host.clone(),
        &Broken,
        Box::new(Broken),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("dscl: cannot start"), "{err}");
    sim.nobody_ids.set(Some((0, 0)));
    let err = serve_mac_with(
        &config(&socket, "nobody"),
        mac.host.clone(),
        &*sim,
        Box::new(Shared(Rc::clone(&sim))),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("has no usable ids"), "{err}");
    // A work directory that is a file.
    let mut blocked = mac.host.clone();
    blocked.work = run.join("work-file");
    fs::write(&blocked.work, "a file").unwrap();
    let err = serve_mac_with(
        &config(&socket, "_kbf"),
        blocked,
        &*sim,
        Box::new(Shared(Rc::clone(&sim))),
        &stopped,
    )
    .unwrap_err();
    assert!(err.contains("work directory"), "{err}");
}

/// A runner that is the scripted Mac, kept by the test.
struct Shared(Rc<Sim>);

impl Runner for Shared {
    fn run(&self, cmd: &Cmd) -> std::io::Result<Exec> {
        self.0.run(cmd)
    }
}

/// Each layout reaches its own backend before anything is bound or written (an account this
/// host does not have stops all three at the same place), and a Mac, which keeps its releases
/// in the kit, refuses a package layout.
#[test]
fn each_layout_reaches_its_backend_and_a_mac_takes_only_its_own() {
    for layout in ["installer", "deb", "rpm"] {
        let config = ServeConfig::parse(&words(&[
            "--socket",
            "/run/kbf/s",
            "--socket-user",
            "no-such-account-kbf",
            "--layout",
            layout,
            "--restart-unit",
            "kbf-daemon.service",
        ]))
        .unwrap();
        if cfg!(target_os = "linux") {
            assert!(serve_linux(&config).is_err(), "{layout}");
        }
        let mac = serve_mac(&config).unwrap_err();
        if layout == "installer" {
            assert!(mac.contains("a Mac restarts the daemon's job"), "{mac}");
        } else {
            assert!(mac.contains("--layout is for a Linux host"), "{mac}");
        }
    }
}

/// A helper started with the default file is bound to no container (its files keep their
/// names); with another file it is bound to the container that file names, read through the
/// trust, and a file that cannot be read stops the start naming the file.
#[test]
fn a_helper_with_its_own_file_is_bound_to_the_container_it_names() {
    let (dir, _run) = scratch("hi-bound");
    let file = dir.join("fe-b.toml");
    fs::write(
        &file,
        "[frontend]\nname = \"kbf-fe-b\"\nrestart = \"always\"\napi = \"127.0.0.1:28982\"\ndrain_token_file = \"/t\"\n",
    )
    .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let flags = |extra: &[&str]| {
        let mut all = vec![
            "--socket",
            "/s",
            "--socket-user",
            "u",
            "--frontend-container",
        ];
        all.extend(extra);
        ServeConfig::parse(&words(&all)).unwrap()
    };
    let t = trust(&dir);
    assert_eq!(bound_container(&t, &flags(&[])), Ok(None));
    let own = flags(&["--frontend-config", &file.display().to_string()]);
    assert_eq!(bound_container(&t, &own), Ok(Some("kbf-fe-b".to_owned())));
    let missing = flags(&[
        "--frontend-config",
        &dir.join("nope.toml").display().to_string(),
    ]);
    // A file older programs accepted (drain_secs = 0) still lets the program start and name its
    // container; the switch is what refuses it.
    let zero = dir.join("fe-zero.toml");
    fs::write(
        &zero,
        "[frontend]\nname = \"kbf-fe-z\"\nrestart = \"always\"\napi = \"127.0.0.1:28983\"\ndrain_token_file = \"/t\"\ndrain_secs = 0\n",
    )
    .unwrap();
    fs::set_permissions(&zero, fs::Permissions::from_mode(0o644)).unwrap();
    let zeroed = flags(&["--frontend-config", &zero.display().to_string()]);
    assert_eq!(
        bound_container(&t, &zeroed),
        Ok(Some("kbf-fe-z".to_owned()))
    );
    assert!(
        bound_container(&t, &missing)
            .unwrap_err()
            .contains("nope.toml")
    );
    if cfg!(target_os = "linux") {
        // The program's own start reads the file as root: this one is not, so it stops there.
        let err = serve_linux(&missing).unwrap_err();
        assert!(err.contains("nope.toml"), "{err}");
    }
}

/// Catches a served socket that prunes with another count than `--keep-releases` names: the
/// confirm a client sends through the socket asks the backend to keep exactly that many.
#[test]
fn the_served_confirm_prunes_with_the_count_of_the_flag() {
    let fx = Fx::new("pr-serve-keep");
    fx.world.borrow_mut().retain_file = Some(fx.dir.join("retain.json"));
    // A release on trial, through the same state file the served socket reads.
    fx.install(super::core_tests::VB);
    fx.ask(&format!("switch kbf={}", super::core_tests::VB));
    // The served loop's tick reads the real clock: a trial with no deadline is not reverted.
    if let Some(pending) = fx.world.borrow_mut().pending.as_mut() {
        pending.deadline_unix = None;
    }
    let run = fx.dir.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = run.join("host.sock");
    let meta = fs::metadata(&*fx.dir).unwrap();
    let trust = trust(&fx.dir);
    let audit = fx.dir.join("audit.log");
    let place = Place {
        state_dir: &fx.dir,
        state_name: "state.json",
        trust: &trust,
        ids: (meta.uid(), meta.gid()),
        audit: &audit,
    };
    let backend = Fake {
        world: Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let mut config = config(&socket, "u");
    config.keep_releases = 3;
    let stop = Arc::new(AtomicBool::new(false));
    let line = format!("confirm kbf={}\n", super::core_tests::VB);
    let (asked, halt) = (socket.clone(), Arc::clone(&stop));
    let client = std::thread::spawn(move || {
        let mut stream = loop {
            match UnixStream::connect(&asked) {
                Ok(stream) => break stream,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        stream.write_all(line.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        halt.store(true, Ordering::Relaxed);
        text
    });
    serve_with(&config, backend, &place, &stop).unwrap();
    let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
    assert_eq!(answer["result"], "confirmed", "{answer}");
    let prunes: Vec<String> = fx
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("prune"))
        .collect();
    assert_eq!(prunes, ["prune 3"]);
}
